//! agent-memory-proxy — MCP 反向代理
//!
//! 转发 MCP Streamable HTTP 请求到远端 agent-memory-server，鉴权策略：
//! - 本机回环地址（127.0.0.1, ::1）：免鉴权
//! - 非本机 IP：需 Authorization: Bearer <access_key>
//! - 代理→远端服务器：自动注入 Authorization: Bearer <server_auth_token>

use anyhow::Result;
use clap::Parser;
use std::time::Instant;
use tracing_subscriber::EnvFilter;

use agent_memory_server::proxy_config::ProxyConfig;

#[derive(Parser, Debug)]
#[command(name = "agent-memory-proxy", version, about = "MCP reverse proxy for agent-memory-server")]
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

    let state = ProxyState {
        target: config.target.clone(),
        access_key: config.access_key.clone(),
        server_auth_token: config.server_auth_token.clone(),
    };

    // 所有请求走 proxy_handler（内部处理 health check、鉴权、转发）
    // 使用 Extension 传递状态，避免 with_state 可能导致的路由问题
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

/// 代理共享状态
#[derive(Clone)]
struct ProxyState {
    target: String,
    access_key: Option<String>,
    server_auth_token: Option<String>,
}

/// 反向代理处理器：健康检查 + 鉴权 + 转发到远端 MCP 服务器
async fn proxy_handler(
    axum::extract::Extension(state): axum::extract::Extension<ProxyState>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    req: axum::extract::Request<axum::body::Body>,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    let started = Instant::now();
    let path = req.uri().path().to_string();

    // 健康检查：GET /health 直接返回，不代理
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

    // 解析远端目标 URL
    let target_uri = parse_target(&state.target)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("目标地址无效: {}", e)))?;

    // 构建转发请求
    let mut forward_req = axum::http::Request::builder()
        .method(parts.method.clone())
        .uri(target_uri.clone())
        .body(body)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 复制原始 headers
    for (key, value) in &parts.headers {
        // 跳过 hop-by-hop headers
        if is_hop_by_hop(key.as_str()) {
            continue;
        }
        forward_req.headers_mut().insert(key.clone(), value.clone());
    }

    // 注入服务端鉴权 token（覆盖客户端的 Authorization）
    if let Some(token) = &state.server_auth_token {
        let auth_value = format!("Bearer {}", token);
        if let Ok(v) = axum::http::HeaderValue::from_str(&auth_value) {
            forward_req
                .headers_mut()
                .insert(axum::http::header::AUTHORIZATION, v);
        }
    }

    // 不覆盖 Host header，让 hyper 客户端自动设置

    // 发起 HTTP 请求（流式，不缓冲）
    let client = build_http_client();
    let res = client
        .request(forward_req)
        .await
        .map_err(|e| {
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

    // 流式透传响应（保留 headers 和 body stream）
    let (res_parts, res_body) = res.into_parts();
    let mut response = axum::response::Response::builder().status(res_parts.status);
    for (key, value) in &res_parts.headers {
        if !is_hop_by_hop(key.as_str()) {
            response = response.header(key, value);
        }
    }

    // 将 hyper body 转为 Stream<Item = Result<Bytes, _>> 供 axum Body::from_stream 使用
    use futures::TryStreamExt;
    let stream = http_body_util::BodyStream::new(res_body).map_ok(|frame| {
        frame.into_data().unwrap_or_default()
    });
    response
        .body(axum::body::Body::from_stream(stream))
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// 解析目标 URL 为 hyper URI
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

/// 判断是否为 hop-by-hop header（不应被代理转发）
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
