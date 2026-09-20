//! Embedding 提供方 — OpenAI 兼容 /v1/embeddings 客户端 + 本地服务自动探测
//!
//! 启动时按候选端点并发探测本机常见推理服务（LM Studio / Ollama / vLLM /
//! Xinference / llama.cpp），命中即自动回填配置（provider/base_url/model/dimensions），
//! 未命中则沿用字符 bigram 伪嵌入，两种路径对上层透明。
//!
//! 向量空间一致性：以 `kv_meta.embedding_space`（"openai:<model>@<dim>" 或
//! "pseudo-bigram@<dim>"）记录当前空间，提供方变化时自动重嵌全部存量记忆，
//! 避免新旧向量混用导致相似度失真。
//!
//! HTTP 仅支持 http:// 端点（本地服务场景；依赖树刻意未引入 TLS 栈），
//! https 云端 API 可通过显式配置 + 本地反代接入。

use anyhow::{anyhow, Context, Result};
use futures::future::join_all;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use crate::config::AppConfig;
use crate::storage::SQLiteStorage;

/// 自动探测端口：LM Studio 1234 / Ollama 11434 / vLLM 8000 / Xinference 9997 / llama.cpp 8080
const DETECT_PORTS: [u16; 5] = [1234, 11434, 8000, 9997, 8080];
/// 探测超时（本地回环，600ms 足够；5 个端口并发探测，总开销 ~1s）。
/// 仅用于 /models 发现：无服务时快速失败
const DETECT_TIMEOUT: Duration = Duration::from_millis(600);
/// 探测中的 embeddings 试算超时：模型可能处于冷加载（2B 级首次推理需数秒），放宽到 30s
const PROBE_EMBED_TIMEOUT: Duration = Duration::from_secs(30);
const EMBED_TIMEOUT: Duration = Duration::from_secs(30);

/// kv_meta 中记录当前向量空间的键名
pub const META_EMBEDDING_SPACE: &str = "embedding_space";

type HttpBody = Full<Bytes>;
type HttpClient = Client<HttpConnector, HttpBody>;

/// 自动探测命中的服务器描述
pub struct Detected {
    pub base_url: String,
    pub model: String,
    pub dimensions: usize,
}

/// 嵌入协议（由配置 `embedding.provider` 选择，新增协议即新增枚举值 + embed 分支）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// OpenAI 兼容 POST {base}/v1/embeddings {"model","input":[..]}
    OpenAi,
    /// Ollama 原生 POST {base}/api/embed {"model","input":[..]} → {"embeddings":[[..]]}
    Ollama,
}

impl Protocol {
    pub fn from_provider(p: &str) -> Self {
        match p.trim().to_lowercase().as_str() {
            "ollama" => Protocol::Ollama,
            _ => Protocol::OpenAi, // openai 及其他兼容实现默认走 OpenAI 协议
        }
    }
    pub fn space_prefix(self) -> &'static str {
        match self {
            Protocol::OpenAi => "openai",
            Protocol::Ollama => "ollama",
        }
    }
}

pub struct EmbeddingClient {
    http: HttpClient,
    protocol: Protocol,
    base_url: String, // 无尾斜杠；OpenAi 协议已含 /v1
    model: String,
    api_key: Option<String>,
    pub model_id: String,
    pub dimensions: usize,
}

impl EmbeddingClient {
    pub fn new(
        protocol: Protocol,
        base_url: String,
        model: String,
        api_key: Option<String>,
        dimensions: usize,
    ) -> Self {
        let mut base_url = base_url.trim_end_matches('/').to_string();
        if protocol == Protocol::OpenAi && !base_url.ends_with("/v1") {
            base_url.push_str("/v1");
        }
        Self {
            http: Client::builder(TokioExecutor::new()).build_http::<HttpBody>(),
            protocol,
            base_url,
            model,
            api_key,
            model_id: String::new(),
            dimensions,
        }
    }

    /// 批量嵌入，返回顺序与输入一致（按协议分支，默认 30s 超时）
    pub async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_with_timeout(texts, EMBED_TIMEOUT).await
    }

    /// 带自定义超时的批量嵌入（存量重嵌等大批量场景用更长超时）
    pub async fn embed_with_timeout(
        &self,
        texts: &[&str],
        timeout: Duration,
    ) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        match self.protocol {
            Protocol::OpenAi => self.embed_openai(texts, timeout).await,
            Protocol::Ollama => self.embed_ollama(texts, timeout).await,
        }
    }

    async fn embed_openai(&self, texts: &[&str], timeout: Duration) -> Result<Vec<Vec<f32>>> {
        let resp = self
            .post_json("/embeddings", json!({"model": self.model, "input": texts}), timeout)
            .await?;
        let v: Value = serde_json::from_str(&resp).context("embedding 响应非 JSON")?;
        let data = v
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| anyhow!("embedding 响应缺少 data 数组: {}", truncate(&resp, 200)))?;

        let mut out: Vec<(usize, Vec<f32>)> = data
            .iter()
            .filter_map(|item| {
                let idx = item.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let emb = item.get("embedding")?.as_array()?;
                if emb.is_empty() {
                    return None;
                }
                Some((idx, emb.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect()))
            })
            .collect();
        if out.len() != texts.len() {
            return Err(anyhow!(
                "embedding 返回条数不匹配: 期望 {} 实际 {}",
                texts.len(),
                out.len()
            ));
        }
        out.sort_by_key(|(i, _)| *i);
        Ok(out.into_iter().map(|(_, v)| v).collect())
    }

    /// Ollama 原生批量接口 POST /api/embed {"model","input":[..]} → {"embeddings":[[..]]}
    async fn embed_ollama(&self, texts: &[&str], timeout: Duration) -> Result<Vec<Vec<f32>>> {
        let resp = self
            .post_json("/api/embed", json!({"model": self.model, "input": texts}), timeout)
            .await?;
        let v: Value = serde_json::from_str(&resp).context("embedding 响应非 JSON")?;
        let data = v
            .get("embeddings")
            .and_then(|d| d.as_array())
            .ok_or_else(|| anyhow!("ollama 响应缺少 embeddings 数组: {}", truncate(&resp, 200)))?;
        if data.len() != texts.len() {
            return Err(anyhow!(
                "embedding 返回条数不匹配: 期望 {} 实际 {}",
                texts.len(),
                data.len()
            ));
        }
        Ok(data
            .iter()
            .filter_map(|emb| emb.as_array())
            .map(|emb| emb.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect())
            .collect())
    }

    async fn post_json(&self, path: &str, body: Value, timeout: Duration) -> Result<String> {
        let url = format!("{}{}", self.base_url, path);
        let text = self
            .request(hyper::Method::POST, &url, Some(body.to_string()), timeout)
            .await?;
        Ok(text)
    }

    async fn request(
        &self,
        method: hyper::Method,
        url: &str,
        body: Option<String>,
        timeout: Duration,
    ) -> Result<String> {
        let mut builder = hyper::Request::builder()
            .method(method)
            .uri(url)
            .header(hyper::header::CONTENT_TYPE, "application/json");
        if let Some(k) = &self.api_key {
            if !k.is_empty() {
                builder = builder.header(hyper::header::AUTHORIZATION, format!("Bearer {k}"));
            }
        }
        let req = builder.body(Full::new(Bytes::from(body.unwrap_or_default())))?;
        let resp = tokio::time::timeout(timeout, self.http.request(req))
            .await
            .map_err(|_| anyhow!("embedding 请求超时: {url}"))??;
        let status = resp.status();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .with_context(|| format!("读取 embedding 响应失败: {url}"))?
            .to_bytes();
        let text = String::from_utf8_lossy(&bytes).to_string();
        if !status.is_success() {
            return Err(anyhow!("HTTP {status} from {url}: {}", truncate(&text, 300)));
        }
        Ok(text)
    }
}

/// 归一化 base URL：去尾斜杠，缺 /v1 时补上
pub fn normalize_base(url: &str) -> String {
    let mut u = url.trim().trim_end_matches('/').to_string();
    if !u.ends_with("/v1") {
        u.push_str("/v1");
    }
    u
}

/// 探测单个候选端点：GET /models → 挑模型 → POST /embeddings 试算维度
pub async fn probe(base: &str) -> Option<Detected> {
    let http: HttpClient = Client::builder(TokioExecutor::new()).build_http::<HttpBody>();
    let models_url = format!("{}/models", base.trim_end_matches('/'));

    let body = tokio::time::timeout(DETECT_TIMEOUT, get_json(&http, &models_url))
        .await
        .ok()?
        .ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    let ids: Vec<String> = v
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("id")?.as_str().map(String::from))
        .collect();
    let model = pick_model(&ids)?;

    let client = EmbeddingClient::new(Protocol::OpenAi, base.to_string(), model.clone(), None, 0);
    let embs = tokio::time::timeout(
        PROBE_EMBED_TIMEOUT,
        client.embed_with_timeout(&["embedding probe"], PROBE_EMBED_TIMEOUT),
    )
    .await
    .ok()?
    .ok()?;
    let dimensions = embs.into_iter().next()?.len();
    if !(16..=100_000).contains(&dimensions) {
        return None;
    }
    Some(Detected { base_url: base.to_string(), model, dimensions })
}

/// 并发探测全部候选端点，按候选顺序返回首个命中
pub async fn detect(candidates: &[String]) -> Option<Detected> {
    if candidates.is_empty() {
        return None;
    }
    let probes = candidates.iter().map(|c| probe(c));
    let results = join_all(probes).await;
    for (cand, r) in candidates.iter().zip(results) {
        if let Some(d) = r {
            let _ = cand;
            return Some(d);
        }
    }
    None
}

/// 模型挑选：WeMM / 含 embed 字样优先，同分取先出现者
fn pick_model(ids: &[String]) -> Option<String> {
    let score = |id: &str| -> i32 {
        let l = id.to_lowercase();
        if l.contains("wemm") {
            4
        } else if l.contains("embed") {
            3
        } else if l.contains("bge") || l.contains("gte") || l.starts_with("e5") {
            2
        } else {
            0
        }
    };
    let mut best: Option<(i32, &String)> = None;
    for id in ids {
        let s = score(id);
        if best.map(|(bs, _)| s > bs).unwrap_or(true) {
            best = Some((s, id));
        }
    }
    best.map(|(_, id)| id.clone())
}

async fn get_json(http: &HttpClient, url: &str) -> Result<String> {
    let req = hyper::Request::builder()
        .method(hyper::Method::GET)
        .uri(url)
        .body(Full::new(Bytes::new()))?;
    let resp = http.request(req).await?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {}", resp.status()));
    }
    let bytes = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ----------------------------------------------------------------------
// 启动装配：探测/显式配置 + 向量空间一致性（切换即重嵌）
// ----------------------------------------------------------------------

pub async fn setup(
    config: &mut AppConfig,
    storage: &Arc<SQLiteStorage>,
) -> Result<Option<Arc<EmbeddingClient>>> {
    let explicit = config
        .embedding
        .provider
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    let client: Option<Arc<EmbeddingClient>> = if explicit {
        // 显式配置直接采用，不探测（配置错误在使用时报错，便于定位）；
        // provider 决定协议：openai（默认，兼容实现均走此协议）/ ollama（原生 /api/embed）
        let protocol = Protocol::from_provider(config.embedding.provider.as_deref().unwrap_or(""));
        let dim = config.embedding.dimensions.max(1) as usize;
        let base = match protocol {
            // Ollama 原生协议：地址/模型缺省时给本地默认值（用户显式配置优先）
            Protocol::Ollama => {
                let u = config.embedding.base_url.trim().trim_end_matches('/').to_string();
                if u.is_empty() || u.contains("api.openai.com") {
                    "http://127.0.0.1:11434".to_string()
                } else {
                    u
                }
            }
            Protocol::OpenAi => normalize_base(&config.embedding.base_url),
        };
        // Ollama 模型未显式指定时（仍是 OpenAI 默认值），从 /api/tags 按 pick_model 挑选
        let mut model = config.embedding.model.clone();
        if protocol == Protocol::Ollama
            && (model.is_empty() || model == "text-embedding-3-small")
        {
            let http: HttpClient = Client::builder(TokioExecutor::new()).build_http::<HttpBody>();
            let tags = get_json(&http, &format!("{base}/api/tags")).await.unwrap_or_default();
            let ids: Vec<String> = serde_json::from_str::<Value>(&tags)
                .ok()
                .and_then(|v| {
                    v.get("models")?.as_array().map(|a| {
                        a.iter()
                            .filter_map(|m| m.get("name")?.as_str().map(String::from))
                            .collect()
                    })
                })
                .unwrap_or_default();
            if let Some(picked) = pick_model(&ids).or_else(|| ids.first().cloned()) {
                model = picked;
                config.embedding.model = model.clone();
            }
        }
        // Ollama 维度未显式指定时（仍为默认 1536），试算一条测出真实维度
        let mut dim = dim;
        if protocol == Protocol::Ollama && config.embedding.dimensions == 1536 {
            let probe_c = EmbeddingClient::new(protocol, base.clone(), model.clone(), None, 0);
            if let Ok(embs) = probe_c
                .embed_with_timeout(&["dimension probe"], PROBE_EMBED_TIMEOUT)
                .await
            {
                if let Some(v) = embs.first() {
                    dim = v.len();
                    config.embedding.dimensions = dim as i32;
                }
            }
        }
        let c = EmbeddingClient::new(
            protocol,
            base,
            model,
            config.embedding.api_key.clone(),
            dim,
        );
        tracing::info!(
            target: "embedding",
            protocol = ?protocol,
            base_url = %c.base_url,
            model = %c.model,
            dimensions = dim,
            "embedding provider: 使用显式配置"
        );
        Some(Arc::new(c))
    } else {
        let mut candidates: Vec<String> = Vec::new();
        // 环境变量优先，其次配置里显式写的本地地址，最后常见端口
        if let Ok(u) = std::env::var("EMBEDDING_BASE_URL") {
            if !u.trim().is_empty() {
                candidates.push(normalize_base(&u));
            }
        }
        let cfg_base = config.embedding.base_url.to_lowercase();
        if cfg_base.contains("127.0.0.1") || cfg_base.contains("localhost") {
            candidates.push(normalize_base(&config.embedding.base_url));
        }
        for p in DETECT_PORTS {
            let c = format!("http://127.0.0.1:{p}/v1");
            if !candidates.contains(&c) {
                candidates.push(c);
            }
        }

        match detect(&candidates).await {
            Some(d) => {
                tracing::info!(
                    target: "embedding",
                    base_url = %d.base_url,
                    model = %d.model,
                    dimensions = d.dimensions,
                    "embedding 服务器自动检测成功"
                );
                config.embedding.provider = Some("openai".to_string());
                config.embedding.base_url = d.base_url.clone();
                config.embedding.model = d.model.clone();
                config.embedding.dimensions = d.dimensions as i32;
                Some(Arc::new(EmbeddingClient::new(
                    Protocol::OpenAi,
                    d.base_url,
                    d.model,
                    config.embedding.api_key.clone(),
                    d.dimensions,
                )))
            }
            None => {
                tracing::info!(
                    target: "embedding",
                    "未检测到本地 embedding 服务器（已探测 LM Studio:1234 / Ollama:11434 / vLLM:8000 / Xinference:9997 / llama.cpp:8080，可用 EMBEDDING_BASE_URL 指定），沿用字符 bigram 伪嵌入"
                );
                None
            }
        }
    };

    // 向量空间一致性：提供方或维度变化 → 重嵌全部存量记忆，保证新旧向量可比。
    // 重嵌失败不阻断启动（服务可用，向量处于混合空间），下次重启自动重试
    let space = match &client {
        Some(c) => format!("{}:{}@{}", c.protocol.space_prefix(), c.model, c.dimensions),
        None => format!("pseudo-bigram@{}", storage.embedding_dim()),
    };
    if storage.get_meta(META_EMBEDDING_SPACE)?.as_deref() != Some(space.as_str()) {
        match reembed_all(storage, client.as_deref()).await {
            Ok(n) => {
                storage.set_meta(META_EMBEDDING_SPACE, &space)?;
                tracing::info!(target: "embedding", reembedded = n, space = %space, "向量空间变化：存量记忆已重嵌");
            }
            Err(e) => {
                tracing::error!(target: "embedding", error = %e, space = %space,
                    "存量记忆重嵌失败（不影响启动）；向量处于混合空间，下次重启将自动重试");
            }
        }
    }

    Ok(client)
}

/// 重嵌全部存量记忆（client 为 None 时用伪嵌入，用于回退路径的空间归一）。
/// 小批量 + 长超时：2B 级模型首次推理慢，单请求 500 条会超时
async fn reembed_all(storage: &SQLiteStorage, client: Option<&EmbeddingClient>) -> Result<usize> {
    const BATCH: i32 = 32;
    const BATCH_TIMEOUT: Duration = Duration::from_secs(600);
    let mut offset = 0i32;
    let mut done = 0usize;
    loop {
        let batch = storage.list_filtered(None, None, BATCH, offset)?;
        if batch.is_empty() {
            break;
        }
        offset += BATCH;
        if let Some(c) = client {
            let texts: Vec<&str> = batch.iter().map(|m| m.content.as_str()).collect();
            let embs = c.embed_with_timeout(&texts, BATCH_TIMEOUT).await?;
            for (m, e) in batch.iter().zip(embs) {
                storage.update_embedding(&m.id, &e)?;
            }
        } else {
            for m in &batch {
                let e = pseudo_embedding(&m.content, storage.embedding_dim());
                storage.update_embedding(&m.id, &e)?;
            }
        }
        done += batch.len();
        tracing::info!(target: "embedding", done, "存量重嵌进行中");
    }
    Ok(done)
}

// ----------------------------------------------------------------------
// 伪嵌入（无 embedding 服务器时的回退实现）
// ----------------------------------------------------------------------

/// 字符 bigram 集合：中文无需分词即可产生重叠特征，英文/混合文本同样有效
pub(crate) fn char_bigrams(s: &str) -> std::collections::HashSet<String> {
    let chars: Vec<char> = s.chars().collect();
    match chars.len() {
        0 => std::collections::HashSet::new(),
        1 => [chars[0].to_string()].into_iter().collect(),
        _ => chars
            .windows(2)
            .map(|w| w.iter().collect::<String>())
            .collect(),
    }
}

/// 字符 bigram 哈希伪嵌入（L2 归一化）。
/// 写入与查询使用同一函数即构成端到端向量检索；检测到真实 embedding 服务器后自动停用。
pub(crate) fn pseudo_embedding(text: &str, dim: usize) -> Vec<f32> {
    let dim = dim.max(1);
    let mut v = vec![0f32; dim];
    for g in char_bigrams(text) {
        let mut h = 0usize;
        for &b in g.as_bytes() {
            h = h.wrapping_mul(31).wrapping_add(b as usize);
        }
        v[h % dim] += 1.0;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}
