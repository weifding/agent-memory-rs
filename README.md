# agent-memory-rs

**Language: English | [中文](README.zh-CN.md)**

A from-scratch Rust rewrite of the agent-memory MCP server, using SQLite plus an
optional Kùzu knowledge graph as storage backends.

## Features

- **Memory storage**: SQLite persistence with namespace, category, importance,
  entities, topics and other metadata
- **Vector search**: pure-Rust cosine similarity with top-k semantic search
- **Embedding auto-detection**: at startup the server concurrently probes local
  OpenAI-compatible embedding services (LM Studio:1234 / Ollama:11434 /
  vLLM:8000 / Xinference:9997 / llama.cpp:8080, or `EMBEDDING_BASE_URL`), and on
  a hit automatically fills in provider/model/dimensions, replacing the built-in
  pseudo-embedding. When the vector space changes, all existing memories are
  re-embedded automatically. If nothing is detected it falls back to the built-in
  character-bigram pseudo embedding (zero external dependencies); setting
  `embedding.provider` explicitly skips probing
- **Knowledge graph** (optional): Kùzu embedded graph database with entity /
  relation / multi-hop query / property aggregation
- **Memories auto-linked into the graph** (optional): on `store_memory`, entities
  are extracted via the built-in/custom dictionary plus namespace skeleton and
  graph nodes with `*_MEMORY` edges are created; on startup existing memories are
  back-filled idempotently once
- **MCP protocol**: based on rmcp 3.2 with both stdio and HTTP (Streamable HTTP +
  SSE) transports; compatible with ZCode / Cursor / Claude Desktop and other MCP clients
- **HTTP auth**: `server.auth_token` enables a Bearer-token middleware; required
  when binding to a non-loopback address, otherwise startup is refused
- **Zero runtime dependencies**: SQLite embedded (rusqlite bundled), single-binary deploy

## Project structure

```
src/
├── lib.rs          # crate exports
├── main.rs         # CLI entry (stdio / HTTP MCP server)
├── config.rs       # configuration (YAML + env + clap)
├── models.rs       # data models (Memory / MemorySearchResult / MemoryStats)
├── storage.rs      # SQLite storage layer + vector search
├── embedding.rs    # embedding providers (auto-detect / openai / ollama / pseudo)
├── extract.rs      # rule-based entity extraction (dictionary + namespace skeleton)
├── graph.rs        # Kùzu graph module (feature = "graph")
└── server.rs       # MCP ServerHandler implementation
examples/
└── bench.rs        # performance benchmark
scripts/
├── build.sh        # macOS/Linux build entry
├── build.ps1       # Windows (MSVC) build entry
└── graph_viewer.py # web UI to browse the knowledge graph
```

## Build (cross-platform switches)

Universal commands (equivalent on all three platforms):

```bash
cargo build --release                  # base version (no graph)
cargo build --release --features graph # graph version (kuzu; needs per-OS toolchain, see table)
```

Build-time platform switches are handled by `build.rs`: when the graph feature is
enabled it checks the toolchain per target OS (cmake / CLT / GCC version / MSVC
environment) and prints a `cargo:warning` with install instructions at the
earliest stage — instead of failing deep inside cmake.

| Platform | Toolchain for the graph build | One-shot script | Notes |
|---|---|---|---|
| **macOS** (arm64/x86_64) | Xcode CLT (`xcode-select --install`) + cmake (`brew install cmake`); Apple clang builds kuzu directly, **no GCC 12 needed** | `./scripts/build.sh` | `--no-graph` memory-only; `--target` for cross builds |
| **Linux** (x86_64/aarch64) | GCC >= 12 (or clang >= 16) + cmake; glibc >= 2.28 | `./scripts/build.sh` | `--musl` for static linking on old distros; point CC/CXX at gcc-12 |
| **Windows** (x86_64 MSVC) | VS Build Tools 2019/2022 (C++ desktop + CMake); the script adds VS-bundled cmake/ninja to PATH | `.\scripts\build.ps1` | `-NoGraph` memory-only; `-Target` for cross (MinGW not recommended); the graph build **requires `--release`** (see note below) |

CI: `.github/workflows/ci.yml` builds and verifies macOS / Windows / Ubuntu ×
(graph / no-graph) — six combinations. The memory-only build (no graph) is pure
Rust with no platform toolchain requirements.

> kuzu 0.11.x's C++ dependencies use AVX-512 FP16 instructions; GCC 11 and below
> cannot compile it on Linux. Apple clang on macOS and MSVC on Windows are not
> affected.

> **The Windows graph build must use `--release`**: in debug mode kuzu's C++
> static library is ~4 GB and rustc packs it whole into the rlib (`+bundle` is
> the default for static native libs); once the archive exceeds the 4 GiB limit
> of ar's 32-bit offsets the symbol table corrupts and linking fails with masses
> of "unresolved external symbol" (MSVC link.exe) or a malformed archive
> (lld-link). Release kuzu.lib is ~1 GB and unaffected; CI and build.ps1 both use
> release.

## Run

```bash
# stdio mode (default; spawned by the MCP client)
./target/release/agent-memory-server

# HTTP mode (resident service; endpoint is POST http://<host>:<port>/mcp)
./target/release/agent-memory-server --transport http --host 127.0.0.1 --port 8888

# Expose HTTP publicly (auth_token is mandatory)
AGENT_MEMORY_SERVER__AUTH_TOKEN=<secret> ./target/release/agent-memory-server --transport http --host 0.0.0.0 --port 8888

# Custom config file
./target/release/agent-memory-server --config config.yaml

# Help
./target/release/agent-memory-server --help
```

> Note: running the stdio mode directly in a terminal exits immediately (no
> JSON-RPC client on stdin) — that is expected. In HTTP mode each session gets
> its own handler from the service factory while storage and the Kùzu graph are
> shared via `Arc`; the service remains a single process with a single
> connection, satisfying the Kùzu embedded single-process constraint.

### macOS resident deployment (launchd)

After installing `~/Library/LaunchAgents/com.agent-memory.server.plist`:

```bash
launchctl load ~/Library/LaunchAgents/com.agent-memory.server.plist   # start (auto-start at boot, restart on crash)
launchctl kickstart -k gui/$(id -u)/com.agent-memory.server           # manual restart (run after updating the binary)
launchctl unload ~/Library/LaunchAgents/com.agent-memory.server.plist # stop
tail -f ~/.agent-memory/server.log                                    # logs
```

Key plist settings: `RunAtLoad` + `KeepAlive=true`, env
`AGENT_MEMORY_GRAPH__ENABLED=true`, stderr goes to
`~/.agent-memory/server.log`. Deploy the binary on a **local** disk (e.g.
`~/.agent-memory/bin/`) — launchd-spawned processes may hang in `dyld open()`
when the executable lives on an external volume; after editing the plist you
must `launchctl unload + load` (`kickstart` alone keeps the old job definition).

## Embedding

### How it works

`store_memory` / `search_memory` both depend on text vectors. The server picks
the embedding source by the following priority (see `src/embedding.rs`):

1. **Explicit config**: `embedding.provider` set in the config file or env →
   used as-is, no probing
2. **Auto-detection**: at startup the server concurrently probes local
   OpenAI-compatible endpoints (LM Studio:1234 / Ollama:11434 / vLLM:8000 /
   Xinference:9997 / llama.cpp:8080, or `EMBEDDING_BASE_URL`), picks a model via
   `GET /v1/models` then measures dimensions with `POST /v1/embeddings`
3. **Pseudo-embedding fallback**: if nothing is detected, a built-in character
   bigram hash embedding is used (zero external dependencies; lexical similarity
   works, semantic generalization is weak)

**Vector-space consistency**: the current space is recorded in
`kv_meta.embedding_space` (e.g. `openai:bge-m3@1024` or `pseudo-bigram@1536`).
When the provider or dimensions change, all existing memories are re-embedded
automatically at startup, keeping old and new vectors comparable — switching
between with/without an embedding service needs no manual migration.

### Usage

```bash
# Option 1: zero config — if LM Studio / Ollama etc. is running locally it is picked up automatically

# Option 2: point at a non-default port/address via env
EMBEDDING_BASE_URL=http://127.0.0.1:9527 ./agent-memory-server --transport http

# Option 3: explicit config file (skips probing; config errors surface at call time for easy diagnosis)
```

```yaml
embedding:
  provider: openai                 # non-empty enables explicit mode
  base_url: http://127.0.0.1:1234  # missing /v1 is appended automatically
  model: bge-m3
  api_key: ""                      # can stay empty for local services
  dimensions: 1024                 # must match the model's real output
```

> The dependency tree deliberately excludes TLS: the embedding client only
> speaks `http://` (local-service scenario). To use an https cloud API
> (OpenAI / SiliconFlow etc.) run a local reverse proxy (e.g.
> `caddy reverse-proxy --from 127.0.0.1:1234 to api.siliconflow.cn`) and point
> option 3 at it.

### Adding more embedding backends (config-file mode)

**Provider selection is entirely config-file driven**; two protocols are built
in and new endpoints need no code:

**Option A: OpenAI-compatible services (`provider: openai`, default protocol)** —
anything exposing `POST /v1/embeddings` can be configured in: cloud APIs behind a
local reverse proxy, self-hosted inference (`infinity`,
`text-embeddings-inference`), LM Studio / vLLM, etc. For auto-detection the model
picker (`pick_model()`) prefers names containing `embed`/`bge`/`gte`/`e5`/`wemm`.

**Option B: Ollama native protocol (`provider: ollama`)** — uses Ollama's own
`POST /api/embed {"model","input":[..]}` batch endpoint (bypassing its OpenAI
compatibility layer):

```yaml
embedding:
  provider: ollama        # select the native protocol, skip auto-detection
  base_url: http://127.0.0.1:11434   # empty → http://127.0.0.1:11434 (no /v1 appended)
  model: bge-m3
  dimensions: 1024        # must match the model's real output
```

Env equivalent: `AGENT_MEMORY_EMBEDDING__PROVIDER=ollama`.

Both options share the same vector-space mechanism: space ids are
`openai:<model>@<dim>` / `ollama:<model>@<dim>`; switching between
openai/ollama/pseudo and restarting re-embeds existing memories automatically.

**Only a brand-new wire protocol requires code** (add a variant to the
`Protocol` enum in `embedding.rs`, a branch in `embed()`, and a mapping in
`from_provider()` — about 30 lines), after which it is again config-file driven.

> Dimension consistency is a hard constraint: `embedding.dimensions` must equal
> the model's real output; mixing dimensions distorts cosine similarity (the
> vector-space mechanism re-embeds on switches as a safety net, but inconsistent
> dimensions inside one space cannot self-heal).

### Case study: integrating WeChat WeMM-Embedding-2B (2026-09-20)

A complete record of a real-embedding integration; use it as a template for the
same workflow.

**Target model**: [WeMM-Embedding-2B](https://github.com/) — open-sourced in
Aug 2026 by the WeChat Vision Team at Tencent; a multimodal embedding model
built on Qwen3.5 (2048-dim L2-normalized vectors for text input).

**Steps (three total)**:

```bash
# 1. Start the Ollama service (brew install; launchd-resident)
brew install ollama && brew services start ollama

# 2. Pull the model (1.6GB)
ollama pull milkey/wemm-embedding-2b

# 3. Done. No configuration needed — restarting the memory server auto-detects it:
#    INFO embedding: embedding server auto-detected
#      base_url=http://127.0.0.1:11434/v1 model=milkey/wemm-embedding-2b:latest dimensions=2048
#    INFO embedding: re-embedding existing memories done=32 → 64 → 75
#    INFO embedding: vector space changed; re-embedded space=openai:milkey/wemm-embedding-2b:latest@2048
```

**Before/after** (pseudo embedding → real 2048-dim embeddings):

| Query | Top hit (similarity) | Notes |
|---|---|---|
| "how do I control kids' tablet usage" | tablet-control memory (0.478) | **Zero keyword overlap**, pure semantic hit; impossible with pseudo embeddings |
| "knowledge graph visualization page" | graph-viewer memory (0.537) | Same |

**Three issues found and fixed along the way** (all merged; you won't hit them
repeating this flow):

1. **Probe cold-load misdetection**: the probe's embeddings test timed out at
   600ms while a 2B model's first inference takes seconds → misdetection
   "not found" and fallback to pseudo embeddings. Fix: `/models` discovery keeps
   600ms fast-fail, but once a service is found the embeddings probe is relaxed
   to 30s (`PROBE_EMBED_TIMEOUT`).
2. **Re-embed timeout caused a crash loop**: existing-memory re-embedding used
   500-per-request + 30s timeout, which a 2B model cannot satisfy → setup failed
   → launchd restart loop. Fix: batches of 32 + 600s timeout; failures only warn
   and do not block startup, retried on the next restart (the space marker is
   only updated after a fully successful re-embed).
3. **launchd spawn hung in dyld open()**: launching the binary from an external
   volume (/Volumes/...) repeatedly hung the launchd-spawned process during
   dynamic linking (terminal runs were completely fine; sampling pinned it at
   `dyld3::open`). Fix: deploy the binary on a local disk
   (`~/.agent-memory/bin/`) and point the plist there.
   **Note: after editing a plist you must `launchctl unload + load`;**
   **`kickstart` alone keeps using the old job definition.**

**Verification checklist** (run through after any integration):

- [ ] Startup log shows `auto-detected` and `re-embedded`
- [ ] `kv_meta`.`embedding_space` updated to the new space id
- [ ] A paraphrased semantic query returns similarity ≥ 0.4 with expected hits
- [ ] New `store_memory` works (automatically using the real embedding), no embedding errors

## Performance benchmarks

Test environment: 10,000 memories, 128-dim vectors, 2,000 with vectors.

| Operation | Rust (release) | Python (original) | Speedup |
|------|---------------|--------------|---------|
| Batch insert | 19,446 rec/s | 2,372 rec/s | **8.2x** |
| Point query | 115,642 q/s (0.009ms) | 7,424 q/s (0.135ms) | **15.6x** |
| List query (20 rows) | 150 q/s (6.66ms) | 57 q/s (17.67ms) | **2.6x** |
| Stats query | 348 q/s (2.87ms) | 206 q/s (4.87ms) | **1.7x** |
| Vector search (top10) | 152 q/s (6.58ms) | 334 q/s (2.99ms) | **0.45x** ⚠️ |

### Key findings

1. **Rust dominates writes and point queries** (8–15x) — no GIL, zero-cost
   abstraction, compile-time optimization.
2. **List/stats queries lead by 1.7–2.6x** — the bottleneck is SQLite I/O, not
   the language layer.
3. **Vector search was faster in Python** (2.2x) — the Python version uses the
   `sqlite-vec` C extension for the heavy lifting while Rust did pure in-memory
   cosine similarity. With real embedding models and/or SIMD this gap closes.
4. **Memory footprint**: the Rust binary is ~5MB with < 20MB RSS; the Python
   process idles at 30–50MB.
5. **Startup**: Rust cold-start < 10ms; Python interpreter ~100–200ms.

### Reproducing the benchmarks

```bash
# Rust
cargo run --release --example bench

# Python (in the agent-memory directory)
python3 ../bench_python.py
```

## MCP tools

### Memory tools (7)

| Tool | Description |
|------|------|
| `store_memory` | Store a memory (content capped at 10,000 chars, truncated beyond; entities auto-extracted into the graph) |
| `search_memory` | Semantic vector search (Chinese-friendly, ranked by similarity) |
| `get_memory` | Fetch one memory |
| `update_memory` | Update a memory (content changes re-embed automatically) |
| `delete_memory` | Delete a memory (also cleans the graph MemoryRef) |
| `list_memories` | List memories (pagination/filtering) |
| `get_memory_stats` | Statistics |

### Graph tools (11; requires `--features graph` and `graph.enabled=true`)

| Tool | Description |
|------|------|
| `graph_create_entity` | Create an entity |
| `graph_get_entity` | Get an entity |
| `graph_list_entities` | List entities |
| `graph_delete_entity` | Delete an entity |
| `graph_create_relation` | Create a relation (idempotent) |
| `graph_link_memory` | Link a memory to an entity |
| `graph_get_related_memories` | Reverse-lookup memories linked to an entity |
| `graph_query` | Read-only Cypher multi-hop queries |
| `graph_aggregate` | Aggregate by property |
| `graph_stats` | Graph statistics |
| `graph_checkpoint` | Manual CHECKPOINT |

## ZCode configuration

Add to `mcp.servers` in `~/.zcode/cli/config.json`. The HTTP mode is recommended
(all sessions share one resident process, avoiding multiple stdio processes
fighting over `graph.kuzu`):

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

stdio mode (process spawned on demand by the client):

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

> ⚠️ Kùzu does not allow multiple processes to open the same database directory.
> Do not run multiple graph-enabled stdio sessions at once, nor mix stdio (graph
> on) with HTTP (graph on); a single-process resident HTTP service is the
> recommended setup.

## DSH (DeepSeek Harness) integration

Bridge via `@deepseek-ai/dsh-mcp-client`; tools appear in DSH sessions as
`mcp__agent-memory__<tool>` (e.g. `mcp__agent-memory__search_memory`).
**streamable-http pointed at the resident 8888 service is recommended**: one
process owns Kùzu and the warm embedding model, and all DSH sessions share the
same memory.

Add to the MCP section of `cordis.yml`:

```yaml
- id: mcp-agent-memory
  name: '@deepseek-ai/dsh-mcp-client'
  config:
    serverName: agent-memory
    transport: streamable-http
    url: http://127.0.0.1:8888/mcp
    # headers:                          # only when auth_token is enabled
    #   Authorization: 'Bearer <token>'
    toolCallTimeoutMs: 60000
```

stdio fallback (each DSH session spawns its own process; the embedding model
cold-loads per session and Kùzu queues through flock — only recommended if you
do not want a resident service):

```yaml
- id: mcp-agent-memory
  name: '@deepseek-ai/dsh-mcp-client'
  config:
    serverName: agent-memory
    transport: stdio
    command: /Users/dingweifeng/.agent-memory/bin/agent-memory-server
    env:
      AGENT_MEMORY_GRAPH__ENABLED: 'true'
```

Integration notes:

- **Protocol compatibility**: the server's Streamable HTTP (stateless + inline
  JSON) has been verified end-to-end with the official TS SDK; `tools/list`
  responses include the SEP-2549 required fields (`ttlMs`/`cacheScope`) so
  Zod-based clients (DSH/Cursor etc.) parse them fine
- **Prerequisites**: the 8888 resident service is running
  (`launchctl list | grep agent-memory`); for public exposure set
  `AGENT_MEMORY_SERVER__AUTH_TOKEN` and send the Bearer token in `headers`
- **DSH-side crash isolation**: MCP plugin failures do not affect the memory
  service; DSH's reconnect logic (default 10 backoff attempts) covers service
  restart windows; MCP `resources` are not bridged by DSH (all core capability
  lives in the 18 tools, so no impact)

## Configuration example

```yaml
storage:
  db_path: ./data/memory.db
  embedding_dim: 128

embedding:
  provider: openai          # empty → auto-detect; openai | ollama
  base_url: http://127.0.0.1:1234
  model: bge-m3
  api_key: null
  dimensions: 1024

server:
  transport: stdio          # stdio | http
  http_host: 127.0.0.1      # bind address (non-loopback requires auth_token)
  http_port: 8888
  auth_token: null          # Bearer token; required when exposing HTTP publicly

graph:
  enabled: false
  db_path: ./data/graph.db
  buffer_pool_size_mb: 256
  auto_extract: true        # store_memory auto-extracts entities into the graph (default true)
  backfill_on_start: true   # idempotent graph back-fill of existing memories on startup (default true)
  # rules:                  # optional: extra extraction rules (beyond the built-in dictionary)
  #   - label: System
  #     entity: MyApp
  #     keywords: ["myapp", "my-app"]
```

## Dependencies

- `rusqlite` (bundled) — embedded SQLite
- `rmcp` 3.2 — MCP protocol implementation
- `serde` / `serde_json` — serialization
- `clap` — CLI argument parsing
- `tokio` — async runtime
- `hyper` / `hyper-util` / `futures` — embedding HTTP client (local, no TLS)
- `kuzu` 0.11 (optional) — embedded graph database
- `tracing` — logging

## License

MIT
