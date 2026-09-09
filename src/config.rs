//! 配置管理 — YAML 文件 + 环境变量覆盖

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_db_path")]
    pub db_path: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self { db_path: default_db_path() }
    }
}

fn default_db_path() -> String {
    "~/.agent-memory/memory.db".to_string()
}

impl StorageConfig {
    pub fn resolved_path(&self) -> PathBuf {
        expand_tilde(&self.db_path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_embedding_model")]
    pub model: String,
    #[serde(default = "default_dimensions")]
    pub dimensions: i32,
}

fn default_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}
fn default_embedding_model() -> String {
    "text-embedding-3-small".to_string()
}
fn default_dimensions() -> i32 {
    1536
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: None,
            base_url: default_base_url(),
            api_key: None,
            model: default_embedding_model(),
            dimensions: default_dimensions(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default = "default_host")]
    pub http_host: String,
    #[serde(default = "default_port")]
    pub http_port: u16,
    /// HTTP 传输鉴权令牌；绑定非 127.0.0.1 时必须设置（对齐 Python 版安全约束）
    #[serde(default)]
    pub auth_token: Option<String>,
}

fn default_transport() -> String {
    "stdio".to_string()
}
fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    8888
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: default_transport(),
            http_host: default_host(),
            http_port: default_port(),
            auth_token: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_graph_path")]
    pub db_path: String,
    #[serde(default = "default_buffer_pool")]
    pub buffer_pool_size_mb: usize,
    /// store_memory 时自动抽取实体写入图谱（需 enabled=true 且 graph feature）
    #[serde(default = "default_true")]
    pub auto_extract: bool,
    /// 进程启动时把存量记忆按同一规则回填一次图谱（幂等）
    #[serde(default = "default_true")]
    pub backfill_on_start: bool,
    /// 追加的抽取规则（见 extract::ExtractRuleConf），追加到内置词典之后
    #[serde(default)]
    pub rules: Vec<crate::extract::ExtractRuleConf>,
}

fn default_graph_path() -> String {
    "~/.agent-memory/graph.kuzu".to_string()
}
fn default_buffer_pool() -> usize {
    128
}
fn default_true() -> bool {
    true
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: default_graph_path(),
            buffer_pool_size_mb: default_buffer_pool(),
            auto_extract: true,
            backfill_on_start: true,
            rules: Vec::new(),
        }
    }
}

impl GraphConfig {
    pub fn resolved_path(&self) -> PathBuf {
        expand_tilde(&self.db_path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub graph: GraphConfig,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_log_level() -> String {
    "INFO".to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            storage: StorageConfig { db_path: default_db_path() },
            embedding: EmbeddingConfig {
                provider: None,
                base_url: default_base_url(),
                api_key: None,
                model: default_embedding_model(),
                dimensions: default_dimensions(),
            },
            server: ServerConfig::default(),
            graph: GraphConfig::default(),
            log_level: default_log_level(),
        }
    }
}

pub fn load_config(path: Option<&str>) -> Result<AppConfig> {
    let mut config = AppConfig::default();

    if let Some(p) = path {
        let p = expand_tilde(p);
        if p.exists() {
            let content = std::fs::read_to_string(&p)
                .with_context(|| format!("Failed to read config file: {:?}", p))?;
            config = serde_yaml::from_str(&content)
                .with_context(|| "Failed to parse config YAML")?;
        }
    }

    // 环境变量覆盖: AGENT_MEMORY_STORAGE__DB_PATH -> storage.db_path
    for (key, value) in std::env::vars() {
        if let Some(rest) = key.strip_prefix("AGENT_MEMORY_") {
            let parts: Vec<&str> = rest.split("__").collect();
            apply_env_override(&mut config, &parts, &value);
        }
    }

    Ok(config)
}

fn apply_env_override(config: &mut AppConfig, parts: &[&str], value: &str) {
    // 简化实现：只处理常见的两级覆盖
    if parts.len() == 2 {
        match (parts[0], parts[1]) {
            ("STORAGE", "DB_PATH") => config.storage.db_path = value.to_string(),
            ("SERVER", "TRANSPORT") => config.server.transport = value.to_string(),
            ("SERVER", "AUTH_TOKEN") => config.server.auth_token = Some(value.to_string()),
            ("SERVER", "HTTP_HOST") => config.server.http_host = value.to_string(),
            ("SERVER", "HTTP_PORT") => {
                if let Ok(p) = value.parse() {
                    config.server.http_port = p;
                }
            }
            ("GRAPH", "ENABLED") => config.graph.enabled = parse_bool(value),
            ("GRAPH", "DB_PATH") => config.graph.db_path = value.to_string(),
            ("GRAPH", "AUTO_EXTRACT") => config.graph.auto_extract = parse_bool(value),
            ("GRAPH", "BACKFILL_ON_START") => config.graph.backfill_on_start = parse_bool(value),
            ("GRAPH", "BUFFER_POOL_SIZE_MB") => {
                if let Ok(v) = value.parse() {
                    config.graph.buffer_pool_size_mb = v;
                }
            }
            ("EMBEDDING", "PROVIDER") => config.embedding.provider = Some(value.to_string()),
            ("EMBEDDING", "DIMENSIONS") => {
                if let Ok(d) = value.parse() {
                    config.embedding.dimensions = d;
                }
            }
            _ => {}
        }
    }
}

fn parse_bool(value: &str) -> bool {
    value == "true" || value == "1" || value == "yes"
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}
