//! agent-memory-server — Rust 实现

use anyhow::Result;
use clap::Parser;
use rmcp::ServiceExt;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use agent_memory_server::config;
use agent_memory_server::server::MemoryHandler;
use agent_memory_server::storage::SQLiteStorage;

#[derive(Parser, Debug)]
#[command(name = "agent-memory-server", version, about = "MCP memory server for AI agents (Rust)")]
struct Cli {
    #[arg(long)]
    config: Option<String>,
    #[arg(long)]
    transport: Option<String>,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = config::load_config(cli.config.as_deref())?;

    // 日志必须写 stderr：MCP stdio 传输下 stdout 是 JSON-RPC 协议流，任何日志混入都会导致客户端解析失败
    // 默认级别取 config.log_level（INFO），可用 RUST_LOG 覆盖（支持 http/mcp 等 target 定向）
    let filter = std::env::var("RUST_LOG")
        .map(EnvFilter::new)
        .unwrap_or_else(|_| EnvFilter::new(&config.log_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    if let Some(t) = cli.transport {
        config.server.transport = t;
    }
    if let Some(h) = cli.host {
        config.server.http_host = h;
    }
    if let Some(p) = cli.port {
        config.server.http_port = p;
    }

    let storage = SQLiteStorage::open(
        &config.storage.resolved_path(),
        config.embedding.dimensions as usize,
    )?;
    let storage = Arc::new(storage);

    // Embedding 提供方：显式配置直接采用，否则自动探测本地 OpenAI 兼容服务
    //（LM Studio / Ollama / vLLM / Xinference / llama.cpp），命中即自动配置；
    // 向量空间变化时自动重嵌存量记忆（见 embedding::setup）
    let embedding = agent_memory_server::embedding::setup(&mut config, &storage).await?;

    // 图谱初始化（仅在启用 graph feature 且配置开启时）
    #[cfg(feature = "graph")]
    let graph: Option<Arc<agent_memory_server::graph::GraphDB>> = if config.graph.enabled {
        use agent_memory_server::graph::{init_schema, GraphDB};
        let gdb = GraphDB::open(
            &config.graph.resolved_path(),
            config.graph.buffer_pool_size_mb,
        )?;
        init_schema(&gdb)?;
        tracing::info!("Knowledge graph initialized");
        Some(Arc::new(gdb))
    } else {
        None
    };
    #[cfg(not(feature = "graph"))]
    let graph: Option<Arc<()>> = None;

    let handler = MemoryHandler::new(storage.clone(), graph.clone(), embedding.clone(), config.clone());

    // 启动回填：把存量记忆按同一套抽取规则补建图谱（create_entity / link_memory 均幂等，可安全重复）
    #[cfg(feature = "graph")]
    if let (Some(gdb), true) = (&graph, config.graph.enabled && config.graph.backfill_on_start) {
        use agent_memory_server::{extract, graph as graph_api};
        let memories = storage.list_filtered(None, None, 100_000, 0)?;
        let mut linked_memories = 0usize;
        let mut linked_edges = 0usize;
        for mem in &memories {
            let hits = extract::extract(&mem.content, &mem.namespace, &mem.topics, &config.graph.rules);
            let mut linked = 0usize;
            for hit in &hits {
                let preview = mem.content.chars().take(200).collect::<String>();
                let _ = graph_api::create_entity(
                    gdb,
                    &hit.label,
                    &hit.name,
                    &std::collections::HashMap::new(),
                );
                if graph_api::link_memory(gdb, &hit.label, &hit.name, &mem.id, &preview).is_ok() {
                    linked += 1;
                }
            }
            if linked > 0 {
                linked_memories += 1;
                linked_edges += linked;
            }
        }
        tracing::info!(
            target: "graph",
            total_memories = memories.len(),
            linked_memories,
            linked_edges,
            "graph backfill done"
        );

        // 孤儿清扫：图谱中存在但 SQLite 已删除的 MemoryRef 连边一起删掉
        let live_ids: std::collections::HashSet<String> = memories.iter().map(|m| m.id.clone()).collect();
        let mut orphans_removed = 0usize;
        if let Ok((_, rows)) = gdb.query("MATCH (m:MemoryRef) RETURN m.memory_id AS id") {
            for row in rows {
                if let Some(kuzu::Value::String(id)) = row.first() {
                    if !live_ids.contains(id) {
                        if graph_api::delete_memory_ref(gdb, id).unwrap_or(false) {
                            orphans_removed += 1;
                        }
                    }
                }
            }
        }
        if orphans_removed > 0 {
            tracing::info!(target: "graph", removed = orphans_removed, "orphan MemoryRef sweep done");
        }
    }

    tracing::info!(
        "Starting agent-memory-server (transport={})",
        config.server.transport,
    );

    match config.server.transport.as_str() {
        "stdio" => {
            tracing::info!("Starting stdio MCP server");
            let running = handler
                .serve((tokio::io::stdin(), tokio::io::stdout()))
                .await?;
            running.waiting().await?;
        }
        "http" | "streamable-http" => {
            use rmcp::transport::streamable_http_server::{
                session::local::LocalSessionManager, StreamableHttpServerConfig,
                StreamableHttpService,
            };

            // 安全约束：绑定非回环地址时必须设置 auth_token，否则拒绝启动
            let is_loopback =
                ["127.0.0.1", "::1", "localhost"].contains(&config.server.http_host.as_str());
            let auth_token = config.server.auth_token.clone();
            if !is_loopback && auth_token.as_deref().map(str::is_empty).unwrap_or(true) {
                anyhow::bail!(
                    "HTTP transport bound to {} requires server.auth_token (or bind 127.0.0.1)",
                    config.server.http_host
                );
            }

            // 每个 HTTP 会话由 service_factory 创建独立的 MemoryHandler；
            // Storage/GraphDB/EmbeddingClient 通过 Arc 共享（Kùzu 保持单进程单连接）
            let factory_storage = storage.clone();
            let factory_graph = graph.clone();
            let factory_embedding = embedding.clone();
            let factory_config = config.clone();
            let service = StreamableHttpService::new(
                move || {
                    Ok(MemoryHandler::new(
                        factory_storage.clone(),
                        factory_graph.clone(),
                        factory_embedding.clone(),
                        factory_config.clone(),
                    ))
                },
                std::sync::Arc::new(LocalSessionManager::default()),
                StreamableHttpServerConfig::default()
                    // 与 ZCode 等客户端兼容：走 stateless + JSON 内联响应，
                    // 规避该 fork 在 SSE GET 流上的响应分发缺陷（响应丢失导致工具列表超时）
                    .with_legacy_session_mode(false)
                    .with_json_response(true),
            );

            let router = axum::Router::new().nest_service("/mcp", service);

            // Bearer Token 鉴权（配置了 auth_token 时启用）
            let router = match auth_token {
                Some(token) => {
                    use axum::{extract::Request, http::StatusCode, middleware};
                    router.layer(middleware::from_fn(
                        move |req: Request<axum::body::Body>, next: middleware::Next| {
                            let expected = format!("Bearer {}", token);
                            async move {
                                let ok = req
                                    .headers()
                                    .get(axum::http::header::AUTHORIZATION)
                                    .and_then(|v| v.to_str().ok())
                                    .is_some_and(|v| v == expected);
                                if ok {
                                    Ok(next.run(req).await)
                                } else {
                                    Err((
                                        StatusCode::UNAUTHORIZED,
                                        "Missing or invalid Authorization header",
                                    ))
                                }
                            }
                        },
                    ))
                }
                None => router,
            };

            // 请求日志中间件：记录每个 HTTP 请求的方法/路径/关键头（含 mcp-session-id、
            // mcp-protocol-version、Authorization 掩码）与 POST 请求体，便于追踪
            // ZCode 等客户端的真实调用路径与传送参数。GET(SSE) 长连接只在建立时记录一次。
            use std::time::Instant;
            let router = router.layer(axum::middleware::from_fn(
                |req: axum::extract::Request<axum::body::Body>,
                 next: axum::middleware::Next| async move {
                    let started = Instant::now();
                    let (parts, body) = req.into_parts();
                    let bytes = axum::body::to_bytes(body, 4 * 1024 * 1024)
                        .await
                        .unwrap_or_default();

                    let mut headers = Vec::new();
                    for key in [
                        "mcp-protocol-version",
                        "mcp-session-id",
                        "mcp-request-id",
                        "last-event-id",
                        "content-type",
                        "accept",
                        "user-agent",
                        "origin",
                    ] {
                        if let Some(v) = parts.headers.get(key).and_then(|v| v.to_str().ok()) {
                            headers.push(format!("{}={}", key, v));
                        }
                    }
                    let auth = parts
                        .headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(|a| {
                            if a.contains("Bearer ") {
                                "Bearer ***".to_string()
                            } else {
                                a.to_string()
                            }
                        })
                        .unwrap_or_else(|| "none".to_string());

                    let body_text = if parts.method == axum::http::Method::POST {
                        let t = String::from_utf8_lossy(&bytes);
                        if t.len() > 2000 { format!("{}…[+{}B]", &t[..2000], t.len() - 2000) } else { t.to_string() }
                    } else {
                        String::new()
                    };
                    tracing::info!(
                        target: "http",
                        method = %parts.method,
                        uri = %parts.uri,
                        headers = %headers.join(" | "),
                        auth,
                        "=> HTTP request"
                    );
                    if !body_text.is_empty() {
                        tracing::info!(target: "http", "   body: {}", body_text);
                    }

                    let req = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
                    let res = next.run(req).await;
                    tracing::info!(
                        target: "http",
                        status = res.status().as_u16(),
                        elapsed_ms = started.elapsed().as_millis(),
                        "<= HTTP response"
                    );
                    res
                },
            ));

            let addr = format!("{}:{}", config.server.http_host, config.server.http_port);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            tracing::info!("HTTP MCP server listening on http://{}/mcp", addr);
            axum::serve(listener, router).await?;
        }
        other => {
            anyhow::bail!(
                "Unsupported transport: {} (supported: stdio, http)",
                other
            );
        }
    }

    Ok(())
}
