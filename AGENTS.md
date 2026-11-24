# 本机开发/运维须知（agent-memory-rs）

> 适用于在本机（Windows）这个仓库里工作的所有人和 agent。最后更新：2026-11-24。

## 1. 铁律：本机只跑代理，不跑本地服务

- **不要启动 `agent-memory-server.exe`**——任何端口、任何配置都不要，8888 更不行。
- **只能启动代理** `agent-memory-proxy.exe`，而且**只能有一个**。
- 8888 不通时唯一正确的动作是重新拉起代理（幂等，已在运行就直接返回）：

  ```bash
  bash scripts/start_memory_proxy.sh --detach
  ```

- **在 agent 会话（ZCode 等）里不要用 `--daemon`**：那样拉起的代理落在该应用的 job 里，应用一退出或更新就被一并带走，
  2026-11-23 代理就是这样消失的（见第 6 节）。`--detach` 经 WMI 重新拉起自身再走 `--daemon`，代理不属于任何应用的进程树。
- 直连 104 返回 `401` 是正常的（104 需要 token）。**你不需要自己拿 token**：代理会自动注入，
  `scripts/sync_local_to_104.py` 也是自己从 `deployment/proxy.yaml` 读。不要因为「找不到 token」改起本地 server。
- `deployment/proxy.yaml` 里的 `access_key` / `server_auth_token` 是未入库的本地机密：不要提交，不要抄进文档、代码或记忆。
- README 里「`agent-memory-server --transport http --port 8888`」是通用部署说明，**在本机不适用**。

为什么：本地 server 一起，8888 上就换成了本机旧库，所有 agent 的读写会**静默**落到本地，104 什么都收不到，
而且不会有任何报错（2026-11-24 就是这样丢过 3 条，见第 6 节）。

## 2. `http://127.0.0.1:8888/mcp` 指向哪里

```
本机 AI agent ──> http://127.0.0.1:8888/mcp  （本机代理 agent-memory-proxy，监听 0.0.0.0:8888）
                      │ HTTP 直连（代理注入 server_auth_token）
                      ▼
                http://192.168.25.104:8888/mcp （104 上的 Docker 容器 agent-memory，数据卷 agent-memory-data）
```

- **所有记忆数据只在 104。** 本机没有任何生产数据，本机的 SQLite / Kùzu 文件都是历史副本（见第 5 节）。
- 本机回环访问免鉴权；局域网其他机器访问 8888 需要 `Authorization: Bearer <access_key>`。
- 只认 `127.0.0.1:8888` 的客户端：ZCode（`~/.zcode/cli/config.json`）、AStudio（huimei-his 项目的
  `acode-home-overlay/config.toml`）、局域网机器（如 192.168.251.152）。所以 8888 上是谁至关重要。
- 开机自启：启动文件夹的 `start_memory_proxy.vbs`。**它并不等价于 `--detach`，而且有缺陷**：
  `bash -lc <脚本> --daemon` 里的 `--daemon` 被当成了 `$0`，脚本实际以前台模式运行——日志被隐藏窗口吞掉，
  工作目录也不是仓库（失败写入的落盘目录 `write-journal` 因此不在仓库里）。尚未修复，见第 6 节「遗留」。

## 3. 确认 8888 上是代理（30 秒）

| 检查 | 代理（正确） | 本地 server（错误） |
|---|---|---|
| `curl -s -i http://127.0.0.1:8888/health` | `200` + `ok` | `404` |
| `netstat -ano \| grep ':8888.*LISTENING'` | `0.0.0.0:8888` | `127.0.0.1:8888` |
| `tasklist \| grep -i agent-memory` | 只有 1 个 `agent-memory-proxy.exe` | 出现 `agent-memory-server.exe` |
| `get_memory_stats` 的 `total_memories` | 104 的量级（两千多条、二十多个 namespace） | 本地旧副本（两百多条、12 个 namespace） |

`initialize` 响应区分不了：两边都是 `rmcp 3.2.0`。

还要确认代理**没有挂在某个应用（ZCode）的进程树下**，否则 ZCode 一退出它就没了：

```bash
powershell -NoProfile -Command "$(cat <<'EOF'
$sig = '[DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint a, bool i, int p); [DllImport("kernel32.dll")] public static extern bool IsProcessInJob(IntPtr h, IntPtr j, out bool r);'
Add-Type -Namespace W -Name K -MemberDefinition $sig
$h = [W.K]::OpenProcess(0x1000, $false, (Get-Process agent-memory-proxy).Id); $r = $false
[void][W.K]::IsProcessInJob($h, [IntPtr]::Zero, [ref]$r); "inJob=$r"
EOF
)"
```

`inJob=False` 才对。是 `True` 就重来一遍：`taskkill //F //IM agent-memory-proxy.exe`，然后 `bash scripts/start_memory_proxy.sh --detach`。

## 4. 出现了本地独有数据：同步到 104

本地 server 默认写 `~/.agent-memory/memory.db`。一旦发现它被启动过，按下面做（先停再同步，否则还会继续分叉）：

1. 停掉本地 server——先确认 PID 对应的确实是 `agent-memory-server.exe`，再 `taskkill //F //PID <pid>`。
2. 拉起代理（第 1 节），按第 3 节确认。
3. 取 104 的只读快照并拉回本机（不改 104）：

   ```bash
   ssh root@192.168.25.104 "sqlite3 -readonly /var/lib/docker/volumes/agent-memory-data/_data/memory.db '.backup /tmp/am-104.db'"
   scp root@192.168.25.104:/tmp/am-104.db "C:/Users/ding/AppData/Local/Temp/am-104.db"   # 目标必须是带盘符的 Windows 路径
   ```

4. 先预演，再执行（执行时会逐条回读校验）：

   ```bash
   python scripts/sync_local_to_104.py --remote-db "C:/Users/ding/AppData/Local/Temp/am-104.db"           # 预演，不写
   python scripts/sync_local_to_104.py --remote-db "C:/Users/ding/AppData/Local/Temp/am-104.db" --apply  # 写入 104
   ```

5. 重新取一次 104 快照再跑一遍预演，应当显示「只在本地 0 条」。

脚本的边界，用之前必须知道：

- **直连 104 写，不走代理**。代理对 `store_memory` 会无条件重新生成 `memory_id`，走代理就保不住原 ID
  （客户端手里已经是旧 ID，之后 get/update/delete 会对不上）。
- `created_at` / `updated_at` 会变成同步时刻（接口不接受原始时间戳）；ID、正文、标签、分类、重要度、向量都不变。
- 两边都有但内容不同、或本地已删而 104 还在的记录：**只报告、不处理**，需要人工确认。
- 104 没有启用 graph，本地 Kùzu 图数据不同步。
- 同步只往 104 补数据，**不删本地库里的任何东西**。

## 5. 本机的数据文件（都不是生产数据）

| 路径 | 是什么 |
|---|---|
| `~/.agent-memory/memory.db` | 本地 server 的默认库：2026-10-11 的 104 旧快照，加上之后被误写入的记录。出问题时拿它和 104 比对 |
| `~/.agent-memory/graph.kuzu` | 本地 server 的图库，同样是旧的 |
| `~/.agent-memory/data-snapshot.db` | 2026-10-11 的 104 快照，没有进程在用 |
| 仓库里的 `data/memory.db`、`data/graph.kuzu` | 同一份 2026-10-11 快照（`memory.db` 与上一行 md5 相同）。**Dockerfile 的 `COPY data/ /data/` 会把它打进镜像**，新建的空数据卷会以它为初始数据——所以它不是生产库，别往里写；现有卷 `agent-memory-data` 不受它影响 |
| `~/.agent-memory/backup-from-104-20261011.db` | 104 库在 2026-10-11 的备份 |
| `logs/proxy.log`、`write-journal/` | 代理日志，以及转发失败的写请求的落盘目录（有残留文件 = 有写请求还没送到 104）。**只有工作目录是仓库的代理才写这里**；经启动文件夹 VBS 拉起的实例两者都不在仓库 |
| `E:\ChatGPT\kuzu_memory\agent-memory.yaml` | 本地 server 的配置（`127.0.0.1:8888` + graph）。本机不该用它启动任何东西 |

## 6. 事故记录

### 2026-11-24：本地 server 占住 8888，3 条记忆写进了本地库

- **代理为什么不在（根因）**：11-21 10:09:27 一次 ZCode 会话用 `start_memory_proxy.sh --daemon` 重启了代理，
  代理从此挂在 ZCode 的进程树（job）下。11-23 09:29:34 ZCode 从 0.17.1 更新到 0.18.0 并重启，代理被一并结束
  （可确定的区间是 09:19:40–09:34:19：之前还在正常转发，之后第一个新会话连 8888 直接 `fetch failed`）。
  不是崩溃，也不是机器重启：系统日志在这段时间没有任何关机/重启/睡眠事件，应用日志里没有代理的崩溃记录，
  同一登录会话里的其他进程（explorer、微信、Chrome、Clash、终端里的 redis 等）全部存活。
- **为什么没人发现**：之后没有任何东西把代理拉回来——启动文件夹的自启只在登录时触发，而本次登录会话从
  11-06 15:08 一直延续到现在。8888 空了 31 小时，ZCode 每次开会话连 agent-memory 都失败（12 次
  `Version negotiation probe failed: fetch failed`），但只记在 `~/.zcode/cli/log/zcode-<日期>.jsonl` 里。
- 16:24:35 AStudio agent 在 huimei-his 项目里发现 8888 不通、直连 104 返回 401，
  误以为「本机没有 proxy.yaml 的真实 token」，于是用
  `agent-memory-server.exe --config E:\ChatGPT\kuzu_memory\agent-memory.yaml` 起了本地 server。
- 16:25:37 3 条 `huimei-his` 记忆写进了本地库，104 没有。
- 处理：本地库做只读快照 → 停本地 server → 起代理 → 按 ID 比对（本地 232 条里 229 条与 104 完全一致，
  仅这 3 条本地独有，无冲突）→ `sync_local_to_104.py` 同步 3 条 → 回读、全字段、向量比对全部通过
  （104 从 2396 条到 2400 条，多出的另 1 条是期间经代理的正常写入）。
- 当天 16:41 那次重启用的是 `--daemon`（从 ZCode 会话里执行），代理又挂回了 ZCode 的进程树；
  17:21 改用 `--detach` 重启，确认 `inJob=False`、日志和落盘目录都在仓库。

| memory_id | 分类 | 原始 created_at（UTC） | 104 上的 created_at（UTC） |
|---|---|---|---|
| `63949863-a73b-4f1d-aced-322671da95fb` | feature | `2026-11-24T08:25:37.570` | `2026-11-24T08:50:43.651` |
| `4abe9451-2786-4802-a897-ae3908a90bd8` | bugfix | `2026-11-24T08:25:37.587` | `2026-11-24T08:50:43.718` |
| `5620540e-fd9d-4d2d-9cfb-15a8ee5f5840` | deployment | `2026-11-24T08:25:37.618` | `2026-11-24T08:50:43.775` |

- 同步前 104 库的备份：`104:/root/agent-memory-backup-20261124-presync.db`。

遗留：

- **启动文件夹的 VBS 会丢掉 `--daemon`**（第 2 节）。后果：开机自启拉起的代理没有日志，失败写入的落盘目录也不在仓库
  （实测经 WMI 拉起时工作目录是 `C:\Windows\system32`；登录自启时的工作目录没实测，但同样不是仓库）。当前实例是 `--detach` 拉起的，不受影响，但下次登录自启还会出现。
  修法：VBS 里把命令整体加引号（`-lc "/e/.../start_memory_proxy.sh --daemon"`），并让脚本自己 `cd` 到仓库根目录；
  仓库里的 `scripts/start_memory_proxy.vbs` 和启动文件夹里的副本要一起改。尚未修。
- **代理没有守护**：进程一旦退出不会自动重启。此前实测本机 `schtasks /create` 被策略拒绝，做不了计划任务兜底。
- 待负责人决定：第 3 条记忆（`5620540e…`）正文的「记忆体 MCP 运维」一节记录的正是上面的错误做法
  （教人起本地 server），并且含一个数据库口令明文。按原样同步，未做改动。
