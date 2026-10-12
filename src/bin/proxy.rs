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

/// 共享 HTTP 客户端类型别名
type SharedClient = hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    axum::body::Body,
>;

/// 代理共享状态
#[derive(Clone)]
struct ProxyState {
    target: String,
    access_key: Option<String>,
    server_auth_token: Option<String>,
    write_tx: tokio::sync::mpsc::Sender<WriteTask>,
    /// 全进程共享的 HTTP 客户端：连接池跨请求复用。
    /// 此前读路径每请求新建 client，连接无法复用且后端重启/网络抖动时
    /// 易出现 hyper SendRequest 瞬时错误（「转发失败」）
    http_client: SharedClient,
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

    // 全进程共享一个 HTTP 客户端（读路径 + 写 worker 共用连接池）
    let http_client = build_http_client();

    // 启动写队列后台 worker
    let worker_target = config.target.clone();
    let worker_auth_token = config.server_auth_token.clone();
    let worker_journal_dir = config.journal_dir.clone();
    tokio::spawn(write_worker(
        write_rx,
        worker_target,
        worker_auth_token,
        http_client.clone(),
        worker_journal_dir,
    ));

    let state = ProxyState {
        target: config.target.clone(),
        access_key: config.access_key.clone(),
        server_auth_token: config.server_auth_token.clone(),
        write_tx,
        http_client: http_client.clone(),
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
    let mut json: serde_json::Value = serde_json::from_slice(body_bytes)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("JSON 解析失败: {}", e)))?;
    let rpc_id = json.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let arguments = json
        .get("params")
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or(serde_json::json!({}));

    // store_memory：代理本地生成 memory_id，注入转发请求 arguments，
    // 保证即时 ACK 返回的 ID 与后端落库 ID 一致（后端 INSERT OR REPLACE 幂等）
    let mut forward_body = json.clone();
    let generated_id = if write_method == "store_memory" {
        let memory_id = uuid::Uuid::new_v4().to_string();
        if let Some(args) = forward_body
            .get_mut("params")
            .and_then(|p| p.get_mut("arguments"))
            .and_then(|a| a.as_object_mut())
        {
            args.insert("memory_id".to_string(), serde_json::Value::String(memory_id.clone()));
        }
        Some(memory_id)
    } else {
        None
    };

    // 生成立即返回的响应
    let result = match write_method {
        "store_memory" => {
            let memory_id = generated_id.unwrap_or_default();
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

    // 构建 JSON-RPC 成功响应。
    // resultType 必须携带：2026-07-28 协议的客户端（如 ZCode discover 生命周期）
    // 对 tools/call 结果强制校验该字段，缺失直接报协议错（老协议客户端按规范忽略未知字段，兼容）
    let response_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": rpc_id,
        "result": {
            "resultType": "complete",
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&result).unwrap_or_default()
            }]
        }
    });

    // 入队：将（注入 memory_id 后的）原始请求交给后台 worker 转发
    let task = WriteTask {
        method: parts.method.to_string(),
        uri: parts.uri.to_string(),
        headers: parts.headers.clone(),
        body: bytes::Bytes::from(serde_json::to_vec(&forward_body).unwrap_or_default()),
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

/// 读操作：直接转发到远端 MCP 服务器（共享连接池；瞬时连接错误重试一次）
async fn forward_request(
    parts: axum::http::request::Parts,
    body_bytes: bytes::Bytes,
    state: &ProxyState,
    addr: &std::net::SocketAddr,
    started: Instant,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    match forward_request_once(&parts, body_bytes.clone(), state, addr, started).await {
        Ok(res) => Ok(res),
        Err((status, msg)) if status == axum::http::StatusCode::BAD_GATEWAY => {
            // 瞬时连接错误（如后端重启后旧连接失效）：换新连接重试一次
            if msg.contains("SendRequest") || msg.contains("Connect") {
                tracing::warn!(target: "proxy", error = %msg, "瞬时转发失败，重试一次");
                forward_request_once(&parts, body_bytes, state, addr, started).await
            } else {
                Err((status, msg))
            }
        }
        Err(e) => Err(e),
    }
}

/// 单次转发（无重试）
async fn forward_request_once(
    parts: &axum::http::request::Parts,
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

    // 共享连接池：跨请求复用连接，避免每请求新建连接与后端重启后的 stale 连接问题
    let res = state
        .http_client
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

/// 写队列后台 worker：单线程依次转发写请求到后端。
/// 客户端已收到 ACK，转发失败绝不能丢——重试后仍失败的一律落盘 journal。
/// journal 定期重放（含代理重启后），成功一条删一条。
async fn write_worker(
    mut rx: tokio::sync::mpsc::Receiver<WriteTask>,
    target: String,
    server_auth_token: Option<String>,
    client: SharedClient,
    journal_dir: String,
) {
    tracing::info!(target: "write_queue", "写队列 worker 已启动");

    // 启动先重放上次遗留的失败 journal
    journal_replay_all(&client, &target, &server_auth_token, std::path::Path::new(&journal_dir)).await;

    // 每 30s 触发一次 journal 重放（运行期间堆积的失败写入也能被补发）
    let mut replay_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    replay_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let task = tokio::select! {
            biased;
            Some(task) = rx.recv() => task,
            _ = replay_interval.tick() => {
                // 定期重放堆积的 journal（重放函数自建新 client 避免陈旧连接）
                journal_replay_all(&client, &target, &server_auth_token,
                    std::path::Path::new(&journal_dir)).await;
                continue;
            }
            else => break,
        };

        let started = Instant::now();
        let method = task.method.clone();

        // store_memory 注入了固定 memory_id 且后端 INSERT OR REPLACE，重试幂等安全。
        // 每次重试用新建 client（新连接），避免共享池中的陈旧连接反复 SendRequest 失败
        let mut attempt = 0;
        loop {
            let attempt_client = if attempt == 0 {
                client.clone()
            } else {
                build_http_client()
            };
            match forward_write_task(&attempt_client, &task, &target, &server_auth_token).await {
                // 非 2xx 同样是失败（此前 415 曾被当成功记 INFO，后端拒收但客户端已 ACK）
                Ok(status) if (200..300).contains(&status) => {
                    tracing::info!(
                        target: "write_queue",
                        method = %method,
                        status = status,
                        attempt,
                        elapsed_ms = started.elapsed().as_millis(),
                        "写操作已转发到后端"
                    );
                    break;
                }
                Ok(status) if attempt < 2 && (500..600).contains(&status) => {
                    attempt += 1;
                    tracing::warn!(target: "write_queue", method = %method, status = status, "后端 5xx，重试");
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
                Ok(status) => {
                    tracing::error!(
                        target: "write_queue",
                        method = %method,
                        status = status,
                        elapsed_ms = started.elapsed().as_millis(),
                        "写操作被后端拒绝，已落盘 journal"
                    );
                    journal_persist(std::path::Path::new(&journal_dir), &task);
                    break;
                }
                Err(e) if attempt < 2 && is_transient_legacy_error(&e) => {
                    attempt += 1;
                    tracing::warn!(
                        target: "write_queue",
                        method = %method,
                        error = %e,
                        attempt,
                        "写转发瞬时失败，用新连接重试"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(300 * attempt)).await;
                }
                Err(e) => {
                    tracing::error!(
                        target: "write_queue",
                        method = %method,
                        error = %e,
                        elapsed_ms = started.elapsed().as_millis(),
                        "写操作转发失败，已落盘 journal"
                    );
                    journal_persist(std::path::Path::new(&journal_dir), &task);
                    break;
                }
            }
        }
    }

    tracing::warn!(target: "write_queue", "写队列 worker 退出（channel 已关闭）");
}

/// 失败写请求落盘：JSON 文件 {method, body}，文件名带时间戳+uuid 防碰撞。
/// 落盘本身失败只告警（磁盘满等），不阻塞队列。
fn journal_persist(dir: &std::path::Path, task: &WriteTask) {
    let entry = serde_json::json!({
        "method": task.method,
        "body": String::from_utf8_lossy(&task.body),
    });
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::error!(target: "write_queue", error = %e, "创建 journal 目录失败");
        return;
    }
    let file = dir.join(format!(
        "{}-{}.json",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        uuid::Uuid::new_v4()
    ));
    match std::fs::write(&file, entry.to_string()) {
        Ok(()) => tracing::warn!(target: "write_queue", file = %file.display(), "写请求已落盘待重放"),
        Err(e) => tracing::error!(target: "write_queue", file = %file.display(), error = %e, "journal 落盘失败，该写请求丢失"),
    }
}

/// 重放 journal 目录中的全部遗留写请求：成功一条删一条，失败的保留等下次
async fn journal_replay_all(
    client: &SharedClient,
    target: &str,
    server_auth_token: &Option<String>,
    dir: &std::path::Path,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return, // 目录不存在 = 无遗留
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    if files.is_empty() {
        return;
    }
    tracing::warn!(target: "write_queue", count = files.len(), "发现遗留 journal，开始重放");
    for file in files {
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&content) else {
            tracing::error!(target: "write_queue", file = %file.display(), "journal 条目损坏，跳过");
            continue;
        };
        let task = WriteTask {
            method: entry["method"].as_str().unwrap_or("POST").to_string(),
            uri: String::new(),
            headers: axum::http::HeaderMap::new(),
            body: bytes::Bytes::from(entry["body"].as_str().unwrap_or("").to_string().into_bytes()),
        };
        // 每条用独立 client + 间隔：SSH 隧道对快速连续请求会关闭连接
        //（"connection closed before message completed"），独立 client 确保新 TCP 连接
        // 用传入 client 转发（真实的失败根因是服务端 char-boundary panic，已修）
        match forward_write_task(client, &task, target, server_auth_token).await {
            Ok(status) if (200..300).contains(&status) => {
                let _ = std::fs::remove_file(&file);
                tracing::info!(target: "write_queue", file = %file.display(), status = status, "journal 重放成功");
            }
            Ok(status) => {
                tracing::error!(target: "write_queue", file = %file.display(), status = status, "journal 重放被拒，保留文件");
            }
            Err(e) => {
                tracing::error!(target: "write_queue", file = %file.display(), error = %e, "journal 重放失败，保留文件");
            }
        }
        // 条目间隔 100ms，让 SSH 隧道有时间建立新连接
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// 判断 legacy client 错误是否为瞬时连接错误
fn is_transient_legacy_error(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    s.contains("SendRequest") || s.contains("Connect") || s.contains("channel closed")
}

/// 转发单个写任务到后端。
/// 注：曾误判为"大 body 在 SSH 隧道上必失败"，实为服务端请求日志中间件按字节
/// 截断中文 body 触发 char-boundary panic（已修 main.rs）。此处恢复常规 HTTP 客户端。
async fn forward_write_task(
    client: &SharedClient,
    task: &WriteTask,
    target: &str,
    server_auth_token: &Option<String>,
) -> Result<u16> {
    let target_uri: hyper::Uri = target.parse().map_err(|e| anyhow::anyhow!("目标地址无效: {}", e))?;

    let mut forward_req = axum::http::Request::builder()
        .method(task.method.as_str())
        .uri(target_uri)
        .body(axum::body::Body::from(task.body.clone()))?;

    // 复制 headers（跳过 hop-by-hop 与 content-length——body 已重新序列化注入
    // memory_id，长度与原始请求不同，陈旧 content-length 会导致后端 415；
    // hyper 会按实际 body 大小自动补 content-length）
    for (key, value) in &task.headers {
        if is_hop_by_hop(key.as_str()) || key.as_str().eq_ignore_ascii_case("content-length") {
            continue;
        }
        forward_req.headers_mut().insert(key.clone(), value.clone());
    }

    // 确保 Accept header 包含 MCP 所需的两种类型
    if !task.headers.contains_key(axum::http::header::ACCEPT) {
        forward_req.headers_mut().insert(
            axum::http::header::ACCEPT,
            axum::http::HeaderValue::from_static("application/json, text/event-stream"),
        );
    }

    // 确保 Content-Type（journal 重放的 task headers 为空，缺它后端 415）
    if !task.headers.contains_key(axum::http::header::CONTENT_TYPE) {
        forward_req.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
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

    let res = client.request(forward_req).await.map_err(|e| {
        // 输出完整错误链（hyper_util 的 Display 太简略，source 才有根因）
        let mut chain = vec![format!("{}", e)];
        let mut src = std::error::Error::source(&e);
        while let Some(s) = src {
            chain.push(format!("{}", s));
            src = std::error::Error::source(s);
        }
        anyhow::anyhow!("{} | chain: {}", e, chain.join(" -> "))
    })?;
    Ok(res.status().as_u16())
}

/// 解析目标 URL
fn parse_target(target: &str) -> Result<hyper::Uri, String> {
    target
        .parse::<hyper::Uri>()
        .map_err(|e| format!("无法解析 {}: {}", target, e))
}

/// 构建 HTTP 客户端（连接池）
/// pool_max_idle_per_host=1：限制每主机最多 1 条空闲连接，防止陈旧连接累积
/// pool_idle_timeout=10s：空闲 10 秒即驱逐，比 30s 更快清理死连接
fn build_http_client(
) -> hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body> {
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_nodelay(true);
    connector.enforce_http(true);

    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_max_idle_per_host(1)
        .pool_idle_timeout(std::time::Duration::from_secs(10))
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