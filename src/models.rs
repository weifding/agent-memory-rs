//! 数据模型定义

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 一条记忆单元
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub namespace: String,
    pub content: String,
    pub summary: Option<String>,
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub category: String,
    pub importance: f64,
    pub connections: Vec<String>,
    pub consolidated: bool,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

impl Memory {
    pub fn new(content: String, namespace: String, category: String) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            id: Uuid::new_v4().to_string(),
            namespace,
            content,
            summary: None,
            entities: vec![],
            topics: vec![],
            category,
            importance: 0.5,
            connections: vec![],
            consolidated: false,
            source: "mcp".to_string(),
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

/// 带相似度的记忆搜索结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub memory: Memory,
    pub similarity: f64,
}

/// 命名空间
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Namespace {
    pub name: String,
    pub description: String,
    pub created_at: String,
}

/// 记忆统计信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStats {
    pub total_memories: i64,
    pub by_category: std::collections::HashMap<String, i64>,
    pub unconsolidated_count: i64,
    pub namespaces: Vec<String>,
}
