#!/usr/bin/env python3
"""
sync_local_to_104.py — 把「只在本地库、104 没有」的记忆同步到 104

仅用于事故恢复：本机误启动了 agent-memory-server（占住 8888），导致写入落到本地库。
正常情况下本机只跑代理，所有读写直达 104，不存在两边数据。

用法:
  python scripts/sync_local_to_104.py --remote-db <104只读快照.db>            # 预演（默认）：只报告差异，不写
  python scripts/sync_local_to_104.py --remote-db <104只读快照.db> --apply   # 实际同步

104 只读快照的取法（不改动 104）:
  ssh root@192.168.25.104 "sqlite3 -readonly /var/lib/docker/volumes/agent-memory-data/_data/memory.db '.backup /tmp/am-104.db'"
  scp root@192.168.25.104:/tmp/am-104.db "C:/Users/ding/AppData/Local/Temp/am-104.db"

要点:
  - 本地库经 SQLite backup API 取一致性快照，全程只读，不改动本地库
  - 按 id 比对，只推「本地有、104 无」的记录；id 在 104 墓碑里的跳过（104 上已删，不复活）
  - 直连 104 写入而不走本机代理：代理对 store_memory 会重新生成 memory_id，
    直连带 memory_id 才能保住原 ID（客户端已经拿着这些 ID 做 get/update/delete）
  - 两边都有但内容不同的记录只报告不覆盖（可能是一侧的更新/删除，需人工确认）
  - created_at/updated_at 会变成同步时刻（store_memory 不接受原始时间戳），其余字段原样
  - 104 未启用 graph，本地 Kùzu 图数据不同步
"""

import argparse
import json
import os
import re
import sqlite3
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
DEFAULT_CONFIG = SCRIPT_DIR.parent / "deployment" / "proxy.yaml"
DEFAULT_LOCAL_DB = Path.home() / ".agent-memory" / "memory.db"
MAX_CONTENT_LENGTH = 10000
COLS = ["id", "namespace", "content", "category", "importance", "topics"]


def read_ro(path):
    """只读打开 SQLite（本地 server 在跑时也可安全读取）"""
    return sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro", uri=True)


def take_snapshot(db_path):
    """经 backup API 把本地库复制成一致性快照文件，返回快照路径（源库不被写入）"""
    fd, tmp = tempfile.mkstemp(prefix="am-local-snapshot-", suffix=".db")
    os.close(fd)
    src = read_ro(db_path)
    dst = sqlite3.connect(tmp)
    try:
        src.backup(dst)
    finally:
        dst.close()
        src.close()
    return tmp


def load_config(path):
    """从 proxy.yaml 取 target / server_auth_token（避免为此引入 PyYAML）"""
    text = Path(path).read_text(encoding="utf-8")
    cfg = {}
    for key in ("target", "server_auth_token"):
        m = re.search(rf'^\s*{key}\s*:\s*"?([^"\r\n#]+?)"?\s*(?:#.*)?$', text, re.M)
        if not m:
            sys.exit(f"[ERROR] {path} 里找不到 {key}")
        cfg[key] = m.group(1).strip()
    return cfg


class McpClient:
    """直连 104 的最小 MCP over HTTP 客户端（忽略系统代理设置）"""

    def __init__(self, url, token, timeout=30):
        self.url = url
        self.token = token
        self.timeout = timeout
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def call_tool(self, name, arguments):
        payload = json.dumps({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }, ensure_ascii=False).encode("utf-8")
        req = urllib.request.Request(self.url, data=payload, method="POST", headers={
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "Authorization": "Bearer " + self.token,
        })
        try:
            with self.opener.open(req, timeout=self.timeout) as resp:
                raw = resp.read().decode("utf-8")
        except urllib.error.HTTPError as e:
            raise RuntimeError("HTTP %s: %s" % (e.code, e.read().decode("utf-8", "replace")[:300]))
        # 响应可能是 JSON，也可能是 SSE（取第一个 data: 行）
        if raw.lstrip().startswith(("event:", "data:")):
            raw = next(line[5:].strip() for line in raw.splitlines() if line.startswith("data:"))
        rpc = json.loads(raw)
        if "error" in rpc:
            raise RuntimeError("JSON-RPC error: %s" % (rpc["error"],))
        result = rpc["result"]
        text = result["content"][0]["text"] if result.get("content") else ""
        if result.get("isError"):
            raise RuntimeError("%s 返回错误: %s" % (name, text[:300]))
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            return {"_text": text}


def local_server_running():
    """本地 server 还在跑就先停：否则同步期间还会有新写入落到本地库"""
    try:
        out = subprocess.run(["tasklist", "/FO", "CSV", "/NH"],
                             capture_output=True, text=True, errors="replace").stdout
    except OSError:
        return False
    return any(line.lower().startswith('"agent-memory-server') for line in out.splitlines())


def main():
    ap = argparse.ArgumentParser(description="把本地独有的记忆同步到 104")
    ap.add_argument("--remote-db", required=True, help="104 库的只读快照路径")
    ap.add_argument("--local-db", default=str(DEFAULT_LOCAL_DB), help="本地库路径（默认 ~/.agent-memory/memory.db）")
    ap.add_argument("--config", default=str(DEFAULT_CONFIG), help="代理配置（取 target 与 server_auth_token）")
    ap.add_argument("--apply", action="store_true", help="实际写入 104；缺省只预演")
    args = ap.parse_args()

    cfg = load_config(args.config)
    local_snapshot = take_snapshot(args.local_db)
    loc = read_ro(local_snapshot)
    rem = read_ro(args.remote_db)
    try:
        return run(loc, rem, cfg, args.apply)
    finally:
        loc.close()
        rem.close()
        os.unlink(local_snapshot)


def run(loc, rem, cfg, apply):
    loc.row_factory = rem.row_factory = sqlite3.Row
    L = {r["id"]: dict(r) for r in loc.execute("SELECT %s, created_at FROM memories" % ",".join(COLS))}
    R = {r["id"]: dict(r) for r in rem.execute("SELECT %s, created_at, updated_at FROM memories" % ",".join(COLS))}
    # 库里 topics 是 JSON 字符串，API 回读是数组——统一成数组再比对
    for d in list(L.values()) + list(R.values()):
        d["topics"] = json.loads(d["topics"] or "[]")
    remote_tombstones = {r[0] for r in rem.execute("SELECT id FROM tombstones")}
    local_tombstones = {r[0] for r in loc.execute("SELECT id FROM tombstones")}

    only_local = sorted((i for i in L if i not in R), key=lambda i: L[i]["created_at"])
    to_sync = [i for i in only_local if i not in remote_tombstones]
    resurrect = [i for i in only_local if i in remote_tombstones]
    diverged = [i for i in L if i in R and any(L[i][c] != R[i][c] for c in COLS)]
    deleted_locally = sorted(local_tombstones & set(R))

    print("本地库 %d 条 | 104 快照 %d 条 | 两边都有 %d 条" % (len(L), len(R), len(L) - len(only_local)))
    print("只在本地 %d 条 -> 待同步 %d 条" % (len(only_local), len(to_sync)))
    for i in to_sync:
        m = L[i]
        print("  + %s  %s/%s  imp=%s  %d 字  %s" % (i, m["namespace"], m["category"], m["importance"],
                                                 len(m["content"]), m["created_at"]))
    for i in resurrect:
        print("  ! 跳过 %s：104 墓碑里已有（104 上已删除，不复活）" % i)
    for i in diverged:
        print("  ! 冲突 %s：两边都有但内容不同，未处理（需人工确认）" % i)
    for i in deleted_locally:
        print("  ! 冲突 %s：本地已删、104 仍在，未处理（需人工确认）" % i)

    if not to_sync:
        print("\n[OK] 没有需要同步的记录")
        return 0
    if not apply:
        print("\n[预演] 未写入 104。确认无误后加 --apply 执行")
        return 0
    if local_server_running():
        print("\n[ERROR] 本地 agent-memory-server 仍在运行，先停掉它再同步（否则还会继续产生本地独有数据）")
        return 1

    client = McpClient(cfg["target"], cfg["server_auth_token"])
    failed = 0
    print("\n[APPLY] 直连 %s 写入 %d 条 …" % (cfg["target"], len(to_sync)))
    for i in to_sync:
        m = L[i]
        if len(m["content"]) > MAX_CONTENT_LENGTH:
            print("  x %s 超过 %d 字，服务端会截断，已跳过" % (i, MAX_CONTENT_LENGTH))
            failed += 1
            continue
        try:
            resp = client.call_tool("store_memory", {
                "content": m["content"],
                "namespace": m["namespace"],
                "category": m["category"],
                "importance": m["importance"],
                "tags": m["topics"],
                "memory_id": i,
            })
            got = client.call_tool("get_memory", {"memory_id": i})
        except Exception as e:  # 单条失败不影响其余记录
            print("  x %s 写入/回读失败：%s" % (i, e))
            failed += 1
            continue

        old_created = m["created_at"]
        new_created = got.get("created_at")
        if got.get("id") != i or resp.get("memory_id") != i:
            print("  x %s 回读不一致：store=%s get=%s" % (i, resp, got.get("error") or got.get("id")))
            failed += 1
            continue
        mismatch = [c for c in COLS if c != "id" and got.get(c) != m[c]]
        if mismatch:
            print("  x %s 回读字段不一致 %s  created_at %s -> %s" % (i, mismatch, old_created, new_created))
            failed += 1
        else:
            print("  v %s 字段一致  created_at %s -> %s" % (i, old_created, new_created))

    if failed:
        print("\n[FAIL] %d 条失败。修正后可重跑：store 按 ID 幂等，重复写入只覆盖为相同内容" % failed)
        return 1
    print("\n[OK] 同步完成，%d 条全部回读一致" % len(to_sync))
    return 0


if __name__ == "__main__":
    sys.exit(main())
