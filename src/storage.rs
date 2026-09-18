//! SQLite 存储层 — 记忆 CRUD + 向量语义检索

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::models::{Memory, MemorySearchResult, MemoryStats, Namespace};

pub struct SQLiteStorage {
    conn: Mutex<Connection>,
    embedding_dim: usize,
}

impl SQLiteStorage {
    pub fn open(db_path: &Path, embedding_dim: usize) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("Failed to open SQLite: {:?}", db_path))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        let storage = Self {
            conn: Mutex::new(conn),
            embedding_dim,
        };
        storage.init_schema()?;
        Ok(storage)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                namespace TEXT NOT NULL DEFAULT 'default',
                content TEXT NOT NULL,
                summary TEXT,
                entities TEXT DEFAULT '[]',
                topics TEXT DEFAULT '[]',
                category TEXT DEFAULT 'fact',
                importance REAL DEFAULT 0.5,
                connections TEXT DEFAULT '[]',
                consolidated INTEGER DEFAULT 0,
                source TEXT DEFAULT 'mcp',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memories_namespace ON memories(namespace);
            CREATE INDEX IF NOT EXISTS idx_memories_updated_at ON memories(updated_at);

            CREATE TABLE IF NOT EXISTS memory_vectors (
                id TEXT PRIMARY KEY,
                embedding BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS namespaces (
                name TEXT PRIMARY KEY,
                description TEXT DEFAULT '',
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tombstones (
                id TEXT PRIMARY KEY,
                namespace TEXT NOT NULL DEFAULT 'default',
                deleted_at TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // 记忆 CRUD
    // ------------------------------------------------------------------

    pub fn store(&self, memory: &Memory, embedding: Option<&[f32]>) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "INSERT OR IGNORE INTO namespaces (name, description, created_at) VALUES (?, '', ?)",
            params![memory.namespace, memory.created_at],
        )?;

        tx.execute(
            r#"INSERT OR REPLACE INTO memories
               (id, namespace, content, summary, entities, topics, category,
                importance, connections, consolidated, source, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            params![
                memory.id,
                memory.namespace,
                memory.content,
                memory.summary,
                serde_json::to_string(&memory.entities)?,
                serde_json::to_string(&memory.topics)?,
                memory.category,
                memory.importance,
                serde_json::to_string(&memory.connections)?,
                memory.consolidated as i32,
                memory.source,
                memory.created_at,
                memory.updated_at,
            ],
        )?;

        if let Some(emb) = embedding {
            let blob = f32_vec_to_bytes(emb);
            tx.execute(
                "INSERT OR REPLACE INTO memory_vectors (id, embedding) VALUES (?, ?)",
                params![memory.id, blob],
            )?;
        }

        tx.commit()?;
        Ok(memory.id.clone())
    }

    pub fn get(&self, id: &str) -> Result<Option<Memory>> {
        let conn = self.conn.lock().unwrap();
        let mem = conn
            .query_row("SELECT * FROM memories WHERE id = ?", params![id], row_to_memory)
            .optional()?;
        Ok(mem)
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        // 事务化：memories / vectors / tombstones 三步原子完成，防中途失败丢墓碑
        let tx = conn.unchecked_transaction()?;
        let namespace: Option<String> = tx
            .query_row("SELECT namespace FROM memories WHERE id = ?", params![id], |r| {
                r.get(0)
            })
            .optional()?;

        let affected = tx.execute("DELETE FROM memories WHERE id = ?", params![id])?;
        tx.execute("DELETE FROM memory_vectors WHERE id = ?", params![id])?;

        if affected > 0 {
            let now = chrono::Utc::now().to_rfc3339();
            tx.execute(
                "INSERT OR REPLACE INTO tombstones (id, namespace, deleted_at) VALUES (?, ?, ?)",
                params![id, namespace.unwrap_or_else(|| "default".to_string()), now],
            )?;
            tx.commit()?;
            return Ok(true);
        }
        tx.commit()?;
        Ok(false)
    }

    /// 更新记忆（可选字段），content 变化时需传入 new_embedding 同步更新向量
    pub fn update(
        &self,
        memory_id: &str,
        content: Option<&str>,
        importance: Option<f64>,
        category: Option<&str>,
        new_embedding: Option<&[f32]>,
    ) -> Result<Option<Memory>> {
        let conn = self.conn.lock().unwrap();
        let exists: Option<String> = conn
            .query_row(
                "SELECT id FROM memories WHERE id = ?",
                params![memory_id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(None);
        }

        let mut sets: Vec<String> = vec!["updated_at = ?".to_string()];
        let mut vals: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(chrono::Utc::now().to_rfc3339())];
        if let Some(c) = content {
            sets.push("content = ?".to_string());
            vals.push(Box::new(c.to_string()));
        }
        if let Some(i) = importance {
            sets.push("importance = ?".to_string());
            vals.push(Box::new(i));
        }
        if let Some(cat) = category {
            sets.push("category = ?".to_string());
            vals.push(Box::new(cat.to_string()));
        }

        let sql = format!("UPDATE memories SET {} WHERE id = ?", sets.join(", "));
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            vals.iter().map(|b| b.as_ref() as &dyn rusqlite::ToSql).collect();
        let mut final_params: Vec<&dyn rusqlite::ToSql> = param_refs;
        final_params.push(&memory_id);
        stmt.execute(final_params.as_slice())?;
        drop(stmt);

        if let Some(emb) = new_embedding {
            let blob = f32_vec_to_bytes(emb);
            conn.execute(
                "INSERT OR REPLACE INTO memory_vectors (id, embedding) VALUES (?, ?)",
                params![memory_id, blob],
            )?;
        }
        drop(conn);

        self.get(memory_id)
    }

    /// 列表查询（namespace 可为 None = 全部命名空间）
    pub fn list_filtered(
        &self,
        namespace: Option<&str>,
        category: Option<&str>,
        limit: i32,
        offset: i32,
    ) -> Result<Vec<Memory>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from("SELECT * FROM memories WHERE 1=1");
        let mut params_vec: Vec<String> = Vec::new();
        if let Some(ns) = namespace {
            sql.push_str(" AND namespace = ?");
            params_vec.push(ns.to_string());
        }
        if let Some(cat) = category {
            sql.push_str(" AND category = ?");
            params_vec.push(cat.to_string());
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ? OFFSET ?");
        params_vec.push(limit.to_string());
        params_vec.push(offset.to_string());

        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(param_refs.as_slice(), row_to_memory)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn list(
        &self,
        namespace: &str,
        limit: i32,
        offset: i32,
        category: Option<&str>,
    ) -> Result<Vec<Memory>> {
        self.list_filtered(Some(namespace), category, limit, offset)
    }

    // ------------------------------------------------------------------
    // 向量语义检索
    // ------------------------------------------------------------------

    pub fn search(
        &self,
        query_embedding: &[f32],
        namespace: Option<&str>,
        top_k: usize,
        category: Option<&str>,
    ) -> Result<Vec<MemorySearchResult>> {
        let conn = self.conn.lock().unwrap();

        // 单次 JOIN 查询获取向量和记忆全部字段，避免 N+1 和死锁
        let mut sql = String::from(
            "SELECT m.id, m.namespace, m.content, m.summary, m.entities, m.topics, \
             m.category, m.importance, m.connections, m.consolidated, m.source, \
             m.created_at, m.updated_at, v.embedding \
             FROM memory_vectors v JOIN memories m ON m.id = v.id WHERE 1=1",
        );
        let mut conditions: Vec<String> = Vec::new();
        if let Some(ns) = namespace {
            conditions.push(format!("m.namespace = '{}'", escape_sql(ns)));
        }
        if let Some(cat) = category {
            conditions.push(format!("m.category = '{}'", escape_sql(cat)));
        }
        if !conditions.is_empty() {
            sql.push_str(" AND ");
            sql.push_str(&conditions.join(" AND "));
        }

        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<(Memory, Vec<f32>)> = stmt
            .query_map([], |row| {
                let memory = row_to_memory(row)?;
                let blob: Vec<u8> = row.get(13)?;
                Ok((memory, bytes_to_f32_vec(&blob)))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        if rows.is_empty() {
            return Ok(vec![]);
        }

        // 计算余弦相似度
        let mut scored: Vec<(f64, Memory)> = rows
            .into_iter()
            .map(|(mem, emb)| {
                let sim = cosine_similarity(query_embedding, &emb);
                (sim, mem)
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);

        Ok(scored
            .into_iter()
            .map(|(sim, mem)| MemorySearchResult {
                memory: mem,
                similarity: sim,
            })
            .collect())
    }

    // ------------------------------------------------------------------
    // 统计
    // ------------------------------------------------------------------

    pub fn get_stats(&self, namespace: Option<&str>) -> Result<MemoryStats> {
        let conn = self.conn.lock().unwrap();

        let (where_clause, ns_param) = if let Some(ns) = namespace {
            ("WHERE namespace = ?".to_string(), Some(ns.to_string()))
        } else {
            (String::new(), None)
        };

        let total: i64 = {
            let sql = format!("SELECT COUNT(*) FROM memories {}", where_clause);
            let mut stmt = conn.prepare(&sql)?;
            if let Some(ns) = &ns_param {
                stmt.query_row(params![ns], |r| r.get(0))?
            } else {
                stmt.query_row([], |r| r.get(0))?
            }
        };

        let mut by_category = HashMap::new();
        {
            let sql = format!("SELECT category, COUNT(*) FROM memories {} GROUP BY category", where_clause);
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<(String, i64)> = if let Some(ns) = &ns_param {
                stmt.query_map(params![ns], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?.collect::<Result<Vec<_>, _>>()?
            } else {
                stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?.collect::<Result<Vec<_>, _>>()?
            };
            for (k, v) in rows {
                by_category.insert(k, v);
            }
        }

        let unconsolidated: i64 = {
            let sql = if where_clause.is_empty() {
                "SELECT COUNT(*) FROM memories WHERE consolidated = 0".to_string()
            } else {
                format!("SELECT COUNT(*) FROM memories {} AND consolidated = 0", where_clause)
            };
            let mut stmt = conn.prepare(&sql)?;
            if let Some(ns) = &ns_param {
                stmt.query_row(params![ns], |r| r.get(0))?
            } else {
                stmt.query_row([], |r| r.get(0))?
            }
        };

        let mut stmt = conn.prepare("SELECT name FROM namespaces ORDER BY name")?;
        let namespaces: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(MemoryStats {
            total_memories: total,
            by_category,
            unconsolidated_count: unconsolidated,
            namespaces,
        })
    }

    pub fn list_namespaces(&self) -> Result<Vec<Namespace>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT name, description, created_at FROM namespaces ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(Namespace {
                name: r.get(0)?,
                description: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 向量维度（用于生成查询向量，须与写入维度一致）
    pub fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }

    /// 库中是否已存在向量（决定 search 走向量检索还是文本回退）
    pub fn has_vectors(&self) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM memory_vectors", [], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// 更新或插入一条记忆的向量嵌入
    pub fn update_embedding(&self, id: &str, embedding: &[f32]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let blob = f32_vec_to_bytes(embedding);
        conn.execute(
            "INSERT OR REPLACE INTO memory_vectors (id, embedding) VALUES (?, ?)",
            params![id, blob],
        )?;
        Ok(())
    }
}

// ------------------------------------------------------------------
// 辅助函数
// ------------------------------------------------------------------

fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    let entities_str: String = row.get("entities")?;
    let topics_str: String = row.get("topics")?;
    let connections_str: String = row.get("connections")?;

    Ok(Memory {
        id: row.get("id")?,
        namespace: row.get("namespace")?,
        content: row.get("content")?,
        summary: row.get("summary")?,
        entities: serde_json::from_str(&entities_str).unwrap_or_default(),
        topics: serde_json::from_str(&topics_str).unwrap_or_default(),
        category: row.get("category")?,
        importance: row.get("importance")?,
        connections: serde_json::from_str(&connections_str).unwrap_or_default(),
        consolidated: row.get::<_, i32>("consolidated")? != 0,
        source: row.get("source")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn f32_vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for &x in v {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    bytes
}

fn bytes_to_f32_vec(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for i in 0..a.len() {
        let x = a[i] as f64;
        let y = b[i] as f64;
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

fn escape_sql(s: &str) -> String {
    s.replace('\'', "''")
}
