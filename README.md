# agent-memory-rs

基于 Rust 重写的 MCP 外挂记忆体服务器，使用 SQLite + 可选 Kùzu 知识图谱作为存储后端。

## 功能

- **记忆存储**：SQLite 持久化，支持 namespace、category、importance、entities、topics 等元数据
- **向量检索**：纯 Rust 余弦相似度实现，支持 top-k 语义搜索
- **Embedding 自动检测**：启动时并发探测本地 OpenAI 兼容 embedding 服务（LM Studio:1234 / Ollama:11434 / vLLM:8000 / Xinference:9997 / llama.cpp:8080，或 `EMBEDDING_BASE_URL` 指定），命中即自动配置 provider/model/维度并替换内置伪嵌入；向量空间变化时自动重嵌存量记忆。未检测到时回退字符 bigram 伪嵌入（零外部依赖），配置 `embedding.provider` 显式指定时跳过探测
- **知识图谱**（可选）：Kùzu 嵌入式图数据库，支持实体/关系/多跳查询/按属性聚合
- **记忆自动入图**（可选）：`store_memory` 时按内置/自定义词典 + 命名空间骨架自动抽取实体并生成图谱节点与 `*_MEMORY` 关联；启动时可对存量记忆幂等回填一次
- **MCP 协议**：基于 rmcp 3.2，支持 stdio 与 HTTP（Streamable HTTP + SSE）两种传输，兼容 ZCode / Cursor / Claude Desktop 等 MCP 客户端
- **HTTP 鉴权**：`server.auth_token` 配置 Bearer Token 中间件；绑定非回环地址（非 127.0.0.1）时必须设置，否则拒绝启动
- **零外部依赖运行**：SQLite 内嵌（rusqlite bundled），单二进制部署

## 项目结构

```
src/
├── lib.rs       # 库导出
├── main.rs      # CLI 入口（stdio MCP server）
├── config.rs    # 配置管理（YAML + env + clap）
├── models.rs    # 数据模型（Memory / MemorySearchResult / MemoryStats）
├── storage.rs   # SQLite 存储层 + 向量检索
├── graph.rs     # Kùzu 图谱模块（feature = "graph"）
└── server.rs    # MCP ServerHandler 实现
examples/
└── bench.rs     # 性能基准测试
```

## 编译（跨平台开关）

通用命令（三平台等价）：

```bash
cargo build --release                  # 基础版（无图谱）
cargo build --release --features graph # 图谱版（kuzu，需平台工具链，见下表）
```

构建期平台开关由 `build.rs` 自动检测：启用 graph feature 时逐平台检查工具链
（cmake / CLT / GCC 版本 / MSVC 环境），缺失时在编译最早期打印 `cargo:warning`
与安装指引，而不是掉进 cmake 深处的报错。

| 平台 | graph 版工具链要求 | 一键脚本 | 备注 |
|---|---|---|---|
| **macOS** (arm64/x86_64) | Xcode CLT（`xcode-select --install`）+ cmake（`brew install cmake`）；Apple clang 可直接编译 kuzu，**无需 GCC 12** | `./scripts/build.sh` | `--no-graph` 纯记忆版；`--target` 交叉 |
| **Linux** (x86_64/aarch64) | GCC >= 12（或 clang >= 16）+ cmake；glibc >= 2.28 | `./scripts/build.sh` | 老系统用 `--musl` 静态编译；CC/CXX 可指向 gcc-12 |
| **Windows** (x86_64 MSVC) | VS Build Tools 2019/2022 均可（C++ 桌面开发 + CMake）；脚本会自动补齐 VS 自带的 cmake/ninja | `.\scripts\build.ps1` | `-NoGraph` 纯记忆版；`-Target` 交叉（MinGW 不推荐）；graph 版**必须 release**（见下注） |

CI：`.github/workflows/ci.yml` 在 macOS / Windows / Ubuntu 三平台 ×（graph / no-graph）
六种组合上自动构建验证。纯记忆版（无 graph）为纯 Rust，无平台工具链要求。

> kuzu 0.11.x 的 C++ 依赖使用 AVX-512 FP16 指令，Linux 下 GCC 11 及以下无法编译；
> macOS 的 Apple clang 与 Windows 的 MSVC 不受此限制。

> **Windows graph 版必须 `--release`**：debug 模式下 kuzu 的 C++ 静态库约 4 GB，rustc 会将其
> 整体打包进 rlib（static 原生库默认 +bundle），归档超过 ar 格式 32 位偏移的 4 GiB 上限后符号
> 表损坏，链接期报大量“无法解析的外部符号”（MSVC link.exe）或 malformed archive（lld-link）；
> release 的 kuzu.lib 约 1 GB 不受影响，CI 与 build.ps1 均走 release。

## 运行

```bash
# stdio 模式（默认，由 MCP 客户端拉起）
./target/release/agent-memory-server

# HTTP 模式（常驻服务，端点为 POST http://<host>:<port>/mcp）
./target/release/agent-memory-server --transport http --host 127.0.0.1 --port 8888

# HTTP 对外暴露（必须设置 auth_token）
AGENT_MEMORY_SERVER__AUTH_TOKEN=<secret> ./target/release/agent-memory-server --transport http --host 0.0.0.0 --port 8888

# 指定配置文件
./target/release/agent-memory-server --config config.yaml

# 查看帮助
./target/release/agent-memory-server --help
```

> 注意：stdio 模式直接在终端运行会立即退出（stdin 无 JSON-RPC 客户端），属正常行为。
> HTTP 模式每个会话由 service_factory 创建独立 handler，存储与 Kùzu 图谱通过 `Arc` 共享，
> 整个服务仍保持单进程单连接，符合 Kùzu 嵌入式单进程约束。

### macOS 常驻部署（launchd）

安装 `~/Library/LaunchAgents/com.agent-memory.server.plist` 后：

```bash
launchctl load ~/Library/LaunchAgents/com.agent-memory.server.plist   # 启动（开机自启、崩溃自动拉起）
launchctl kickstart -k gui/$(id -u)/com.agent-memory.server           # 手动重启（更新二进制后执行）
launchctl unload ~/Library/LaunchAgents/com.agent-memory.server.plist # 停止
tail -f ~/.agent-memory/server.log                                    # 查看日志
```

plist 关键配置：`RunAtLoad` + `KeepAlive=true`，环境变量 `AGENT_MEMORY_GRAPH__ENABLED=true`，
标准错误写入 `~/.agent-memory/server.log`。

## 嵌入语义计算（Embedding）

### 工作原理

`store_memory` / `search_memory` 写入与检索都依赖文本向量。服务按以下优先级选择嵌入来源
（实现见 `src/embedding.rs`）：

1. **显式配置**：配置文件或环境变量设置了 `embedding.provider` → 直接采用，不做探测
2. **自动探测**：启动时并发探测本机常见推理服务的 OpenAI 兼容端点
   （LM Studio:1234 / Ollama:11434 / vLLM:8000 / Xinference:9997 / llama.cpp:8080，
   或 `EMBEDDING_BASE_URL` 指定的地址），`GET /v1/models` 选模型 → `POST /v1/embeddings`
   试算维度，命中即自动回填 provider/model/维度
3. **伪嵌入回退**：都未命中时用内置字符 bigram 哈希嵌入（零外部依赖，字面相似度可用，
   语义泛化弱）

**向量空间一致性**：当前空间以 `kv_meta.embedding_space` 记录
（如 `openai:bge-m3@1024` 或 `pseudo-bigram@1536`）。提供方或维度变化时，启动阶段自动
**重嵌全部存量记忆**，保证新旧向量可比；因此在有/无 embedding 服务之间切换无需手工迁移。

### 使用方式

```bash
# 方式一：零配置 —— 本机跑着 LM Studio / Ollama 等即自动接入，无需任何设置

# 方式二：环境变量指定非默认端口/地址
EMBEDDING_BASE_URL=http://127.0.0.1:9527 ./agent-memory-server --transport http

# 方式三：配置文件显式指定（跳过探测；配置错误在使用时报错，便于定位）
```

```yaml
embedding:
  provider: openai                 # 非空即启用显式模式
  base_url: http://127.0.0.1:1234  # 缺 /v1 会自动补全
  model: bge-m3
  api_key: ""                      # 本地服务可留空
  dimensions: 1024                 # 必须与模型真实输出维度一致
```

> 依赖树刻意未引入 TLS：嵌入客户端仅支持 `http://`（本地服务场景）。
> 接入 https 云端 API（OpenAI / SiliconFlow 等）请起一个本地反代
> （如 `caddy reverse-proxy --reverse-from 127.0.0.1:1234 to api.siliconflow.cn`），
> 再按方式三指向反代地址。

### 如何增加额外的嵌入语义计算功能（配置文件模式）

**提供方选择完全由配置文件驱动**，内置两种协议，新增接入点无需改代码：

**方式 A：OpenAI 兼容服务（`provider: openai`，默认协议）**——任何提供
`POST /v1/embeddings` 的服务都能配置接入：云 API 走本地反代、自建推理
（`infinity`、`text-embeddings-inference`）或 LM Studio / vLLM 等。
模型挑选规则（自动探测时）见 `pick_model()`：名称含 `embed`/`bge`/`gte`/`e5`/`wemm` 优先。

**方式 B：Ollama 原生协议（`provider: ollama`）**——走 Ollama 自己的
`POST /api/embed {"model","input":[..]}` 批量接口（不再经由其 OpenAI 兼容层）：

```yaml
embedding:
  provider: ollama        # 选择原生协议，跳过自动探测
  base_url: http://127.0.0.1:11434   # 留空则默认 http://127.0.0.1:11434（不补 /v1）
  model: bge-m3
  dimensions: 1024        # 必须与模型真实输出一致
```

环境变量等价：`AGENT_MEMORY_EMBEDDING__PROVIDER=ollama`。

两种方式共用同一套向量空间机制：空间标识分别为 `openai:<model>@<dim>` /
`ollama:<model>@<dim>`，在 openai/ollama/伪嵌入之间切换配置并重启，存量记忆自动重嵌。

**仅当目标服务使用全新协议时才需要改代码**（在 `embedding.rs` 的 `Protocol` 枚举加变体、
`embed()` 加一个分支、`from_provider()` 加映射，约 30 行），随后同样回到配置文件模式使用。

> 维度一致性是硬约束：`embedding.dimensions` 必须等于模型真实输出，混用维度会导致
> 余弦相似度失真（向量空间机制会在切换时重嵌兜底，但同空间内维度不一致无法自愈）。

## 性能基准

测试环境：10000 条记忆数据，128 维向量，2000 条带向量。

| 操作 | Rust (release) | Python (原版) | 提升倍数 |
|------|---------------|--------------|---------|
| 批量插入 | 19,446 条/秒 | 2,372 条/秒 | **8.2x** |
| 单条查询 | 115,642 查询/秒 (0.009ms) | 7,424 查询/秒 (0.135ms) | **15.6x** |
| 列表查询 (20条) | 150 查询/秒 (6.66ms) | 57 查询/秒 (17.67ms) | **2.6x** |
| 统计查询 | 348 查询/秒 (2.87ms) | 206 查询/秒 (4.87ms) | **1.7x** |
| 向量检索 (top10) | 152 查询/秒 (6.58ms) | 334 查询/秒 (2.99ms) | **0.45x** ⚠️ |

### 关键发现

1. **写入和点查 Rust 优势巨大**（8-15 倍），无 GIL、零成本抽象、编译期优化的直接体现。
2. **列表/统计查询 Rust 领先 1.7-2.6 倍**，瓶颈在 SQLite I/O 而非语言层。
3. **向量检索 Python 反而更快**（2.2 倍）——Python 版用 `sqlite-vec` C 扩展做底层向量计算，Rust 版当前是纯内存余弦相似度。接入 `sqlite-vec` 或 SIMD 优化后可反超。
4. **内存占用**：Rust 二进制约 5MB，运行时内存 < 20MB；Python 进程基础内存约 30-50MB。
5. **启动时间**：Rust 冷启动 < 10ms；Python 解释器启动约 100-200ms。

### 复现基准测试

```bash
# Rust
cargo run --release --example bench

# Python（在 agent-memory 目录下）
python3 ../bench_python.py
```

## MCP 工具列表

### 记忆工具（9 个）

| 工具 | 说明 |
|------|------|
| `add_memory` | 添加记忆 |
| `get_memory` | 获取单条记忆 |
| `list_memories` | 列出记忆（分页/过滤） |
| `search_memories` | 向量语义搜索 |
| `update_memory` | 更新记忆 |
| `delete_memory` | 删除记忆 |
| `get_stats` | 统计信息 |
| `create_namespace` | 创建命名空间 |
| `list_namespaces` | 列出命名空间 |

### 图谱工具（11 个，需 `--features graph`）

| 工具 | 说明 |
|------|------|
| `graph_create_entity` | 创建实体 |
| `graph_get_entity` | 获取实体 |
| `graph_list_entities` | 列出实体 |
| `graph_delete_entity` | 删除实体 |
| `graph_create_relation` | 创建关系 |
| `graph_link_memory` | 关联记忆到实体 |
| `graph_query` | 多跳图查询 |
| `graph_aggregate` | 按属性聚合 |
| `graph_stats` | 图谱统计 |
| `graph_checkpoint` | 手动 checkpoint |

## ZCode 配置

在 `~/.zcode/cli/config.json` 的 `mcp.servers` 中添加。推荐 HTTP 方式（所有会话共用一个常驻进程，避免多个 stdio 进程争抢 `graph.kuzu`）：

```json
{
  "mcp": {
    "servers": {
      "agent-memory": {
        "url": "http://127.0.0.1:8888/mcp"
      }
    }
  }
}
```

stdio 方式（客户端按需拉起进程）：

```json
{
  "mcp": {
    "servers": {
      "agent-memory": {
        "command": "/path/to/agent-memory-server",
        "env": { "AGENT_MEMORY_GRAPH__ENABLED": "true" }
      }
    }
  }
}
```

> ⚠️ Kùzu 不支持多进程打开同一数据库目录。不要同时运行多个开启图谱的 stdio 会话，
> 也不要 stdio（开图谱）与 HTTP（开图谱）并存；统一走单进程 HTTP 常驻服务是推荐做法。

## 配置示例

```yaml
storage:
  db_path: ./data/memory.db
  embedding_dim: 128

server:
  transport: stdio          # stdio | http
  http_host: 127.0.0.1      # HTTP 绑定地址（非回环地址必须设 auth_token）
  http_port: 8888
  auth_token: null          # Bearer Token 鉴权，HTTP 对外暴露时必填

graph:
  enabled: false
  db_path: ./data/graph.db
  buffer_pool_size_mb: 256
  auto_extract: true        # store_memory 自动抽取实体入图谱（默认 true）
  backfill_on_start: true   # 启动时对存量记忆幂等回填图谱（默认 true）
  # rules:                  # 可选：追加抽取规则（内置词典之外）
  #   - label: System
  #     entity: MyApp
  #     keywords: ["myapp", "my-app"]
```

## 依赖

- `rusqlite` (bundled) — SQLite 嵌入式数据库
- `rmcp` 3.2 — MCP 协议实现
- `serde` / `serde_json` — 序列化
- `clap` — CLI 参数解析
- `tokio` — 异步运行时
- `kuzu` 0.11 (optional) — 嵌入式图数据库
- `tracing` — 日志

## License

MIT
