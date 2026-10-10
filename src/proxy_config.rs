//! 代理配置管理 — YAML 文件 + 环境变量覆盖（前缀 AGENT_MEMORY_PROXY__）
//!
//! 配置优先级：CLI 参数 > 环境变量 > YAML 文件 > 默认值

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// 监听地址（默认 0.0.0.0，允许局域网访问）
    #[serde(default = "default_listen_host")]
    pub listen_host: String,

    /// 监听端口（默认 8888）
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,

    /// 远端 MCP 服务器地址（含路径 /mcp）
    #[serde(default = "default_target")]
    pub target: String,

    /// 非本机 IP 访问鉴权 key（客户端需携带 Authorization: Bearer <access_key>）
    #[serde(default)]
    pub access_key: Option<String>,

    /// 代理→远端服务器鉴权 token（代理自动注入 Authorization: Bearer <server_auth_token>）
    #[serde(default)]
    pub server_auth_token: Option<String>,

    /// 日志级别
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// 写缓冲队列容量（默认 1000）
    #[serde(default = "default_write_queue_size")]
    pub write_queue_size: usize,
}

fn default_listen_host() -> String {
    "0.0.0.0".to_string()
}
fn default_listen_port() -> u16 {
    8888
}
fn default_target() -> String {
    "http://192.168.25.104:8888/mcp".to_string()
}
fn default_log_level() -> String {
    "INFO".to_string()
}
fn default_write_queue_size() -> usize {
    1000
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen_host: default_listen_host(),
            listen_port: default_listen_port(),
            target: default_target(),
            access_key: None,
            server_auth_token: None,
            log_level: default_log_level(),
            write_queue_size: default_write_queue_size(),
        }
    }
}

impl ProxyConfig {
    /// 从 YAML 文件加载，再用环境变量覆盖
    pub fn load(path: Option<&str>) -> Result<Self> {
        let mut config = Self::default();

        if let Some(p) = path {
            let p = expand_tilde(p);
            if p.exists() {
                let content = std::fs::read_to_string(&p)
                    .with_context(|| format!("无法读取代理配置文件: {:?}", p))?;
                config = serde_yaml::from_str(&content)
                    .context("解析代理配置 YAML 失败")?;
            }
        }

        // 环境变量覆盖: AGENT_MEMORY_PROXY__TARGET -> target
        for (key, value) in std::env::vars() {
            if let Some(rest) = key.strip_prefix("AGENT_MEMORY_PROXY__") {
                apply_env_override(&mut config, rest, &value);
            }
        }

        Ok(config)
    }
}

fn apply_env_override(config: &mut ProxyConfig, key: &str, value: &str) {
    match key {
        "LISTEN_HOST" => config.listen_host = value.to_string(),
        "LISTEN_PORT" => {
            if let Ok(p) = value.parse() {
                config.listen_port = p;
            }
        }
        "TARGET" => config.target = value.to_string(),
        "ACCESS_KEY" => config.access_key = Some(value.to_string()),
        "SERVER_AUTH_TOKEN" => config.server_auth_token = Some(value.to_string()),
        "LOG_LEVEL" => config.log_level = value.to_string(),
        "WRITE_QUEUE_SIZE" => {
            if let Ok(v) = value.parse() {
                config.write_queue_size = v;
            }
        }
        _ => {}
    }
}

fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(path)
}
