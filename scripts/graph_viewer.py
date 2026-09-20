#!/usr/bin/env python3
"""图谱可视化网页服务器 —— 浏览器查看 Kùzu 图谱数据。

数据路径：本服务器 → MCP HTTP 端点(graph_query 等只读工具) → Kùzu。
本进程绝不直接打开 graph.kuzu（Kùzu 单进程独占约束），只读且经 MCP 白名单校验。

用法：
    /usr/bin/python3 scripts/graph_viewer.py            # http://127.0.0.1:8899
    /usr/bin/python3 scripts/graph_viewer.py --port 9000 --mcp http://127.0.0.1:8888/mcp

零第三方依赖（Python 标准库 + 原生 JS 力导向图，无 CDN）。
"""

import argparse
import json
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MCP_URL = "http://127.0.0.1:8888/mcp"

# 与 graph.rs 白名单一致（展示用元数据）
LABELS = {
    "Hospital":  "#FF8FB1", "System": "#35B37E", "Interface": "#E8A33D",
    "FaultCase": "#E85D5D", "Company": "#9B6DE8", "Region": "#3DC9C9",
    "Project":   "#4C8DFF", "Family": "#F2C14E", "MemoryRef": "#98A2B3",
}
# (relation, src_label, dst_label, src_key, dst_key)
RELATIONS = [
    ("DEPLOY", "Hospital", "System", "name", "name"),
    ("CONNECT", "System", "Interface", "name", "name"),
    ("FAULT_CASE", "Interface", "FaultCase", "name", "title"),
    ("LOCATED_IN", "Hospital", "Region", "name", "name"),
    ("OWNED_BY", "System", "Company", "name", "name"),
    ("HAS_PROJECT", "Hospital", "Project", "name", "name"),
    ("HOSPITAL_MEMORY", "Hospital", "MemoryRef", "name", "memory_id"),
    ("SYSTEM_MEMORY", "System", "MemoryRef", "name", "memory_id"),
    ("INTERFACE_MEMORY", "Interface", "MemoryRef", "name", "memory_id"),
    ("FAULT_MEMORY", "FaultCase", "MemoryRef", "name", "memory_id"),
    ("COMPANY_MEMORY", "Company", "MemoryRef", "name", "memory_id"),
    ("PROJECT_MEMORY", "Project", "MemoryRef", "name", "memory_id"),
    ("REGION_MEMORY", "Region", "MemoryRef", "name", "memory_id"),
    ("FAMILY_MEMORY", "Family", "MemoryRef", "name", "memory_id"),
]

# ---------------------------------------------------------------- MCP 客户端

def _post(payload, session=None):
    headers = {"Content-Type": "application/json",
               "Accept": "application/json, text/event-stream"}
    if session:
        headers["mcp-session-id"] = session
    req = urllib.request.Request(MCP_URL, data=json.dumps(payload).encode(), headers=headers)
    resp = urllib.request.urlopen(req, timeout=15)
    sid = resp.headers.get("mcp-session-id")
    body = resp.read().decode().strip()
    obj = None
    if body.startswith("{"):
        obj = json.loads(body)
    else:
        for line in body.splitlines():
            if line.startswith("data: {"):
                obj = json.loads(line[6:])
                break
    return obj, sid


def call_tool(name, args):
    """完整握手 + 工具调用，返回解析后的文本 JSON。"""
    _, sid = _post({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "graph-viewer", "version": "1.0"}}})
    _post({"jsonrpc": "2.0", "method": "notifications/initialized"}, sid)
    obj, _ = _post({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": name, "arguments": args}}, sid)
    return json.loads(obj["result"]["content"][0]["text"])


def q(cypher):
    return call_tool("graph_query", {"cypher": cypher}).get("results", [])


def entity_memories(label, name):
    """实体的全部关联记忆（含全文）。"""
    ids = call_tool("graph_get_related_memories",
                    {"label": label, "name": name}).get("memory_ids", [])
    mems = []
    for mid in ids:
        try:
            mems.append(call_tool("get_memory", {"memory_id": mid}))
        except Exception:  # noqa: BLE001
            mems.append({"id": mid, "content": "（读取失败）"})
    mems.sort(key=lambda m: m.get("created_at") or "", reverse=True)
    return {"label": label, "name": name, "memories": mems}


def memory_context(memory_id):
    """单条记忆全文 + 反向挂载它的实体列表。"""
    try:
        mem = call_tool("get_memory", {"memory_id": memory_id})
    except Exception:  # noqa: BLE001
        mem = {"id": memory_id, "content": "（读取失败）"}
    parents = []
    for rel, src, dst, _skey, _dkey in RELATIONS:
        if dst != "MemoryRef":
            continue
        for row in q(f"MATCH (a:{src})-[r:{rel}]->(m:MemoryRef {{memory_id:'{memory_id}'}}) "
                     "RETURN a.name AS name"):
            parents.append({"label": src, "name": row["name"], "rel": rel})
    return {"memory": mem, "parents": parents}


# ---------------------------------------------------------------- 数据组装

def build_graph():
    nodes, links = [], []
    seen = set()

    def node(nid, ntype, name, preview="", mid=""):
        if nid not in seen:
            seen.add(nid)
            nodes.append({"id": nid, "type": ntype, "name": name,
                          "preview": preview, "mid": mid})

    # 实体节点
    for label in LABELS:
        if label == "MemoryRef":
            continue
        for row in q(f"MATCH (n:{label}) RETURN n.name AS name"):
            node(f"{label}:{row['name']}", label, row["name"])
    # MemoryRef 节点
    for row in q("MATCH (m:MemoryRef) RETURN m.memory_id AS id, m.content_preview AS p"):
        node(f"mem:{row['id']}", "MemoryRef", row["id"][:8], row.get("p") or "", mid=row["id"])

    # 边
    for rel, src, dst, skey, dkey in RELATIONS:
        rows = q(f"MATCH (a:{src})-[r:{rel}]->(b:{dst}) RETURN a.{skey} AS s, b.{dkey} AS t")
        for row in rows:
            sid_ = f"{src}:{row['s']}" if src != "MemoryRef" else f"mem:{row['s']}"
            tid_ = f"{dst}:{row['t']}" if dst != "MemoryRef" else f"mem:{row['t']}"
            links.append({"source": sid_, "target": tid_, "rel": rel})
    return {"nodes": nodes, "links": links}


# ---------------------------------------------------------------- HTTP 服务

PAGE = r"""<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="utf-8">
<title>记忆体知识图谱</title>
<style>
  body { margin:0; font-family:-apple-system,"PingFang SC",sans-serif; background:#0f1420; color:#dde3ee; }
  #bar { position:fixed; top:0; left:0; right:0; height:44px; background:#161d2e; display:flex;
         align-items:center; gap:14px; padding:0 14px; box-shadow:0 1px 4px #0008; z-index:10; }
  #bar b { font-size:14px; }
  #search { background:#0f1420; border:1px solid #2a3550; color:#dde3ee; border-radius:6px;
            padding:4px 10px; width:180px; font-size:12px; }
  #legend { position:fixed; left:12px; bottom:12px; z-index:9; background:#161d2eee;
            border:1px solid #2a3550; border-radius:10px; padding:8px 12px;
            display:grid; grid-template-columns:repeat(2, auto); gap:5px 16px; }
  .lg { display:flex; align-items:center; gap:5px; font-size:11px; cursor:pointer; user-select:none; color:#aab4c8; }
  .lg i { width:9px; height:9px; border-radius:50%; display:inline-block; }
  .lg.off { opacity:.25; }
  #meta { margin-left:auto; font-size:11px; color:#8b96ab; }
  #refresh { background:#2a3550; color:#dde3ee; border:0; border-radius:6px; padding:5px 12px; cursor:pointer; }
  #stage { position:fixed; top:44px; left:0; right:0; bottom:0; cursor:grab; user-select:none; -webkit-user-select:none; }
  #panel { position:fixed; right:0; top:44px; bottom:0; width:360px; background:#161d2eee;
           border-left:1px solid #2a3550; padding:12px; overflow:auto; }
  #p-head { font-size:13px; font-weight:600; margin-bottom:4px; }
  #p-sub { font-size:11px; color:#8b96ab; margin-bottom:10px; }
  .ent { font-size:12px; margin:2px 0 8px; }
  .mem { border:1px solid #232e48; border-radius:8px; margin:6px 0; background:#121828; overflow:hidden; }
  .mem-h { padding:7px 9px; cursor:pointer; }
  .mem-h:hover { background:#1a2238; }
  .mem-h .prev { font-size:12px; color:#c3cbdc; }
  .mem-h .meta { font-size:10px; color:#7f8aa3; margin-top:3px; }
  .mem .full { display:none; white-space:pre-wrap; word-break:break-all; font-size:11.5px;
               line-height:1.65; color:#b9c2d4; padding:2px 10px 10px; border-top:1px dashed #232e48;
               font-family:inherit; margin:0; }
  .mem.open .full { display:block; }
  .mem.open .mem-h { background:#1a2238; }
  .cat { display:inline-block; width:8px; height:8px; border-radius:50%; margin-right:6px; }
  #tip { position:fixed; pointer-events:none; background:#1d2740; border:1px solid #35426b;
         padding:6px 10px; border-radius:6px; font-size:12px; max-width:320px; display:none; z-index:20; }
</style>
</head>
<body>
<div id="bar">
  <b>记忆体知识图谱</b>
  <input id="search" placeholder="搜索节点…">
  <span id="meta"></span>
  <button id="refresh">刷新数据</button>
</div>
<svg id="stage"></svg>
<div id="legend"></div>
<div id="panel">
  <div id="p-head">记忆树</div>
  <div id="p-sub">点击图谱中的实体节点，查看其关联的全部记忆</div>
  <div id="p-body"></div>
</div>
<div id="tip"></div>
<script>
const COLORS = __COLORS__;
const svg = document.getElementById('stage'), panel = document.getElementById('panel'),
      tip = document.getElementById('tip'), meta = document.getElementById('meta');
let nodes = [], links = [], hidden = new Set(), query = '';
let W = innerWidth, H = innerHeight - 44;
const PANEL_W = 360;                 // 右侧记忆树面板宽度，布局中心避开它
let view = {k: 1, x: -PANEL_W / 2, y: 0};
let selId = null;

function resize() { W = innerWidth; H = innerHeight - 44;
  svg.setAttribute('width', W); svg.setAttribute('height', H); }
addEventListener('resize', resize); resize();

const CAT_COLORS = {knowledge:'#4C8DFF', fact:'#35B37E', episode:'#E8A33D',
                    preference:'#9B6DE8', procedure:'#3DC9C9', user_profile:'#FF8FB1'};
const catColor = c => CAT_COLORS[c] || '#7f8aa3';
const esc = s => (s || '').replace(/&/g, '&amp;').replace(/</g, '&lt;');

// 图例
const lgBox = document.getElementById('legend');
for (const [t, c] of Object.entries(COLORS)) {
  const s = document.createElement('span'); s.className = 'lg'; s.dataset.t = t;
  s.innerHTML = `<i style="background:${c}"></i>${t}`;
  s.onclick = () => { s.classList.toggle('off');
    s.classList.contains('off') ? hidden.add(t) : hidden.delete(t); tick(); };
  lgBox.appendChild(s);
}

async function load() {
  meta.textContent = '加载中…';
  const d = await (await fetch('/api/graph')).json();
  nodes = d.nodes.map(n => ({...n, x: W/2 + (Math.random()-.5)*400, y: H/2 + (Math.random()-.5)*300, vx:0, vy:0}));
  const idx = Object.fromEntries(nodes.map(n => [n.id, n]));
  links = d.links.filter(l => idx[l.source] && idx[l.target])
                 .map(l => ({s: idx[l.source], t: idx[l.target], rel: l.rel}));
  meta.textContent = `${nodes.length} 节点 · ${links.length} 边`;
  for (let i = 0; i < 260; i++) step();
  draw();
}
document.getElementById('refresh').onclick = load;

function step() {
  for (const a of nodes) for (const b of nodes) {
    if (a === b) continue;
    let dx = b.x-a.x, dy = b.y-a.y, d2 = dx*dx+dy*dy || 1;
    if (d2 > 90000) continue;
    const f = 2400 / d2, d = Math.sqrt(d2);
    a.vx -= dx/d*f; a.vy -= dy/d*f; b.vx += dx/d*f; b.vy += dy/d*f;
  }
  for (const l of links) {
    let dx = l.t.x-l.s.x, dy = l.t.y-l.s.y, d = Math.sqrt(dx*dx+dy*dy) || 1;
    const target = (l.s.type==='MemoryRef' || l.t.type==='MemoryRef') ? 70 : 130;
    const f = (d-target) * 0.012;
    l.s.vx += dx/d*f; l.s.vy += dy/d*f; l.t.vx -= dx/d*f; l.t.vy -= dy/d*f;
  }
  for (const n of nodes) {
    n.vx += ((W - PANEL_W) / 2 - n.x) * 0.0035; n.vy += (H/2 - n.y) * 0.0035;
    n.vx *= .82; n.vy *= .82; n.x += n.vx; n.y += n.vy;
    n.x = Math.max(30, Math.min(W - PANEL_W - 30, n.x)); n.y = Math.max(30, Math.min(H-30, n.y));
  }
}

function visible(n) { return !hidden.has(n.type) &&
  (!query || (n.name + ' ' + n.preview + ' ' + n.type).toLowerCase().includes(query)); }

function draw() {
  const show = nodes.filter(visible);
  const set = new Set(show.map(n => n.id));
  const shLinks = links.filter(l => set.has(l.s.id) && set.has(l.t.id));
  const Z = v => [ (v[0]-W/2)*view.k + W/2 + view.x, (v[1]-H/2)*view.k + H/2 + view.y ];
  let out = '';
  for (const l of shLinks) {
    const [x1,y1] = Z([l.s.x,l.s.y]), [x2,y2] = Z([l.t.x,l.t.y]);
    out += `<line x1="${x1}" y1="${y1}" x2="${x2}" y2="${y2}" stroke="#2a3550" stroke-width="1"/>`;
  }
  for (const n of show) {
    const [x,y] = Z([n.x,n.y]);
    const r = n.type==='MemoryRef' ? 5*view.k : 9*view.k;
    const c = COLORS[n.type] || '#888';
    const stroke = n.id === selId ? ' stroke="#fff" stroke-width="2"' : '';
    out += `<circle cx="${x}" cy="${y}" r="${r}" fill="${c}"${stroke} data-id="${n.id}" `+
           `style="cursor:pointer"><title>${n.type}:${n.name}\n${(n.preview||'').slice(0,120)}</title></circle>`;
    if (n.type !== 'MemoryRef')
      out += `<text x="${x}" y="${y + r + 11}" text-anchor="middle" font-size="${10*view.k}" fill="#9aa5ba">${esc(n.name)}</text>`;
  }
  svg.innerHTML = out;
}
function tick() { draw(); }

// 事件委托（挂在 svg 上，innerHTML 重建不丢失）；拖拽超过 5px 视为平移/拖节点，不触发详情
let downPos = null;
svg.addEventListener('mousedown', e => {
  downPos = e.target.dataset && e.target.dataset.id ? [e.clientX, e.clientY] : null;
});
svg.addEventListener('click', e => {
  const id = e.target.dataset && e.target.dataset.id;
  if (!id || !downPos) return;
  if (Math.hypot(e.clientX - downPos[0], e.clientY - downPos[1]) > 5) return;
  const n = nodes.find(v => v.id === id);
  if (!n) return;
  n.type === 'MemoryRef' ? openMemory(n) : openEntity(n);
});
svg.addEventListener('mouseover', e => {
  const id = e.target.dataset && e.target.dataset.id;
  if (!id) return;
  const n = nodes.find(v => v.id === id);
  if (!n) return;
  tip.style.display = 'block';
  tip.innerHTML = `<b style="color:${COLORS[n.type]}">${n.type}</b> ${n.name}<br>${(n.preview || '').slice(0, 140)}`;
});
svg.addEventListener('mousemove', e => {
  if (tip.style.display === 'block') {
    tip.style.left = (e.clientX + 14) + 'px';
    tip.style.top = (e.clientY + 10) + 'px';
  }
});
svg.addEventListener('mouseout', e => {
  if (e.target.dataset && e.target.dataset.id) tip.style.display = 'none';
});

// 点击节点 → 右侧记忆树
const pHead = document.getElementById('p-head'), pSub = document.getElementById('p-sub'),
      pBody = document.getElementById('p-body');

function memCard(m) {
  const prev = (m.content || '').slice(0, 60).replace(/\\n/g, ' ');
  const full = esc(m.content || '');
  return `<div class="mem"><div class="mem-h" onclick="this.parentNode.classList.toggle('open')">
    <div class="prev"><span class="cat" style="background:${catColor(m.category)}"></span>${esc(prev)}…</div>
    <div class="meta">${esc(m.category || '')} · ${esc(m.namespace || '')} · importance ${m.importance ?? '-'} · ${(m.created_at || '').slice(0, 10)}</div>
  </div><div class="full">${full}</div></div>`;
}

async function openEntity(n) {
  selId = n.id; draw();
  pHead.innerHTML = `<span style="color:${COLORS[n.type]}">●</span> ${esc(n.name)}`;
  pSub.textContent = `${n.type} · 正在加载关联记忆…`;
  pBody.innerHTML = '';
  try {
    const d = await (await fetch('/api/entity_memories?label=' + encodeURIComponent(n.type)
                                + '&name=' + encodeURIComponent(n.name))).json();
    pSub.textContent = `${n.type} · ${d.memories.length} 条关联记忆（点击展开全文）`;
    pBody.innerHTML = d.memories.length
      ? d.memories.map(memCard).join('')
      : '<div style="font-size:12px;color:#7f8aa3">（无关联记忆）</div>';
  } catch (e) { pSub.textContent = '加载失败：' + e; }
}

async function openMemory(n) {
  selId = n.id; draw();
  pHead.innerHTML = `<span style="color:${COLORS.MemoryRef}">●</span> 记忆 ${esc(n.name)}`;
  pSub.textContent = '加载中…';
  pBody.innerHTML = '';
  try {
    const d = await (await fetch('/api/memory_context/' + (n.mid || n.name))).json();
    const m = d.memory || {};
    pSub.textContent = (d.parents.length
      ? '挂在：' + d.parents.map(p => `${p.label}:${p.name}`).join('、')
      : '（未挂载到任何实体）');
    pBody.innerHTML = memCard(m);
    const card = pBody.querySelector('.mem');
    if (card) card.classList.add('open');
  } catch (e) { pSub.textContent = '加载失败：' + e; }
}

// 搜索 & 缩放拖拽
document.getElementById('search').oninput = e => { query = e.target.value.toLowerCase(); draw(); };
let dragging = null, panning = null;
svg.onmousedown = e => {
  const id = e.target.dataset && e.target.dataset.id;
  if (id) { dragging = nodes.find(v=>v.id===id); }
  else panning = {x: e.clientX, y: e.clientY};
};
svg.onmousemove = e => {
  if (dragging) {
    dragging.x = (e.clientX - W/2 - view.x)/view.k + W/2;
    dragging.y = (e.clientY - H/2 - view.y)/view.k + H/2;
    dragging.vx = dragging.vy = 0; draw();
  } else if (panning) {
    view.x += e.clientX - panning.x; view.y += e.clientY - panning.y;
    panning = {x: e.clientX, y: e.clientY}; draw();
  }
};
addEventListener('mouseup', () => { dragging = null; panning = null; });
svg.onwheel = e => { e.preventDefault();
  view.k = Math.max(.3, Math.min(3, view.k * (e.deltaY < 0 ? 1.1 : .9))); draw(); };

load();
</script>
</body>
</html>"""


class Handler(BaseHTTPRequestHandler):
    def _send(self, code, ctype, body):
        data = body if isinstance(body, bytes) else body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/" or self.path.startswith("/index"):
            self._send(200, "text/html; charset=utf-8",
                       PAGE.replace("__COLORS__", json.dumps(LABELS)))
        elif self.path == "/api/graph":
            try:
                self._send(200, "application/json; charset=utf-8",
                           json.dumps(build_graph(), ensure_ascii=False))
            except Exception as e:  # noqa: BLE001
                self._send(502, "application/json; charset=utf-8",
                           json.dumps({"error": str(e)}))
        elif self.path.startswith("/api/entity_memories"):
            from urllib.parse import urlparse, parse_qs
            qs = parse_qs(urlparse(self.path).query)
            label = (qs.get("label") or [""])[0]
            name = (qs.get("name") or [""])[0]
            try:
                self._send(200, "application/json; charset=utf-8",
                           json.dumps(entity_memories(label, name), ensure_ascii=False))
            except Exception as e:  # noqa: BLE001
                self._send(502, "application/json; charset=utf-8",
                           json.dumps({"error": str(e)}))
        elif self.path.startswith("/api/memory_context/"):
            mid = self.path.rsplit("/", 1)[1]
            try:
                self._send(200, "application/json; charset=utf-8",
                           json.dumps(memory_context(mid), ensure_ascii=False))
            except Exception as e:  # noqa: BLE001
                self._send(502, "application/json; charset=utf-8",
                           json.dumps({"error": str(e)}))
        elif self.path.startswith("/api/memory/"):
            mid = self.path.rsplit("/", 1)[1]
            try:
                raw = call_tool("get_memory", {"memory_id": mid})
                self._send(200, "application/json; charset=utf-8",
                           json.dumps(raw, ensure_ascii=False))
            except Exception as e:  # noqa: BLE001
                self._send(502, "application/json; charset=utf-8",
                           json.dumps({"error": str(e)}))
        else:
            self._send(404, "text/plain", "not found")

    def log_message(self, fmt, *args):  # 静默访问日志
        pass


def main():
    global MCP_URL
    ap = argparse.ArgumentParser(description="图谱可视化网页服务器（只读，经 MCP 取数）")
    ap.add_argument("--port", type=int, default=8899)
    ap.add_argument("--mcp", default=MCP_URL, help="MCP 端点（默认 127.0.0.1:8888/mcp）")
    args = ap.parse_args()
    MCP_URL = args.mcp
    srv = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"图谱查看器: http://127.0.0.1:{args.port}/  (MCP: {MCP_URL})")
    srv.serve_forever()


if __name__ == "__main__":
    main()
