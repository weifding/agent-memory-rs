//! agent-memory-proxy — MCP 反向代理（带写缓冲队列）
//!
//! 功能：
//! - 读操作：直接转发到远端 MCP 服务器
//! - 写操作：立即返回成功，通过单线程队列依次转发，避免多 agent 写入竞争
//! - 鉴权：本机回环免鉴权，非本机需 access_key
//! - 代理→远端：自动注入 Authorization: Bearer <server_auth_token>

use anyhow::Result;
use clap::Parser;
use std::time::Instant;
use tracing_subscriber::EnvFilter;

use agent_memory_server::proxy_config::ProxyConfig;

/// 写操作方法名集合
const WRITE_METHODS: &[&str] = &[
    "store_memory",
    "update_memory",
    "delete_memory",
    "graph_create_entity",
    "graph_delete_entity",
    "graph_create_relation",
    "graph_link_memory",
];

#[derive(Parser, Debug)]
#[command(name = "agent-memory-proxy", version, about = "MCP reverse proxy with write queue")]
struct Cli {
    /// 配置文件路径
    #[arg(long)]
    config: Option<String>,

    /// 远端 MCP 服务器地址（覆盖配置）
    #[arg(long)]
    target: Option<String>,

    /// 非本机访问鉴权 key（覆盖配置）
    #[arg(long)]
    access_key: Option<String>,

    /// 代理→远端服务器鉴权 token（覆盖配置）
    #[arg(long)]
    server_auth_token: Option<String>,

    /// 监听端口（覆盖配置）
    #[arg(long)]
    port: Option<u16>,

    /// 监听地址（覆盖配置）
    #[arg(long)]
    host: Option<String>,

    /// 写缓冲队列容量（覆盖配置）
    #[arg(long)]
    write_queue_size: Option<usize>,
}

/// 写任务：待转发到后端的原始请求
struct WriteTask {
    method: String,
    uri: String,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
}

/// 代理共享状态
#[derive(Clone)]
struct ProxyState {
    target: String,
    access_key: Option<String>,
    server_auth_token: Option<String>,
    write_tx: tokio::sync::mpsc::Sender<WriteTask>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = ProxyConfig::load(cli.config.as_deref())?;

    // CLI 覆盖
    if let Some(t) = cli.target {
        config.target = t;
    }
    if let Some(k) = cli.access_key {
        config.access_key = Some(k);
    }
    if let Some(t) = cli.server_auth_token {
        config.server_auth_token = Some(t);
    }
    if let Some(p) = cli.port {
        config.listen_port = p;
    }
    if let Some(h) = cli.host {
        config.listen_host = h;
    }
    if let Some(s) = cli.write_queue_size {
        config.write_queue_size = s;
    }

    // 日志写 stderr
    let filter = std::env::var("RUST_LOG")
        .map(EnvFilter::new)
        .unwrap_or_else(|_| EnvFilter::new(&config.log_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let addr = format!("{}:{}", config.listen_host, config.listen_port);
    tracing::info!("agent-memory-proxy starting on http://{}", addr);
    tracing::info!("  target: {}", config.target);
    tracing::info!(
        "  auth: loopback=免鉴权, remote={}",
        if config.access_key.is_some() { "需要 access_key" } else { "未配置（放行）" }
    );
    tracing::info!("  write_queue_size: {}", config.write_queue_size);

    // 创建写缓冲队列
    let (write_tx, write_rx) = tokio::sync::mpsc::channel::<WriteTask>(config.write_queue_size);

    // 启动写队列后台 worker
    let worker_target = config.target.clone();
    let worker_auth_token = config.server_auth_token.clone();
    tokio::spawn(write_worker(write_rx, worker_target, worker_auth_token));

    let state = ProxyState {
        target: config.target.clone(),
        access_key: config.access_key.clone(),
        server_auth_token: config.server_auth_token.clone(),
        write_tx,
    };

    // 所有请求走 proxy_handler（内部处理 health check、鉴权、转发/入队）
    let app = axum::Router::new()
        .route("/health", axum::routing::get(|| async { "ok" }))
        .fallback(proxy_handler)
        .layer(axum::Extension(state));

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("proxy listening on http://{}/mcp", addr);
    let make_service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, make_service).await?;
    Ok(())
}

/// 反向代理处理器：健康检查 + 鉴权 + 读转发/写入队
async fn proxy_handler(
    axum::extract::Extension(state): axum::extract::Extension<ProxyState>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    req: axum::extract::Request<axum::body::Body>,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    let started = Instant::now();
    let path = req.uri().path().to_string();

    // 健康检查：GET /health 直接返回
    if req.method() == axum::http::Method::GET && path.starts_with("/health") {
        return Ok(axum::response::Response::new(axum::body::Body::from("ok")));
    }

    // 鉴权：回环地址放行，非本机需 access_key
    let is_loopback = addr.ip().is_loopback();
    if !is_loopback {
        if let Some(expected_key) = &state.access_key {
            let auth_header = req
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let expected = format!("Bearer {}", expected_key);
            if auth_header != expected {
                tracing::warn!(remote = %addr, "非本机访问鉴权失败");
                return Err((
                    axum::http::StatusCode::UNAUTHORIZED,
                    "Missing or invalid Authorization header".to_string(),
                ));
            }
        }
    }

    let (parts, body) = req.into_parts();

    // 读取 body 为 bytes（需要解析 JSON 判断是否写操作）
    let body_bytes = axum::body::to_bytes(body, 4 * 1024 * 1024)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("读取 body 失败: {}", e)))?;

    // 判断是否为写操作
    if let Some(write_method) = detect_write_method(&body_bytes) {
        // 写操作：立即返回成功，入队后台转发
        return handle_write(write_method, &body_bytes, &parts, &state, &addr, started);
    }

    // 读操作：直接转发
    forward_request(parts, body_bytes, &state, &addr, started).await
}

/// 检测请求是否为 MCP 写操作，返回写方法名
fn detect_write_method(body: &[u8]) -> Option<&'static str> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    let method = json.get("method")?.as_str()?;
    if method != "tools/call" {
        return None;
    }
    let tool_name = json
        .get("params")?
        .get("name")?
        .as_str()?;
    WRITE_METHODS
        .iter()
        .find(|&&m| m == tool_name)
        .copied()
}

/// 处理写操作：立即返回成功 + 入队
fn handle_write(
    write_method: &str,
    body_bytes: &bytes::Bytes,
    parts: &axum::http::request::Parts,
    state: &ProxyState,
    addr: &std::net::SocketAddr,
    started: Instant,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    // 从请求体解析 MCP JSON-RPC 信息
    let json: serde_json::Value = serde_json::from_slice(body_bytes)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("JSON 解析失败: {}", e)))?;
    let rpc_id = json.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let arguments = json
        .get("params")
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or(serde_json::json!({}));

    // 生成立即返回的响应
    let result = match write_method {
        "store_memory" => {
            let memory_id = uuid::Uuid::new_v4().to_string();
            let namespace = arguments
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            let category = arguments
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("fact");
            serde_json::json!({
                "memory_id": memory_id,
                "namespace": namespace,
                "category": category,
                "status": "stored"
            })
        }
        _ => {
            serde_json::json!({"status": "queued"})
        }
    };

    // 构建 JSON-RPC 成功响应
    let response_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": rpc_id,
        "result": {
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&result).unwrap_or_default()
            }]
        }
    });

    // 入队：将原始请求交给后台 worker 转发
    let task = WriteTask {
        method: parts.method.to_string(),
        uri: parts.uri.to_string(),
        headers: parts.headers.clone(),
        body: body_bytes.clone(),
    };

    match state.write_tx.try_send(task) {
        Ok(()) => {
            tracing::info!(
                target: "write_queue",
                method = write_method,
                remote = %addr,
                elapsed_ms = started.elapsed().as_millis(),
                "写操作已入队，立即返回"
            );
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            tracing::error!(target: "write_queue", "写队列已满，丢弃请求");
            return Err((
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "写队列已满，请稍后重试".to_string(),
            ));
        }
        Err(e) => {
            tracing::error!(target: "write_queue", error = %e, "入队失败");
            return Err((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("入队失败: {}", e),
            ));
        }
    }

    // 立即返回成功响应
    Ok(axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_string(&response_body).unwrap_or_default(),
        ))
        .unwrap())
}

/// 读操作：直接转发到远端 MCP 服务器
async fn forward_request(
    parts: axum::http::request::Parts,
    body_bytes: bytes::Bytes,
    state: &ProxyState,
    addr: &std::net::SocketAddr,
    started: Instant,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    let target_uri = parse_target(&state.target)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("目标地址无效: {}", e)))?;

    let mut forward_req = axum::http::Request::builder()
        .method(parts.method.clone())
        .uri(target_uri.clone())
        .body(axum::body::Body::from(body_bytes))
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 复制原始 headers（跳过 hop-by-hop）
    for (key, value) in &parts.headers {
        if is_hop_by_hop(key.as_str()) {
            continue;
        }
        forward_req.headers_mut().insert(key.clone(), value.clone());
    }

    // 注入服务端鉴权 token
    if let Some(token) = &state.server_auth_token {
        let auth_value = format!("Bearer {}", token);
        if let Ok(v) = axum::http::HeaderValue::from_str(&auth_value) {
            forward_req
                .headers_mut()
                .insert(axum::http::header::AUTHORIZATION, v);
        }
    }

    let client = build_http_client();
    let res = client.request(forward_req).await.map_err(|e| {
        tracing::error!(error = %e, "转发请求失败");
        (axum::http::StatusCode::BAD_GATEWAY, format!("转发失败: {}", e))
    })?;

    tracing::info!(
        target: "proxy",
        method = %parts.method,
        uri = %parts.uri,
        remote = %addr,
        status = res.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis(),
        "=> forwarded to {}", state.target
    );

    // 流式透传响应
    let (res_parts, res_body) = res.into_parts();
    let mut response = axum::response::Response::builder().status(res_parts.status);
    for (key, value) in &res_parts.headers {
        if !is_hop_by_hop(key.as_str()) {
            response = response.header(key, value);
        }
    }

    use futures::TryStreamExt;
    let stream = http_body_util::BodyStream::new(res_body).map_ok(|frame| {
        frame.into_data().unwrap_or_default()
    });
    response
        .body(axum::body::Body::from_stream(stream))
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// 写队列后台 worker：单线程依次转发写请求到后端
async fn write_worker(
    mut rx: tokio::sync::mpsc::Receiver<WriteTask>,
    target: String,
    server_auth_token: Option<String>,
) {
    tracing::info!(target: "write_queue", "写队列 worker 已启动");
    let client = build_http_client();

    while let Some(task) = rx.recv().await {
        let started = Instant::now();
        let method = task.method.clone();

        match forward_write_task(&client, &task, &target, &server_auth_token).await {
            Ok(status) => {
                tracing::info!(
                    target: "write_queue",
                    method = %method,
                    status = status,
                    elapsed_ms = started.elapsed().as_millis(),
                    "写操作已转发到后端"
                );
            }
            Err(e) => {
                tracing::error!(
                    target: "write_queue",
                    method = %method,
                    error = %e,
                    elapsed_ms = started.elapsed().as_millis(),
                    "写操作转发失败"
                );
            }
        }
    }

    tracing::warn!(target: "write_queue", "写队列 worker 退出（channel 已关闭）");
}

/// 转发单个写任务到后端
async fn forward_write_task(
    client: &hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>,
    task: &WriteTask,
    target: &str,
    server_auth_token: &Option<String>,
) -> Result<u16> {
    let target_uri: hyper::Uri = target.parse().map_err(|e| anyhow::anyhow!("目标地址无效: {}", e))?;

    let mut forward_req = axum::http::Request::builder()
        .method(task.method.as_str())
        .uri(target_uri)
        .body(axum::body::Body::from(task.body.clone()))?;

    // 复制 headers
    for (key, value) in &task.headers {
        if !is_hop_by_hop(key.as_str()) {
            forward_req.headers_mut().insert(key.clone(), value.clone());
        }
    }

    // 确保 Accept header 包含 MCP 所需的两种类型
    if !task.headers.contains_key(axum::http::header::ACCEPT) {
        forward_req.headers_mut().insert(
            axum::http::header::ACCEPT,
            axum::http::HeaderValue::from_static("application/json, text/event-stream"),
        );
    }

    // 注入鉴权 token
    if let Some(token) = server_auth_token {
        let auth_value = format!("Bearer {}", token);
        if let Ok(v) = axum::http::HeaderValue::from_str(&auth_value) {
            forward_req
                .headers_mut()
                .insert(axum::http::header::AUTHORIZATION, v);
        }
    }

    let res = client.request(forward_req).await?;
    Ok(res.status().as_u16())
}

/// 解析目标 URL
fn parse_target(target: &str) -> Result<hyper::Uri, String> {
    target
        .parse::<hyper::Uri>()
        .map_err(|e| format!("无法解析 {}: {}", target, e))
}

/// 构建 HTTP 客户端（连接池）
fn build_http_client(
) -> hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body> {
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_nodelay(true);
    connector.enforce_http(true);

    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .build(connector)
}

/// 判断是否为 hop-by-hop header
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}