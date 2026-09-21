# 修改记录（agent-memory-rs）

## 2026-09-21 — 图谱 label 自由扩展 + graph_create_entity 写入故障修复

### 故障一：graph_create_entity 带"最简属性"报 Cypher write failed

根因：节点表列结构在 `init_schema` 写死，`CREATE (n:Label {...})` 带任何
未预定义的属性键即被 Kùzu 拒绝（如 System 传 `{foo:'bar'}`）。

### 故障二（连带发现，更严重）：主键列等值匹配静默失配

实测（kuzu 0.11.3 MSVC 构建）：`MATCH (n:Label {name:'x'})` 与
`WHERE n.name='x'` 走哈希索引路径，**查不到已存在的节点**（全表扫描正常、
非主键列等值正常）。后果：get_entity 永远 not_found、create_entity 幂等
检查失效（重复创建报错）、create_relation/link_memory 幂等检查失效
（静默重复建边）、get_related_memories 恒空。`MemoryRef.memory_id`
恰好能命中属例外表现，掩盖了问题。

### 修复

1. **label 白名单 → 标识符校验**：`validate_label` 改为 `[A-Za-z_][A-Za-z0-9_]*`
   检查（保留 Cypher 注入防线），任意新 label 可扩展。`NODE_LABELS` 降级为
   "预设清单"（仅 init_schema 建表 + SHOW_TABLES 失败时兜底）。
2. **表结构按需供给**：`ensure_node_table`——预设外 label 首次写入自动
   `CREATE NODE TABLE (name STRING, created_at STRING, PK name)`；已有表
   缺属性键时 `ALTER TABLE ADD <key> STRING`（属性值统一按字符串存）。
   新增内部专用 `raw_query`（CALL 系统表函数不走 MATCH 门禁）。
3. **主键等值统一走 `(col + '') = 'value'`**（`pk_eq` 辅助函数）：强制表达式
   求值绕开失配的索引路径。get/create/delete/relation/link/related 全部
   8 处等值查询改写。升级 kuzu 修复后可整体还原。
4. **关系按需供给**：`create_relation` 预设三元组外自动
   `CREATE REL TABLE IF NOT EXISTS <REL> (FROM s TO t)`（预设关系名端点
   固定不可重定义）；`link_memory`/`get_related_memories` 对预设外 label
   自动用 `{LABEL}_MEMORY` 关系。
5. **杂项**：`graph_stats` 动态枚举全部节点表（`CALL SHOW_TABLES() RETURN *`，
   注意必须带括号）；`execute` 错误带上 Kùzu 底层原因；工具 handler 属性值
   支持数字/布尔（转字符串），null 丢弃；`MemoryRef` 对工具保留。
   历史遗留影响说明：修复前的 graph_linked 计数与重复建边防重不可靠，
   升级后同 (源,关系,目标) 重复边不再增长（存量重复边如需清理可
   `graph_query` 排查后手工处理）。

### 验证（19/19 全通过）

新 label 创建/幂等、预设 label 新属性键（原故障场景）、CJK/ASCII 实体
get、MemoryRef 保留、注入 label 与非法标识符拒绝、store 自动入图、
动态 label link/关联查询、全新动态关系 + 幂等、端点冲突报错、stats 含
动态 label、delete、search 回归。

## 2026-09-19 — 修复 discover 生命周期 tools/list 被 Zod 拒收（ttlMs/cacheScope 缺失）

### 现象

ZCode 2026-07-28 discover 生命周期的会话挂载失败：`initialize`/请求本身正常，
`tools/list` 返回被客户端 Zod schema 整单拒收（`Invalid result for tools/list`），
错误 path 为 `ttlMs`（expected number, received undefined）与 `cacheScope`
（expected "public"|"private"）。四个工作区会话同样失败；走 initialize
老生命周期的其他 agent 客户端不受影响。

### 根因

SEP-2549：协议 2026-07-28 起，分页列表结果（tools/list 等）的 `ttlMs`、
`cacheScope` 为**必填**字段。rmcp `paginated_result!` 宏里两者是
`Option` + `skip_serializing_if`，服务端构造时设 `None` → 字段整体缺失；
新协议客户端严格校验必填 → 拒收。老协议 schema 无此字段（passthrough 容忍）→ 无感。

### 修复

1. `src/server.rs`：`list_tools` / `list_resources` / `list_resource_templates`
   补 `ttl_ms: Some(300_000)`、`cache_scope: Some(CacheScope::Private)`。
2. `vendor/rmcp`（延续已有补丁）：`strip_result_type_for_legacy_peer` 扩展为
   对老协议 peer 同时剥掉列表结果的 `ttlMs`/`cacheScope`（这两个字段仅在
   2026-07-28+ 定义）——老协议响应与修复前逐字节一致，其他 agent 零影响。

### 验证（HTTP 实测双路径）

- discover 生命周期（`_meta` 内联 2026-07-28 + `MCP-Protocol-Version`/
  `MCP-Method` 头）：tools/list 返回 `ttlMs:300000, cacheScope:"private",
  resultType:"complete"`，严格校验 PASS；resources/list 同；tools/call 冒烟通过。
- 老协议（initialize 2025-06-18）：tools/list 顶层仅 `tools` 键，无新增字段 PASS。

## 2026-09-18 — Embedding 自动检测与真实向量接入

### 背景

语义检索此前用"字符 bigram 哈希伪嵌入"（`pseudo_embedding`）凑合：本质是表面字符
重合度，不懂同义改写。config 里的 `embedding.provider/base_url/model` 字段一直是摆设。

### 实现

1. **新增 `src/embedding.rs`**
   - OpenAI 兼容 `/v1/embeddings` 客户端（hyper-util legacy client，仅 http；
     依赖树刻意不带 TLS 栈，本地服务场景够用，云端走本地反代）
   - 启动时并发探测本地服务：LM Studio:1234 / Ollama:11434 / vLLM:8000 /
     Xinference:9997 / llama.cpp:8080，`EMBEDDING_BASE_URL` 环境变量可指定
   - 模型挑选偏好 WeMM > 含 embed 字样 > bge/gte/e5；探测时试算真实维度
     并回填 `embedding.*` 配置
2. **向量空间一致性**（`kv_meta.embedding_space`）
   - 空间标识 `openai:<model>@<dim>` / `pseudo-bigram@<dim>`，启动时比对，
     变化即自动重嵌全部存量记忆（分批 500 条），避免新旧向量混用导致相似度失真
3. **接线**：`MemoryHandler` 增加 `embedding` 字段；store/search/update 三处
   调用点统一走 `embed_text`（服务器在线时真实嵌入，离线时伪嵌入；真实嵌入
   调用失败直接报错而非静默回退，防止污染向量空间）

### 验证

三阶段实测（Windows + mock OpenAI 兼容服务器 @1234）：未检测到→伪嵌入回退；
mock 在线→自动检测命中 `WeMM-Embedding-2B@64` + 存量重嵌 + 读写走真实向量；
mock 下线→自动回退并重嵌回伪嵌入空间。

## 2026-09-18 — Windows 构建适配

- `scripts/build.ps1` 加 UTF-8 BOM（修复 PowerShell 5.1 中文解析失败），
  自动补齐 VS 自带 cmake/ninja 到 PATH；README 记录 debug+graph 的 4GiB rlib 限制
- 实测 VS2019 BuildTools + rustc 1.93 可完成 release+graph 编译（19min）

## 2026-09-08（晚）— 修复 ZCode MCP 挂载失败（未提交）

### 背景

重启 ZCode 后 agent-memory 无法挂载，日志（`~/.zcode/cli/log/`）显示两类失败：
1. ZCode 为每个会话各拉起一个服务进程，多个进程同时打开 `graph.kuzu` 触发 Kùzu 文件锁冲突；
2. ZCode 0.16.5 的 stdio 客户端走 MCP 2026-07-28 discover 生命周期（无 initialize 握手、
   `_meta` 内联协商版本），要求 `tools/list` 结果必须含 `resultType`，而服务器响应里没有，
   连接被拒绝（`Invalid result for tools/list: missing required resultType`）。

### 修复

1. **vendor rmcp 并打补丁**
   - 将 rmcp 3.2.0 源码复制到 `vendor/rmcp`，`Cargo.toml` 增加
     `[patch.crates-io] rmcp = { path = "vendor/rmcp" }`
   - 补丁位置：`vendor/rmcp/src/handler/server.rs` 的 `sep_2322_supported`。
     原逻辑只看 peer 信息里的协商版本，discover 模式下 peer 信息缺失导致
     `resultType` 被误剥；补丁在 peer 缺失时回退到请求 `_meta` 的内联协议版本判断。
   - **升级 rmcp 时需重新评估此补丁**（若官方已修复可移除 vendor 目录与 patch 段）。

2. **服务器 Result 构造补上 resultType**
   - `src/server.rs`：`list_tools` / `list_resources` / `list_resource_templates`
     三处 `result_type: None` → `Some(ResultType::COMPLETE)`。
   - 旧版协议对端仍会被 rmcp 的 `strip_result_type_for_legacy_peer` 自动剥掉，
     新旧两条路径均符合规范。

3. **Kùzu 并发锁：图谱操作队列化**
   - `src/graph.rs` 重构 `GraphDB`：
     - `open()` 只记录路径与参数、建目录，**不再常驻打开 Kùzu**，启动永不因锁冲突失败；
     - 每个图谱操作（`execute` / `query` / `checkpoint`）经 `with_session` 队列化执行：
       进程内 `Mutex` → `graph.kuzu.lock` 文件 flock（跨进程，30s 超时、100ms 轮询）→
       临时开库 → 执行 → 写操作后追加 `CHECKPOINT` → 关库放锁；
     - 进程空闲期间不持有 Kùzu，多进程/多会话可共存，图谱操作跨进程排队。
   - 新增依赖：`fs2 = "0.4"`（文件锁）。
   - 代价：每个图谱操作重新打开一次 Kùzu（毫秒级），换取多进程安全。

### 验证

- 3 个进程并发 `graph_create_entity` 全部成功（此前必现锁冲突崩溃），测试数据已清理；
- discover 路径（`_meta` 内联 2026-07-28）`tools/list` 返回 `resultType=complete` + 18 个工具；
- legacy 路径（initialize 2025-11-25）`tools/list` 正确不含 `resultType`；
- 中文语义搜索、`graph_stats`、写后 CHECKPOINT 落盘均正常。

## 2026-09-08（早）— 此前未提交改动

- `src/graph.rs`：新增 `escape_key`（反引号包裹属性键），修复中文/特殊字符键名
  生成非法 Cypher 的 bug。
- 新增跨平台构建脚本 `scripts/build.sh`（macOS/Linux）与 `scripts/build.ps1`（Windows，
  MSVC + VS Build Tools 2022）。
- 图谱功能（`System —OWNED_BY→ Company`）、`graph_query` 只读拦截、中文搜索召回、
  跨进程持久化已实测通过。
