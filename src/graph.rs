//! Kùzu 知识图谱模块 — 嵌入式原生图数据库
#![cfg(feature = "graph")]

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use kuzu::{Connection, Database, SystemConfig};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 只读查询超时（毫秒），防止复杂多跳查询挂死
const QUERY_TIMEOUT_MS: u64 = 5000;
/// 只读查询返回行数上限（与 Python 版 execute_readonly 的 100 行对齐）
const QUERY_MAX_ROWS: usize = 100;
/// 跨进程等锁超时（毫秒）：操作队列化后，排队等前一个进程完成图谱操作
const LOCK_WAIT_TIMEOUT_MS: u64 = 30_000;
/// 等锁轮询间隔（毫秒）
const LOCK_POLL_INTERVAL_MS: u64 = 100;

/// 预设节点类型（启动时建表，列结构固定）；运行期允许任意新 label——
/// 首次写入时自动建表（name/created_at 基础列），新属性键自动 ALTER ADD
/// STRING 列。MemoryRef 为记忆关联的技术节点，由内部维护、不对工具开放。
pub const NODE_LABELS: &[&str] = &[
    "Hospital", "System", "Interface", "FaultCase", "Company", "Region", "Project", "Family",
    "MemoryRef",
];

/// 允许的关系类型 → (源Label, 目标Label)
pub const RELATIONS: &[(&str, &str, &str)] = &[
    ("DEPLOY", "Hospital", "System"),
    ("CONNECT", "System", "Interface"),
    ("FAULT_CASE", "Interface", "FaultCase"),
    ("LOCATED_IN", "Hospital", "Region"),
    ("OWNED_BY", "System", "Company"),
    ("HAS_PROJECT", "Hospital", "Project"),
    ("HOSPITAL_MEMORY", "Hospital", "MemoryRef"),
    ("SYSTEM_MEMORY", "System", "MemoryRef"),
    ("INTERFACE_MEMORY", "Interface", "MemoryRef"),
    ("FAULT_MEMORY", "FaultCase", "MemoryRef"),
    ("COMPANY_MEMORY", "Company", "MemoryRef"),
    ("PROJECT_MEMORY", "Project", "MemoryRef"),
    ("REGION_MEMORY", "Region", "MemoryRef"),
    ("FAMILY_MEMORY", "Family", "MemoryRef"),
];

/// 图谱数据库句柄。
///
/// Kùzu 同一目录不允许两个进程同时打开（文件锁互斥），而 ZCode 会为每个
/// 会话拉起一个服务进程。因此不做常驻连接，而是把图谱操作队列化：
/// 每个操作先取进程内互斥锁，再取 `<db_path>.lock` 上的跨进程 flock，
/// 然后临时打开数据库执行，写操作补 CHECKPOINT，最后关库放锁。
/// 进程空闲期间不持有 Kùzu，多进程可共存。
pub struct GraphDB {
    db_path: PathBuf,
    buffer_pool_size_mb: usize,
    op_lock: Mutex<()>,
}

impl GraphDB {
    /// 仅记录路径与参数并确保目录存在，不打开 Kùzu（不会因锁冲突失败）
    pub fn open(db_path: &Path, buffer_pool_size_mb: usize) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        Ok(Self {
            db_path: db_path.to_path_buf(),
            buffer_pool_size_mb,
            op_lock: Mutex::new(()),
        })
    }

    fn lock_file_path(&self) -> PathBuf {
        let mut name = self.db_path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".lock");
        self.db_path.parent().unwrap_or_else(|| Path::new("")).join(name)
    }

    /// 队列化执行一个图谱操作：进程内互斥 → 跨进程 flock → 临时开库 → 闭包执行 → 关库
    fn with_session<T>(&self, f: impl FnOnce(&Connection<'_>) -> Result<T>) -> Result<T> {
        let _in_process = self
            .op_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let lock_path = self.lock_file_path();
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("无法创建图谱锁文件: {:?}", lock_path))?;

        let deadline = Instant::now() + Duration::from_millis(LOCK_WAIT_TIMEOUT_MS);
        loop {
            if lock_file.try_lock_exclusive().is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "等待图谱操作锁超时（{:?} 被其他进程长期占用）",
                    lock_path
                ));
            }
            std::thread::sleep(Duration::from_millis(LOCK_POLL_INTERVAL_MS));
        }

        // 拿到锁后执行；无论成败都要释放（drop 关闭文件即解锁）
        let outcome = (|| {
            let config = SystemConfig::default()
                .buffer_pool_size((self.buffer_pool_size_mb as u64) * 1024 * 1024);
            let db = Database::new(&self.db_path, config)
                .with_context(|| format!("Failed to open Kùzu: {:?}", self.db_path))?;
            let conn = Connection::new(&db).map_err(|e| anyhow!("Kùzu 连接失败: {}", e))?;
            f(&conn)
        })();
        drop(lock_file);
        outcome
    }

    /// 执行写操作（同一会话内追加 CHECKPOINT，保证关库前落盘）
    pub fn execute(&self, cypher: &str) -> Result<()> {
        self.with_session(|conn| {
            conn.query(cypher).map_err(|e| {
                anyhow!("Cypher write failed: {} — 原因: {}", cypher, e)
            })?;
            conn.query("CHECKPOINT").context("CHECKPOINT failed")?;
            Ok(())
        })
    }

    /// 执行只读查询，返回 (列名, 行数据)。行数上限 QUERY_MAX_ROWS（与 Python 版 100 行对齐）
    pub fn query(&self, cypher: &str) -> Result<(Vec<String>, Vec<Vec<kuzu::Value>>)> {
        if !is_readonly(cypher) {
            return Err(anyhow!(
                "Only read-only queries (MATCH ... RETURN) are allowed"
            ));
        }
        self.raw_query(cypher)
    }

    /// 内部专用：CALL 系统表函数（TABLE_INFO/SHOW_TABLES）不走 MATCH 门禁。
    /// 仅限本模块硬编码的调用，禁止用于任何工具/用户输入。
    fn raw_query(&self, cypher: &str) -> Result<(Vec<String>, Vec<Vec<kuzu::Value>>)> {
        self.with_session(|conn| {
            // 复杂多跳查询超时保护（对应设计文档 5s 超时）
            conn.set_query_timeout(QUERY_TIMEOUT_MS);
            let mut result = conn.query(cypher)?;
            let columns = result.get_column_names();
            let rows: Vec<Vec<kuzu::Value>> = result
                .by_ref()
                .take(QUERY_MAX_ROWS)
                .collect();
            if rows.len() == QUERY_MAX_ROWS {
                tracing::warn!(target: "graph", rows = rows.len(), "query result truncated at QUERY_MAX_ROWS");
            }
            Ok((columns, rows))
        })
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.with_session(|conn| {
            conn.query("CHECKPOINT").context("CHECKPOINT failed")?;
            Ok(())
        })
    }
}

/// 初始化 Schema（幂等）
pub fn init_schema(db: &GraphDB) -> Result<()> {
    // 节点表
    let node_tables = [
        ("Hospital", "name STRING, industry STRING, region STRING, level STRING, created_at STRING", "name"),
        ("System", "name STRING, vendor STRING, version STRING, created_at STRING", "name"),
        ("Interface", "name STRING, code STRING, protocol STRING, created_at STRING", "name"),
        ("FaultCase", "title STRING, name STRING, severity STRING, status STRING, created_at STRING", "title"),
        ("Company", "name STRING, industry STRING, created_at STRING", "name"),
        ("Region", "name STRING, province STRING, city STRING, created_at STRING", "name"),
        ("Project", "name STRING, status STRING, created_at STRING", "name"),
        ("Family", "name STRING, note STRING, created_at STRING", "name"),
        ("MemoryRef", "memory_id STRING, content_preview STRING, created_at STRING", "memory_id"),
    ];

    for (name, cols, pk) in node_tables {
        let cypher = format!(
            "CREATE NODE TABLE IF NOT EXISTS {}({}, PRIMARY KEY ({}))",
            name, cols, pk
        );
        if let Err(e) = db.execute(&cypher) {
            if !e.to_string().to_lowercase().contains("exist") {
                return Err(e);
            }
        }
    }

    // 关系表
    for (rel, src, dst) in RELATIONS {
        let cypher = format!("CREATE REL TABLE IF NOT EXISTS {}(FROM {} TO {})", rel, src, dst);
        if let Err(e) = db.execute(&cypher) {
            if !e.to_string().to_lowercase().contains("exist") {
                return Err(e);
            }
        }
    }

    // FaultCase 迁移：旧库该表 PK 为 title 且无 name 列，与实体 CRUD 的 name 约定不一致。
    // 幂等补 name 列（CREATE 侧已保证新装库含 name）；"已存在"类错误忽略。
    let mut fault_altered = false;
    for stmt in [
        "ALTER TABLE FaultCase ADD name STRING",
        "ALTER NODE TABLE FaultCase ADD name STRING",
    ] {
        match db.execute(stmt) {
            Ok(_) => {
                fault_altered = true;
                break;
            }
            Err(e) => {
                let m = e.to_string().to_lowercase();
                if m.contains("exist") || m.contains("duplicate") || m.contains("already") {
                    fault_altered = true; // 列已存在，视为完成
                    break;
                }
            }
        }
    }
    if !fault_altered {
        tracing::warn!("FaultCase name column migration skipped (schema may need manual check)");
    }

    Ok(())
}

// ------------------------------------------------------------------
// 实体操作
// ------------------------------------------------------------------

pub fn create_entity(
    db: &GraphDB,
    label: &str,
    name: &str,
    properties: &std::collections::HashMap<String, String>,
) -> Result<String> {
    validate_label(label)?;
    if label == "MemoryRef" {
        return Err(anyhow!("MemoryRef 是记忆关联的内部节点，禁止作为实体 label 创建"));
    }
    if name.is_empty() {
        return Err(anyhow!("entity name is required"));
    }

    // 幂等检查（表不存在时视为不存在）
    let check = format!(
        "MATCH (n:{}) WHERE {} RETURN count(n) AS cnt",
        label,
        pk_eq("n.name", name)
    );
    let exists = match db.query(&check) {
        Ok((_, rows)) => rows
            .first()
            .and_then(|r| r.first())
            .map(|v| matches!(v, kuzu::Value::Int64(c) if *c > 0))
            .unwrap_or(false),
        Err(e) if is_missing_table(&e) => false,
        Err(e) => return Err(e),
    };
    if exists {
        return Ok("already_exists".to_string());
    }

    // 表与属性列按需供给：预设外 label 自动建表，未定义属性键自动加列
    let prop_keys: Vec<String> = properties.keys().cloned().collect();
    ensure_node_table(db, label, &prop_keys)?;

    let now = chrono::Utc::now().to_rfc3339();
    let mut props = properties.clone();
    props.insert("created_at".to_string(), now);
    // FaultCase 主键为 title：与其它节点用 name 做唯一键的约定对齐
    if label == "FaultCase" {
        props.insert("title".to_string(), name.to_string());
    }

    let kv: Vec<String> = props
        .iter()
        .map(|(k, v)| format!("`{}`: '{}'", escape_key(k), escape_str(v)))
        .collect();
    let props_clause = if kv.is_empty() {
        String::new()
    } else {
        format!(", {}", kv.join(", "))
    };

    let cypher = format!(
        "CREATE (n:{} {{name: '{}'{}}})",
        label,
        escape_str(name),
        props_clause
    );
    db.execute(&cypher)?;
    Ok("created".to_string())
}

/// label 校验：任意 label 可扩展，但必须是合法 Cypher 标识符
///（`[A-Za-z_][A-Za-z0-9_]*`）——这是把 label 直接拼进 Cypher 的注入防线
fn validate_label(label: &str) -> Result<()> {
    let ok = !label.is_empty()
        && label.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_').unwrap_or(false)
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(anyhow!(
            "Invalid node label: {} (label 须为 [A-Za-z_][A-Za-z0-9_]* 标识符)",
            label
        ));
    }
    Ok(())
}

/// 属性名白名单（标识符字符集），防 group_by 类参数注入
fn validate_property_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_').unwrap_or(false)
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(anyhow!("Invalid property name: {}", name));
    }
    Ok(())
}

/// 错误是否源于表不存在（get/list/delete 对未建表的 label 应返回空而非报错）
fn is_missing_table(e: &anyhow::Error) -> bool {
    let s = format!("{:#}", e).to_lowercase();
    s.contains("not exist") || s.contains("doesn't exist") || s.contains("non-existent")
}

/// 查询 label 的现有列名；表不存在时返回 None
fn table_columns(db: &GraphDB, label: &str) -> Result<Option<Vec<String>>> {
    match db.raw_query(&format!("CALL TABLE_INFO('{}') RETURN *", label)) {
        Ok((columns, rows)) => {
            let name_idx = columns.iter().position(|c| c.eq_ignore_ascii_case("name"));
            let cols = match name_idx {
                Some(i) => rows
                    .iter()
                    .filter_map(|r| r.get(i).and_then(|v| match v {
                        kuzu::Value::String(s) => Some(s.clone()),
                        _ => None,
                    }))
                    .collect::<Vec<_>>(),
                None => rows.iter().filter_map(|r| r.first()).filter_map(|v| match v {
                    kuzu::Value::String(s) => Some(s.clone()),
                    _ => None,
                }).collect(),
            };
            Ok(Some(cols))
        }
        Err(e) if is_missing_table(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// 确保节点表存在并补齐属性列：
/// - 预设外的 label 首次使用时自动建表（name STRING + created_at STRING，PK=name）
/// - 已有表缺属性键时 ALTER TABLE ADD <key> STRING（Kùzu 动态列，值统一按字符串存）
pub fn ensure_node_table(db: &GraphDB, label: &str, prop_keys: &[String]) -> Result<()> {
    validate_label(label)?;
    let existing = table_columns(db, label)?;
    match existing {
        Some(cols) => {
            for k in prop_keys {
                if !cols.iter().any(|c| c == k) {
                    validate_property_name(k)?;
                    db.execute(&format!("ALTER TABLE {} ADD {} STRING", label, k))?;
                }
            }
        }
        None => {
            db.execute(&format!(
                "CREATE NODE TABLE IF NOT EXISTS {} (name STRING, created_at STRING, PRIMARY KEY(name))",
                label
            ))?;
            for k in prop_keys {
                if k != "name" && k != "created_at" {
                    validate_property_name(k)?;
                    db.execute(&format!("ALTER TABLE {} ADD {} STRING", label, k))?;
                }
            }
        }
    }
    Ok(())
}

pub fn get_entity(db: &GraphDB, label: &str, name: &str) -> Result<Option<String>> {
    validate_label(label)?;
    let cypher = format!(
        "MATCH (n:{}) WHERE {} RETURN n.name AS name",
        label,
        pk_eq("n.name", name)
    );
    let (_, rows) = match db.query(&cypher) {
        Ok(r) => r,
        Err(e) if is_missing_table(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    if rows.is_empty() {
        return Ok(None);
    }
    Ok(Some(name.to_string()))
}

pub fn list_entities(
    db: &GraphDB,
    label: &str,
    limit: usize,
) -> Result<Vec<String>> {
    validate_label(label)?;
    let cypher = format!("MATCH (n:{}) RETURN n.name AS name LIMIT {}", label, limit.min(1000));
    let (_, rows) = match db.query(&cypher) {
        Ok(r) => r,
        Err(e) if is_missing_table(&e) => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut names = Vec::new();
    for row in rows {
        if let Some(kuzu::Value::String(n)) = row.first() {
            names.push(n.clone());
        }
    }
    Ok(names)
}

pub fn delete_entity(db: &GraphDB, label: &str, name: &str) -> Result<bool> {
    validate_label(label)?;
    if get_entity(db, label, name)?.is_none() {
        return Ok(false);
    }
    let cypher = format!(
        "MATCH (n:{}) WHERE {} DETACH DELETE n",
        label,
        pk_eq("n.name", name)
    );
    db.execute(&cypher)?;
    Ok(true)
}

// ------------------------------------------------------------------
// 关系操作
// ------------------------------------------------------------------

pub fn create_relation(
    db: &GraphDB,
    relation: &str,
    source_label: &str,
    source_name: &str,
    target_label: &str,
    target_name: &str,
) -> Result<String> {
    let relation = relation.to_uppercase();
    // 预设关系：三元组精确匹配；预设外的 (关系, 源, 目标) 走按需建关系表
    let preset = RELATIONS.iter().any(|(r, s, t)| {
        *r == relation && *s == source_label && *t == target_label
    });
    if !preset {
        validate_label(source_label)?;
        validate_label(target_label)?;
        validate_property_name(&relation)?;
        // 与预设关系同名的 (源,目标) 组合不允许重定义，避免语义混淆
        if RELATIONS.iter().any(|(r, _, _)| *r == relation) {
            return Err(anyhow!(
                "Invalid relation {} for {} -> {} (预设关系端点固定)",
                relation,
                source_label,
                target_label
            ));
        }
        ensure_node_table(db, source_label, &[])?;
        ensure_node_table(db, target_label, &[])?;
        db.execute(&format!(
            "CREATE REL TABLE IF NOT EXISTS {} (FROM {} TO {})",
            relation, source_label, target_label
        ))?;
    }

    // MemoryRef 用 memory_id 匹配
    let target_eq = if target_label == "MemoryRef" {
        pk_eq("t.memory_id", target_name)
    } else {
        pk_eq("t.name", target_name)
    };

    // 幂等：同 (源,关系,目标) 已存在则不再建边，防重复 store / 回填产生重复边
    let exists_cypher = format!(
        "MATCH (s:{})-[r:{}]->(t:{}) WHERE {} AND {} RETURN count(r) AS cnt",
        source_label,
        relation,
        target_label,
        pk_eq("s.name", source_name),
        target_eq
    );
    let (_, rows) = db.query(&exists_cypher)?;
    if let Some(first) = rows.first() {
        if let Some(kuzu::Value::Int64(cnt)) = first.first() {
            if *cnt > 0 {
                return Ok("already_exists".to_string());
            }
        }
    }

    let cypher = format!(
        "MATCH (s:{}), (t:{}) WHERE {} AND {} CREATE (s)-[r:{}]->(t)",
        source_label,
        target_label,
        pk_eq("s.name", source_name),
        target_eq,
        relation
    );
    db.execute(&cypher)?;
    Ok("created".to_string())
}

pub fn link_memory(
    db: &GraphDB,
    entity_label: &str,
    entity_name: &str,
    memory_id: &str,
    content_preview: &str,
) -> Result<String> {
    // 预设 label 用固定关系名；预设外 label 按需建实体表与 {LABEL}_MEMORY 关系表
    let rel_map = [
        ("Hospital", "HOSPITAL_MEMORY"),
        ("System", "SYSTEM_MEMORY"),
        ("Interface", "INTERFACE_MEMORY"),
        ("FaultCase", "FAULT_MEMORY"),
        ("Company", "COMPANY_MEMORY"),
        ("Project", "PROJECT_MEMORY"),
        ("Region", "REGION_MEMORY"),
        ("Family", "FAMILY_MEMORY"),
    ];
    validate_label(entity_label)?;
    let relation: String = match rel_map.iter().find(|(l, _)| *l == entity_label) {
        Some((_, r)) => (*r).to_string(),
        None => {
            if entity_label == "MemoryRef" {
                return Err(anyhow!("MemoryRef 不参与实体-记忆关联"));
            }
            let rel = format!("{}_MEMORY", entity_label.to_uppercase());
            ensure_node_table(db, entity_label, &[])?;
            db.execute(&format!(
                "CREATE REL TABLE IF NOT EXISTS {} (FROM {} TO MemoryRef)",
                rel, entity_label
            ))?;
            rel
        }
    };

    // 创建 MemoryRef（幂等）
    let check = format!(
        "MATCH (m:MemoryRef) WHERE {} RETURN count(m) AS cnt",
        pk_eq("m.memory_id", memory_id)
    );
    let (_, rows) = db.query(&check)?;
    let exists = rows
        .first()
        .and_then(|r| r.first())
        .map(|v| matches!(v, kuzu::Value::Int64(c) if *c > 0))
        .unwrap_or(false);

    if !exists {
        let now = chrono::Utc::now().to_rfc3339();
        let create = format!(
            "CREATE (m:MemoryRef {{memory_id: '{}', content_preview: '{}', created_at: '{}'}})",
            escape_str(memory_id),
            escape_str(&content_preview.chars().take(200).collect::<String>()),
            now
        );
        db.execute(&create)?;
    }

    create_relation(db, &relation, entity_label, entity_name, "MemoryRef", memory_id)
}

/// 删除记忆引用节点及其全部入边（delete_memory 联动清理，防孤儿 MemoryRef）
pub fn delete_memory_ref(db: &GraphDB, memory_id: &str) -> Result<bool> {
    let check = format!(
        "MATCH (m:MemoryRef {{memory_id: '{}'}}) RETURN count(m) AS cnt",
        escape_str(memory_id)
    );
    let (_, rows) = db.query(&check)?;
    let exists = rows
        .first()
        .and_then(|r| r.first())
        .map(|v| matches!(v, kuzu::Value::Int64(c) if *c > 0))
        .unwrap_or(false);
    if !exists {
        return Ok(false);
    }
    let cypher = format!(
        "MATCH (m:MemoryRef) WHERE {} DETACH DELETE m",
        pk_eq("m.memory_id", memory_id)
    );
    db.execute(&cypher)?;
    Ok(true)
}

// ------------------------------------------------------------------
// 聚合统计
// ------------------------------------------------------------------

pub fn aggregate_by_property(
    db: &GraphDB,
    label: &str,
    group_by: &str,
) -> Result<Vec<(String, i64)>> {
    validate_label(label)?;
    validate_property_name(group_by)?;
    let cypher = format!(
        "MATCH (n:{}) RETURN n.{} AS key, count(*) AS cnt ORDER BY cnt DESC",
        label, group_by
    );
    let (_, rows) = db.query(&cypher)?;
    let mut result = Vec::new();
    for row in rows {
        let key = match row.first() {
            Some(kuzu::Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let count = match row.get(1) {
            Some(kuzu::Value::Int64(c)) => *c,
            _ => 0,
        };
        result.push((key, count));
    }
    Ok(result)
}

pub fn count_entities(db: &GraphDB) -> Result<std::collections::HashMap<String, i64>> {
    let mut stats = std::collections::HashMap::new();
    // 动态枚举全部节点表（含运行期自动供给的 label）；SHOW_TABLES 失败时回退预设清单
    let labels: Vec<String> = match db.raw_query("CALL SHOW_TABLES() RETURN *") {
        Ok((columns, rows)) => {
            let name_i = columns.iter().position(|c| c.eq_ignore_ascii_case("name"));
            let type_i = columns.iter().position(|c| c.eq_ignore_ascii_case("type"));
            tracing::debug!(target: "graph", ?columns, "SHOW_TABLES columns");
            let mut ls = Vec::new();
            for r in &rows {
                let get = |i: Option<usize>| -> Option<String> {
                    i.and_then(|i| r.get(i)).and_then(|v| match v {
                        kuzu::Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                };
                let is_node = match get(type_i) {
                    Some(t) => t.eq_ignore_ascii_case("node"),
                    None => true, // 无 type 列时不过滤
                };
                if is_node {
                    if let Some(n) = get(name_i) {
                        ls.push(n);
                    }
                }
            }
            if ls.is_empty() {
                tracing::debug!(target: "graph", "SHOW_TABLES 解析为空，回退预设 label 清单");
                NODE_LABELS.iter().map(|s| s.to_string()).collect()
            } else {
                ls
            }
        }
        Err(e) => {
            tracing::debug!(target: "graph", error = %e, "SHOW_TABLES 失败，回退预设 label 清单");
            NODE_LABELS.iter().map(|s| s.to_string()).collect()
        }
    };
    for label in labels {
        let cypher = format!("MATCH (n:{}) RETURN count(n) AS cnt", label);
        if let Ok((_, rows)) = db.query(&cypher) {
            if let Some(row) = rows.first() {
                if let Some(kuzu::Value::Int64(c)) = row.first() {
                    stats.insert(label, *c);
                }
            }
        }
    }
    Ok(stats)
}

// ------------------------------------------------------------------
// 记忆关联
// ------------------------------------------------------------------

/// 获取与实体关联的记忆 ID 列表（通过 *_MEMORY 关系链接到 MemoryRef）
pub fn get_related_memories(db: &GraphDB, label: &str, name: &str) -> Result<Vec<String>> {
    validate_label(label)?;
    // 预设 label 走固定关系；预设外 label 的记忆关联关系为 {LABEL}_MEMORY（link_memory 供给）
    let relation: String = match RELATIONS
        .iter()
        .find(|(_, s, t)| *s == label && *t == "MemoryRef")
        .map(|(r, _, _)| *r)
    {
        Some(r) => r.to_string(),
        None => format!("{}_MEMORY", label.to_uppercase()),
    };
    let cypher = format!(
        "MATCH (e:{})-[:{}]->(m:MemoryRef) WHERE {} RETURN m.memory_id AS memory_id",
        label,
        relation,
        pk_eq("e.name", name)
    );
    let (_, rows) = match db.query(&cypher) {
        Ok(r) => r,
        Err(e) if is_missing_table(&e) => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut ids = Vec::new();
    for row in rows {
        if let Some(kuzu::Value::String(s)) = row.first() {
            ids.push(s.clone());
        }
    }
    Ok(ids)
}

// ------------------------------------------------------------------
// 工具函数
// ------------------------------------------------------------------

fn is_readonly(cypher: &str) -> bool {
    let stripped = strip_string_literals(cypher);
    // 多语句（分号分隔）逐段校验，末尾空段忽略
    for segment in stripped.split(';') {
        let seg = segment.trim();
        if seg.is_empty() {
            continue;
        }
        let upper = seg.to_uppercase();
        let is_match = upper.starts_with("OPTIONAL MATCH") || upper.starts_with("MATCH");
        if !is_match {
            return false;
        }
        // 整词匹配禁用关键字（避免 created_at/updated_at 这类子串误伤）
        let mut token = String::new();
        for c in upper.chars().chain(std::iter::once(' ')) {
            if c.is_ascii_alphanumeric() || c == '_' {
                token.push(c);
            } else {
                if FORBIDDEN_KEYWORDS.contains(&token.as_str()) {
                    return false;
                }
                token.clear();
            }
        }
    }
    true
}

/// 写/DDL/过程调用关键字（按词匹配，不含字符串字面量内的内容）
const FORBIDDEN_KEYWORDS: &[&str] = &[
    "CREATE", "DELETE", "DETACH", "SET", "DROP", "MERGE", "INSERT", "UPDATE", "ALTER", "COPY",
    "REMOVE", "CALL", "LOAD", "FOREACH",
];

/// 剥离单引号字符串字面量的内容（保留引号本身），使关键字扫描只作用于语法部分。
/// 处理 \' 与 '' 两种转义。
fn strip_string_literals(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            match c {
                '\\' => {
                    chars.next();
                }
                '\'' => {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        in_str = false;
                        out.push('\'');
                    }
                }
                _ => {}
            }
        } else {
            if c == '\'' {
                in_str = true;
            }
            out.push(c);
        }
    }
    out
}

fn escape_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Kùzu 0.11.3（MSVC 构建）主键列等值匹配静默失配：`{name:'x'}` 与
/// `WHERE name='x'` 走哈希索引路径，查不到已存在节点（全表扫描路径正常，
/// 非主键列等值也正常，MemoryRef.memory_id 恰好可用属例外表现）。实测
/// `(col + '') = 'value'` 强制表达式求值后全部命中。统一经此函数做
/// 主键等值匹配，升级 kuzu 修复后可整体替换回普通等值。
fn pk_eq(col: &str, value: &str) -> String {
    format!("({} + '') = '{}'", col, escape_str(value))
}

/// 属性键经反引号包裹，保证中文/特殊字符键名在 Cypher 中合法
fn escape_key(k: &str) -> String {
    k.replace('\\', "\\\\").replace('`', "\\`")
}
