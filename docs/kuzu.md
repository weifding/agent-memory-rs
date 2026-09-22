# Kùzu 接入设计文档

> 适用范围：agent-memory-rs（Rust 实现）。权威约束见 `AGENTS.md`，本文是图存储层的
> 设计与平台差异速查。

## 版本

| 组件 | 版本 | 说明 |
|---|---|---|
| Rust crate | `kuzu 0.11`（Cargo.lock 锁定 **0.11.3**） | 自带 C++ 源码，构建期经 cmake 编译静态库 |
| Python 直读绑定 | kuzu **0.11.3**（与 Rust 侧同版本） | 仅调试/巡检用，见文末"直读" |

运行时**零外部依赖**：静态库随二进制分发，部署不装 Kùzu。

## Rust 读写依赖与 API

```toml
[dependencies]
kuzu = { version = "0.11", optional = true }

[features]
graph = ["kuzu"]
```

核心三件套（`src/graph.rs`）：

| 操作 | API | 要点 |
|---|---|---|
| 开库 | `kuzu::Database::new(path, SystemConfig)` | **即开即关**，不常驻连接；开库前先抢跨进程 flock |
| 写 | `Connection::query(Cypher)` + `CHECKPOINT` | 每次写后自动 CHECKPOINT 落盘；写前 MATCH 计数查重保幂等 |
| 读 | `Connection::query` + `set_query_timeout(5000)` | 只读校验（is_readonly）、最多返回 100 行 |

### 并发模型（多进程共存的关键）

Kùzu 禁止多进程同时打开同一数据库目录。解决方案是**队列化 + 即开即关**：

```
一次图谱操作的生命周期：
进程内 op_lock (Mutex) → graph.kuzu.lock 跨进程 flock（最长等 30s / 100ms 轮询）
→ Database::new 临时开库 → 执行 → 写操作补 CHECKPOINT → 关库放锁
```

- 服务常驻期间**不持有** Kùzu 连接，空闲期文件无人占用
- 因此 launchd 常驻 HTTP、stdio 多会话、手工直读可以共存
- `buffer_pool_size_mb` 默认 128（`graph.buffer_pool_size_mb`），限制复杂查询中间结果

### 读写入口

- 写路径：`execute()` —— 建实体 / 建关系 / link_memory / 删除等全部经它；
  幂等策略为"写前 MATCH 计数"（实体查重、关系查重、MemoryRef 查重）
- 读路径：`query()` —— `graph_query` 工具、聚合、统计、`store_memory` 自动入图、
  启动回填、删除联动清理全走它；经 `is_readonly` 校验
  （MATCH 开头 + 剥离字符串字面量后整词禁写关键字 + 分号分段校验）

### label 与动态扩展（提交 3b258ed 起）

- `validate_label`：任意 label 可扩展，但必须是合法 Cypher 标识符
  `[A-Za-z_][A-Za-z0-9_]*` —— 这是 label 直接拼 Cypher 的注入防线
- `ensure_node_table`：表不存在则建（`name`/`created_at` + 主键 name），
  属性缺失则 `ALTER TABLE ADD`，实现"表/列按需供给"
- 内置 `NODE_LABELS` 常量仍保留（9 类），仅作统计枚举与 extract 引擎参考
- 注意：`extract.rs` 自动入图仍按 `NODE_LABELS` 过滤，动态新类型需经
  `graph_create_entity` 手动创建（打通为配置化是候选后续）

## 平台差异

| | macOS | Windows |
|---|---|---|
| 编译工具链 | Apple Clang + Xcode CLT + cmake（brew install cmake），开箱即用 | MSVC（VS Build Tools 2019/2022，C++ 桌面开发 + CMake） |
| Kùzu 构建 | cmake 从源码编 C++（无预编译产物） | 同左（MSVC 工具链） |
| 产物 | `target/release/agent-memory-server` | `target\release\agent-memory-server.exe` |
| 已知坑 | launchd 从**外置盘**拉起会卡死在 dyld open()：二进制须放本地盘（`~/.agent-memory/bin/`） | ① graph 版**必须 `--release`**：debug 下 kuzu 静态库 ~4GB 超 ar 4GiB 上限，符号表损坏；② **kuzu 0.11 按主键等值匹配静默失配**（查重恒空 → 幂等失效产生重复节点），统一用 `(col + '') = 'value'` 表达式求值绕过（graph.rs `pk_eq`） |
| 常驻部署 | launchd（RunAtLoad + KeepAlive） | 计划任务 / 服务（未内置，社区可用 NSSM） |

CI：`.github/workflows/ci.yml` 在 macOS / Windows / Ubuntu ×（graph / no-graph）
六矩阵验证构建，Windows 行为以 CI 为准，本机无 Windows 也可保证可构建。

## 直读（调试/巡检）

Python 绑定同版本直读文件，**必须走 flock + read_only**，与服务端共存：

```python
import fcntl, kuzu
lock = open("/Users/<u>/.agent-memory/graph.kuzu.lock", "w")
fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
db = kuzu.Database("/Users/<u>/.agent-memory/graph.kuzu", read_only=True)
conn = kuzu.Connection(db)
print(conn.execute("MATCH (n:System) RETURN count(n)").get_next()[0])
db.close(); fcntl.flock(lock, fcntl.LOCK_UN)
```

工具化入口：`/tmp/kuzu-venv/bin/python`（kuzu 0.11.3 venv，重建：
`python3 -m venv /tmp/kuzu-venv && /tmp/kuzu-venv/bin/pip install kuzu`）。

## 运维要点

1. **单进程约束是最高优先级**：靠 flock 队列化已实现共存，但仍不要并行重负载
2. **定期 CHECKPOINT**：写路径已自动做；手动入口 `graph_checkpoint` 工具
3. **buffer pool**：默认 128MB，防多跳查询内存膨胀
4. **更新二进制流程**：`cargo build --release --features graph` →
   `cp target/release/agent-memory-server ~/.agent-memory/bin/` →
   `launchctl kickstart -k gui/$(id -u)/com.agent-memory.server`
