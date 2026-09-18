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
/// 跨进程等锁超时（毫秒）：操作队列化后，排队等前一个进程完成图谱操作
const LOCK_WAIT_TIMEOUT_MS: u64 = 30_000;
/// 等锁轮询间隔（毫秒）
const LOCK_POLL_INTERVAL_MS: u64 = 100;

/// 允许的节点类型
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
            conn.query(cypher)
                .with_context(|| format!("Cypher write failed: {}", cypher))?;
            conn.query("CHECKPOINT").context("CHECKPOINT failed")?;
            Ok(())
        })
    }

    /// 执行只读查询，返回 (列名, 行数据)
    pub fn query(&self, cypher: &str) -> Result<(Vec<String>, Vec<Vec<kuzu::Value>>)> {
        if !is_readonly(cypher) {
            return Err(anyhow!(
                "Only read-only queries (MATCH ... RETURN) are allowed"
            ));
        }
        self.with_session(|conn| {
            // 复杂多跳查询超时保护（对应设计文档 5s 超时）
            conn.set_query_timeout(QUERY_TIMEOUT_MS);
            let result = conn.query(cypher)?;
            let columns = result.get_column_names();
            let rows: Vec<Vec<kuzu::Value>> = result.collect();
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
    if !NODE_LABELS.contains(&label) {
        return Err(anyhow!("Invalid node label: {}", label));
    }

    // 检查是否已存在
    let check = format!(
        "MATCH (n:{} {{name: '{}'}}) RETURN count(n) AS cnt",
        label,
        escape_str(name)
    );
    let (_, rows) = db.query(&check)?;
    if let Some(first) = rows.first() {
        if let Some(kuzu::Value::Int64(cnt)) = first.first() {
            if *cnt > 0 {
                return Ok("already_exists".to_string());
            }
        }
    }

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

pub fn get_entity(db: &GraphDB, label: &str, name: &str) -> Result<Option<String>> {
    let cypher = format!(
        "MATCH (n:{} {{name: '{}'}}) RETURN n.name AS name",
        label,
        escape_str(name)
    );
    let (_, rows) = db.query(&cypher)?;
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
    let cypher = format!("MATCH (n:{}) RETURN n.name AS name LIMIT {}", label, limit);
    let (_, rows) = db.query(&cypher)?;
    let mut names = Vec::new();
    for row in rows {
        if let Some(kuzu::Value::String(n)) = row.first() {
            names.push(n.clone());
        }
    }
    Ok(names)
}

pub fn delete_entity(db: &GraphDB, label: &str, name: &str) -> Result<bool> {
    if get_entity(db, label, name)?.is_none() {
        return Ok(false);
    }
    let cypher = format!(
        "MATCH (n:{} {{name: '{}'}}) DETACH DELETE n",
        label,
        escape_str(name)
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
    let valid = RELATIONS.iter().any(|(r, s, t)| {
        *r == relation && *s == source_label && *t == target_label
    });
    if !valid {
        return Err(anyhow!(
            "Invalid relation {} for {} -> {}",
            relation,
            source_label,
            target_label
        ));
    }

    // MemoryRef 用 memory_id 匹配
    let target_match = if target_label == "MemoryRef" {
        format!("{{memory_id: '{}'}}", escape_str(target_name))
    } else {
        format!("{{name: '{}'}}", escape_str(target_name))
    };

    // 幂等：同 (源,关系,目标) 已存在则不再建边，防重复 store / 回填产生重复边
    let exists_cypher = format!(
        "MATCH (s:{} {{name: '{}'}})-[r:{}]->(t:{} {}) RETURN count(r) AS cnt",
        source_label,
        escape_str(source_name),
        relation,
        target_label,
        target_match
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
        "MATCH (s:{} {{name: '{}'}}), (t:{} {}) CREATE (s)-[r:{}]->(t)",
        source_label,
        escape_str(source_name),
        target_label,
        target_match,
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
    let relation = rel_map
        .iter()
        .find(|(l, _)| *l == entity_label)
        .map(|(_, r)| *r)
        .ok_or_else(|| anyhow!("link_memory not supported for label: {}", entity_label))?;

    // 创建 MemoryRef（幂等）
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
        let now = chrono::Utc::now().to_rfc3339();
        let create = format!(
            "CREATE (m:MemoryRef {{memory_id: '{}', content_preview: '{}', created_at: '{}'}})",
            escape_str(memory_id),
            escape_str(&content_preview.chars().take(200).collect::<String>()),
            now
        );
        db.execute(&create)?;
    }

    create_relation(db, relation, entity_label, entity_name, "MemoryRef", memory_id)
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
        "MATCH (m:MemoryRef {{memory_id: '{}'}}) DETACH DELETE m",
        escape_str(memory_id)
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
    for label in NODE_LABELS {
        let cypher = format!("MATCH (n:{}) RETURN count(n) AS cnt", label);
        if let Ok((_, rows)) = db.query(&cypher) {
            if let Some(row) = rows.first() {
                if let Some(kuzu::Value::Int64(c)) = row.first() {
                    stats.insert(label.to_string(), *c);
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
    if !NODE_LABELS.contains(&label) {
        return Err(anyhow!("Invalid node label: {}", label));
    }
    let relation = RELATIONS
        .iter()
        .find(|(_, s, t)| *s == label && *t == "MemoryRef")
        .map(|(r, _, _)| *r)
        .ok_or_else(|| anyhow!("label {} 无 memory 关联关系", label))?;
    let cypher = format!(
        "MATCH (e:{} {{name: '{}'}})-[:{}]->(m:MemoryRef) RETURN m.memory_id AS memory_id",
        label,
        escape_str(name),
        relation
    );
    let (_, rows) = db.query(&cypher)?;
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
    let upper = cypher.trim().to_uppercase();
    if !upper.starts_with("MATCH") && !upper.starts_with("OPTIONAL MATCH") {
        return false;
    }
    for kw in ["CREATE", "DELETE", "SET", "DROP", "MERGE", "INSERT", "UPDATE", "ALTER", "COPY"] {
        if upper.contains(kw) {
            return false;
        }
    }
    true
}

fn escape_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// 属性键经反引号包裹，保证中文/特殊字符键名在 Cypher 中合法
fn escape_key(k: &str) -> String {
    k.replace('\\', "\\\\").replace('`', "\\`")
}
