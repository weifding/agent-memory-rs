# 修改记录（agent-memory-rs）

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
