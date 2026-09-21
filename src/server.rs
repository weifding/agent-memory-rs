//! MCP Server — 工具/资源注册与请求处理

use anyhow::Result;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, CacheScope, ContentBlock,
    ListResourcesResult, ListResourceTemplatesResult, ListToolsResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, ResultType, ServerCapabilities, ServerInfo, TextContent,
    Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::sync::Arc;

use crate::config::AppConfig;
use crate::embedding::{char_bigrams, EmbeddingClient};
use crate::models::{Memory, MemorySearchResult};
use crate::storage::SQLiteStorage;

#[cfg(feature = "graph")]
use crate::graph::{self, GraphDB};

/// 正文截断上限（与 Python 版 MAX_CONTENT_LENGTH 对齐）
const MAX_CONTENT_LENGTH: usize = 10_000;
/// 单次返回结果上限（与 Python 版 MAX_RESULTS 对齐）
const MAX_RESULTS: usize = 200;

pub struct MemoryHandler {
    pub storage: Arc<SQLiteStorage>,
    #[cfg(feature = "graph")]
    pub graph: Option<Arc<GraphDB>>,
    #[cfg(not(feature = "graph"))]
    pub graph: Option<Arc<()>>,
    pub embedding: Option<Arc<EmbeddingClient>>,
    pub config: AppConfig,
}

impl MemoryHandler {
    #[cfg(feature = "graph")]
    pub fn new(
        storage: Arc<SQLiteStorage>,
        graph: Option<Arc<GraphDB>>,
        embedding: Option<Arc<EmbeddingClient>>,
        config: AppConfig,
    ) -> Self {
        Self { storage, graph, embedding, config }
    }

    #[cfg(not(feature = "graph"))]
    pub fn new(
        storage: Arc<SQLiteStorage>,
        _graph: Option<Arc<()>>,
        embedding: Option<Arc<EmbeddingClient>>,
        config: AppConfig,
    ) -> Self {
        Self { storage, graph: None, embedding, config }
    }

    /// 生成文本嵌入：检测到 embedding 服务器时走 OpenAI 兼容接口（失败即报错，
    /// 避免伪嵌入向量混入真实向量空间），否则用字符 bigram 伪嵌入
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        if let Some(c) = &self.embedding {
            let mut v = c.embed(&[text]).await?;
            return Ok(v.remove(0));
        }
        Ok(crate::embedding::pseudo_embedding(text, self.storage.embedding_dim()))
    }
}

impl ServerHandler for MemoryHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.as_ref();
        let args = request.arguments.unwrap_or_default();
        let args: Value = serde_json::to_value(&args).unwrap_or(json!({}));
        tracing::info!(target: "mcp", tool = %name, arguments = %args, "tool call");

        let result = match name {
            "store_memory" => self.handle_store_memory(&args).await,
            "search_memory" => self.handle_search_memory(&args).await,
            "update_memory" => self.handle_update_memory(&args).await,
            "get_memory" => self.handle_get_memory(&args).await,
            "delete_memory" => self.handle_delete_memory(&args).await,
            "list_memories" => self.handle_list_memories(&args).await,
            "get_memory_stats" => self.handle_get_stats(&args).await,
            #[cfg(feature = "graph")]
            "graph_create_entity" => self.handle_graph_create_entity(&args).await,
            #[cfg(feature = "graph")]
            "graph_get_entity" => self.handle_graph_get_entity(&args).await,
            #[cfg(feature = "graph")]
            "graph_list_entities" => self.handle_graph_list_entities(&args).await,
            #[cfg(feature = "graph")]
            "graph_delete_entity" => self.handle_graph_delete_entity(&args).await,
            #[cfg(feature = "graph")]
            "graph_create_relation" => self.handle_graph_create_relation(&args).await,
            #[cfg(feature = "graph")]
            "graph_link_memory" => self.handle_graph_link_memory(&args).await,
            #[cfg(feature = "graph")]
            "graph_get_related_memories" => self.handle_graph_get_related_memories(&args).await,
            #[cfg(feature = "graph")]
            "graph_query" => self.handle_graph_query(&args).await,
            #[cfg(feature = "graph")]
            "graph_aggregate" => self.handle_graph_aggregate(&args).await,
            #[cfg(feature = "graph")]
            "graph_stats" => self.handle_graph_stats(&args).await,
            #[cfg(feature = "graph")]
            "graph_checkpoint" => self.handle_graph_checkpoint(&args).await,
            _ => Err(anyhow::anyhow!("Unknown tool: {}", name)),
        };

        let (text, is_error) = match result {
            Ok(t) => (t, false),
            Err(e) => (format!("Error: {}", e), true),
        };

        let content = vec![ContentBlock::Text(TextContent::new(text))];

        let result = if is_error {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        };
        Ok(CallToolResponse::Complete(result))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        tracing::info!(target: "mcp", "tools/list (client 请求工具列表)");
        let schema = |props: Value| {
            serde_json::from_value::<serde_json::Map<String, Value>>(props).unwrap_or_default()
        };

        let mut tools = vec![
            Tool::new(
                "store_memory",
                "Store a new memory. 单条记忆 content 上限 10000 字符，超长自动截断",
                schema(json!({"type":"object","properties":{"content":{"type":"string","maxLength":10000,"description":"记忆正文，单条上限 10000 字符，超长截断"},"namespace":{"type":"string"},"category":{"type":"string"},"tags":{"type":"array","items":{"type":"string"}},"importance":{"type":"number"}},"required":["content"]})),
            ),
            Tool::new(
                "search_memory",
                "Search memories by semantic similarity (中文友好)",
                schema(json!({"type":"object","properties":{"query":{"type":"string"},"namespace":{"type":"string"},"top_k":{"type":"integer"},"category":{"type":"string"}},"required":["query"]})),
            ),
            Tool::new(
                "update_memory",
                "Update an existing memory. 更新 content 时同样受 10000 字符上限，超长自动截断",
                schema(json!({"type":"object","properties":{"memory_id":{"type":"string"},"content":{"type":"string","maxLength":10000,"description":"更新后的正文，单条上限 10000 字符，超长截断"},"importance":{"type":"number"},"category":{"type":"string"}},"required":["memory_id"]})),
            ),
            Tool::new("get_memory", "Get a memory by ID", schema(json!({"type":"object","properties":{"memory_id":{"type":"string"}},"required":["memory_id"]}))),
            Tool::new("delete_memory", "Delete a memory by ID", schema(json!({"type":"object","properties":{"memory_id":{"type":"string"}},"required":["memory_id"]}))),
            Tool::new("list_memories", "List memories with pagination", schema(json!({"type":"object","properties":{"namespace":{"type":"string"},"limit":{"type":"integer"},"offset":{"type":"integer"},"category":{"type":"string"}}}))),
            Tool::new("get_memory_stats", "Get memory statistics", schema(json!({"type":"object","properties":{"namespace":{"type":"string"}}}))),
        ];

        #[cfg(feature = "graph")]
        if self.graph.is_some() {
            tools.extend([
                Tool::new("graph_create_entity", "Create a knowledge graph entity (label 可任意扩展：预设外 label 首次写入自动建表，属性键自动加列，值统一按字符串存储；label 须为 [A-Za-z_][A-Za-z0-9_]* 标识符)", schema(json!({"type":"object","properties":{"label":{"type":"string","pattern":"^[A-Za-z_][A-Za-z0-9_]*$"},"name":{"type":"string"},"properties":{"type":"object","description":"任意键值对，值按字符串存储","additionalProperties":true}},"required":["label","name"]}))),
                Tool::new("graph_get_entity", "Get a graph entity", schema(json!({"type":"object","properties":{"label":{"type":"string"},"name":{"type":"string"}},"required":["label","name"]}))),
                Tool::new("graph_list_entities", "List entities", schema(json!({"type":"object","properties":{"label":{"type":"string"},"limit":{"type":"integer"}},"required":["label"]}))),
                Tool::new("graph_delete_entity", "Delete a graph entity", schema(json!({"type":"object","properties":{"label":{"type":"string"},"name":{"type":"string"}},"required":["label","name"]}))),
                Tool::new("graph_create_relation", "Create a relation", schema(json!({"type":"object","properties":{"relation":{"type":"string"},"source_label":{"type":"string"},"source_name":{"type":"string"},"target_label":{"type":"string"},"target_name":{"type":"string"}},"required":["relation","source_label","source_name","target_label","target_name"]}))),
                Tool::new("graph_link_memory", "Link entity to memory", schema(json!({"type":"object","properties":{"entity_label":{"type":"string"},"entity_name":{"type":"string"},"memory_id":{"type":"string"},"content_preview":{"type":"string"}},"required":["entity_label","entity_name","memory_id"]}))),
                Tool::new("graph_get_related_memories", "Get memory IDs linked to an entity", schema(json!({"type":"object","properties":{"label":{"type":"string"},"name":{"type":"string"}},"required":["label","name"]}))),
                Tool::new("graph_query", "Execute read-only Cypher", schema(json!({"type":"object","properties":{"cypher":{"type":"string"}},"required":["cypher"]}))),
                Tool::new("graph_aggregate", "Aggregate by property", schema(json!({"type":"object","properties":{"label":{"type":"string"},"group_by":{"type":"string"}},"required":["label","group_by"]}))),
                Tool::new("graph_stats", "Get graph statistics", schema(json!({"type":"object"}))),
                Tool::new("graph_checkpoint", "Manually trigger Kùzu CHECKPOINT", schema(json!({"type":"object"}))),
            ]);
        }

        // SEP-2549：协议 2026-07-28+ 的客户端（如 ZCode discover 生命周期）把
        // ttlMs/cacheScope 当必填字段做 Zod 严格校验，缺失会导致整个 tools/list
        // 结果被拒收（老协议客户端不认识这两个字段，rmcp 会在服务层为老 peer 剥掉）。
        // 工具清单进程内静态，5 分钟 TTL 足够新鲜；Private = 仅同授权上下文可缓存。
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            result_type: Some(ResultType::COMPLETE),
            meta: None,
            ttl_ms: Some(300_000),
            cache_scope: Some(CacheScope::Private),
        })
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        tracing::info!(target: "mcp", "resources/list (client 请求资源列表)");
        Ok(ListResourcesResult {
            resources: vec![
                Resource::new("memory://stats", "全局记忆统计")
                    .with_description("全部命名空间的记忆统计信息"),
                Resource::new("memory://namespaces", "命名空间列表")
                    .with_description("当前所有命名空间"),
            ],
            next_cursor: None,
            result_type: Some(ResultType::COMPLETE),
            meta: None,
            ttl_ms: Some(300_000),
            cache_scope: Some(CacheScope::Private),
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult {
            resource_templates: vec![
                ResourceTemplate::new("memory://recent/{namespace}", "最近记忆")
                    .with_description("指定命名空间最近的记忆"),
                ResourceTemplate::new("memory://consolidations/{namespace}", "近期整合结果")
                    .with_description("指定命名空间近期的记忆整合（Rust 版暂返回空）"),
            ],
            next_cursor: None,
            result_type: Some(ResultType::COMPLETE),
            meta: None,
            ttl_ms: Some(300_000),
            cache_scope: Some(CacheScope::Private),
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let uri = request.uri.as_str();
        let read = |text: String| {
            ReadResourceResponse::Complete(ReadResourceResult::new(vec![
                ResourceContents::text(text, uri),
            ]))
        };
        let internal_err =
            |e: anyhow::Error| McpError::internal_error(format!("storage error: {}", e), None);

        match uri {
            "memory://stats" => {
                let stats = self.storage.get_stats(None).map_err(internal_err)?;
                Ok(read(serde_json::to_string_pretty(&stats).unwrap_or_default()))
            }
            "memory://namespaces" => {
                let namespaces = self.storage.list_namespaces().map_err(internal_err)?;
                Ok(read(serde_json::to_string_pretty(&namespaces).unwrap_or_default()))
            }
            _ => {
                if let Some(ns) = uri.strip_prefix("memory://recent/") {
                    let mems = self
                        .storage
                        .list_filtered(Some(ns), None, 10, 0)
                        .map_err(internal_err)?;
                    Ok(read(serde_json::to_string_pretty(&mems).unwrap_or_default()))
                } else if uri.starts_with("memory://consolidations/") {
                    // Rust 版尚无整合功能，返回空列表
                    Ok(read("[]".to_string()))
                } else {
                    Err(McpError::resource_not_found(uri.to_string(), None))
                }
            }
        }
    }
}

// 核心记忆工具
impl MemoryHandler {
    async fn handle_store_memory(&self, args: &Value) -> Result<String> {
        let content = args["content"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(MAX_CONTENT_LENGTH)
            .collect::<String>();
        if content.is_empty() {
            return Ok(json!({"error": "content is required"}).to_string());
        }
        let namespace = args["namespace"].as_str().unwrap_or("default").to_string();
        let category = args["category"].as_str().unwrap_or("fact").to_string();
        let importance = args["importance"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0);
        let tags: Vec<String> = args["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let mut memory = Memory::new(content, namespace.clone(), category.clone());
        memory.importance = importance;
        memory.topics = tags;

        // 规则抽取实体（词典 + 命名空间骨架），结果随记忆落 SQLite entities 列
        let hits = crate::extract::extract(
            &memory.content,
            &namespace,
            &memory.topics,
            &self.config.graph.rules,
        );
        memory.entities = hits.iter().map(|h| h.display()).collect();

        // 嵌入：embedding 服务器在线时用真实向量，否则字符 bigram 伪嵌入
        let embedding = self.embed_text(&memory.content).await?;
        let id = self.storage.store(&memory, Some(&embedding))?;
        tracing::info!(
            target: "memory::store",
            memory_id = %id,
            namespace = %namespace,
            category = %category,
            importance = importance,
            entities = ?memory.entities,
            content_preview = %memory.content.chars().take(60).collect::<String>(),
            "memory stored"
        );
        #[cfg(feature = "graph")]
        {
            // 自动入图：为每个命中实体建节点（幂等，label 表按需自动供给）并 link 到
            // MemoryRef；失败仅告警不影响记忆落库
            let mut graph_linked = 0usize;
            if let (Some(gdb), true) = (&self.graph, self.config.graph.auto_extract) {
                for hit in &hits {
                    let preview = memory.content.chars().take(200).collect::<String>();
                    if let Err(e) = graph::create_entity(
                        gdb,
                        &hit.label,
                        &hit.name,
                        &std::collections::HashMap::new(),
                    ) {
                        tracing::warn!(target: "graph", label=%hit.label, name=%hit.name, error=%e, "create_entity failed");
                        continue;
                    }
                    match graph::link_memory(gdb, &hit.label, &hit.name, &id, &preview) {
                        Ok(_) => graph_linked += 1,
                        Err(e) => tracing::warn!(target: "graph", label=%hit.label, name=%hit.name, error=%e, "link_memory failed"),
                    }
                }
                tracing::info!(target: "graph", memory_id=%id, linked=graph_linked, entities=?hits.iter().map(|h| h.display()).collect::<Vec<_>>(), "store auto-graph done");
            }
            let mut resp = json!({
                "memory_id": id,
                "namespace": namespace,
                "category": category,
                "status": "stored",
            });
            resp["graph_linked"] = json!(graph_linked);
            Ok(resp.to_string())
        }
        #[cfg(not(feature = "graph"))]
        {
            Ok(json!({
                "memory_id": id,
                "namespace": namespace,
                "category": category,
                "status": "stored",
            })
            .to_string())
        }
    }

    async fn handle_search_memory(&self, args: &Value) -> Result<String> {
        let query = args["query"].as_str().unwrap_or("");
        // namespace 缺省 = 全部命名空间（与 Python 版对齐）
        let namespace = args["namespace"].as_str().map(|s| s.to_string());
        let category = args["category"].as_str().map(|s| s.to_string());
        let top_k = (args["top_k"].as_u64().unwrap_or(10) as usize).min(MAX_RESULTS);

        let mut results: Vec<MemorySearchResult> = Vec::new();

        // ① 向量检索路径：库中存在向量时，用同一嵌入函数编码查询
        if !query.is_empty() && self.storage.has_vectors()? {
            let emb = self.embed_text(query).await?;
            let hits = self
                .storage
                .search(&emb, namespace.as_deref(), top_k, category.as_deref())?;
            if !hits.is_empty() {
                results = hits;
            }
        }

        // ② 文本相似度回退（中文友好的字符 bigram Jaccard）；空查询按时间倒序
        if results.is_empty() {
            let all = self.storage.list_filtered(
                namespace.as_deref(),
                category.as_deref(),
                MAX_RESULTS as i32,
                0,
            )?;
            let mut scored: Vec<(f64, Memory)> = if query.is_empty() {
                all.into_iter().map(|m| (0.0, m)).collect()
            } else {
                all.into_iter()
                    .map(|m| (text_similarity(query, &m.content), m))
                    .collect()
            };
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
            scored.truncate(top_k);
            results = scored
                .into_iter()
                .map(|(sim, memory)| MemorySearchResult { memory, similarity: sim })
                .collect();
        }

        let out: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "id": r.memory.id,
                    "content": r.memory.content,
                    "summary": r.memory.summary,
                    "similarity": (r.similarity * 10000.0).round() / 10000.0,
                    "category": r.memory.category,
                    "namespace": r.memory.namespace,
                    "importance": r.memory.importance,
                    "created_at": r.memory.created_at,
                })
            })
            .collect();
        tracing::info!(
            target: "memory::search",
            query = %query.chars().take(60).collect::<String>(),
            namespace = ?namespace,
            category = ?category,
            top_k = top_k,
            hits = results.len(),
            top_similarity = results.first().map(|r| (r.similarity * 10000.0).round() / 10000.0),
            "memory search"
        );
        Ok(serde_json::to_string_pretty(&out)?)
    }

    async fn handle_update_memory(&self, args: &Value) -> Result<String> {
        let memory_id = args["memory_id"].as_str().unwrap_or("");
        let content = args["content"]
            .as_str()
            .map(|s| s.chars().take(MAX_CONTENT_LENGTH).collect::<String>());
        let importance = args["importance"].as_f64().map(|v| v.clamp(0.0, 1.0));
        let category = args["category"].as_str().map(|s| s.to_string());

        let mut new_embedding = None;
        if let Some(c) = &content {
            if !c.is_empty() {
                new_embedding = Some(self.embed_text(c).await?);
            }
        }

        match self.storage.update(
            memory_id,
            content.as_deref(),
            importance,
            category.as_deref(),
            new_embedding.as_deref(),
        )? {
            Some(m) => {
                tracing::info!(
                    target: "memory::update",
                    memory_id = %m.id,
                    category = ?m.category,
                    importance = m.importance,
                    content_preview = %m.content.chars().take(60).collect::<String>(),
                    "memory updated"
                );
                Ok(json!({
                    "memory_id": m.id,
                    "status": "updated",
                    "content": m.content.chars().take(200).collect::<String>(),
                })
                .to_string())
            }
            None => {
                tracing::warn!(target: "memory::update", memory_id = %memory_id, "update failed: memory not found");
                Ok(json!({"error": format!("memory not found: {}", memory_id)}).to_string())
            }
        }
    }

    async fn handle_get_memory(&self, args: &Value) -> Result<String> {
        let id = args["memory_id"].as_str().unwrap_or("");
        match self.storage.get(id)? {
            Some(m) => {
                tracing::debug!(target: "memory::get", memory_id = %id, "memory fetched");
                Ok(serde_json::to_string_pretty(&m)?)
            }
            None => {
                tracing::warn!(target: "memory::get", memory_id = %id, "memory not found");
                Ok(json!({"error": "not_found"}).to_string())
            }
        }
    }

    async fn handle_delete_memory(&self, args: &Value) -> Result<String> {
        let id = args["memory_id"].as_str().unwrap_or("");
        let deleted = self.storage.delete(id)?;
        // 联动清理图谱 MemoryRef 节点及其全部边，防孤儿；失败仅告警不影响删除结果
        #[cfg(feature = "graph")]
        let graph_cleaned: bool = if let (Some(gdb), true) = (&self.graph, self.config.graph.enabled) {
            match graph::delete_memory_ref(gdb, id) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(target: "graph", memory_id = %id, error = %e, "delete_memory_ref failed");
                    false
                }
            }
        } else {
            false
        };
        #[cfg(not(feature = "graph"))]
        let graph_cleaned = false;
        if deleted {
            tracing::info!(
                target: "memory::delete",
                memory_id = %id,
                graph_ref_removed = graph_cleaned,
                "memory deleted"
            );
        } else {
            tracing::warn!(target: "memory::delete", memory_id = %id, "delete failed: memory not found");
        }
        Ok(json!({"memory_id": id, "status": if deleted { "deleted" } else { "not_found" }}).to_string())
    }

    async fn handle_list_memories(&self, args: &Value) -> Result<String> {
        let namespace = args["namespace"].as_str().unwrap_or("default").to_string();
        let limit = (args["limit"].as_u64().unwrap_or(20) as usize).min(MAX_RESULTS) as i32;
        let offset = args["offset"].as_u64().unwrap_or(0) as i32;
        let category = args["category"].as_str().map(|s| s.to_string());
        let memories = self.storage.list(&namespace, limit, offset, category.as_deref())?;
        let stats = self.storage.get_stats(Some(&namespace))?;
        Ok(json!({
            "memories": memories.iter().map(|m| json!({
                "id": m.id,
                "content": m.content.chars().take(200).collect::<String>(),
                "category": m.category,
                "importance": m.importance,
                "consolidated": m.consolidated,
                "created_at": m.created_at,
            })).collect::<Vec<_>>(),
            "total": stats.total_memories,
            "namespace": namespace,
            "limit": limit,
            "offset": offset,
        })
        .to_string())
    }

    async fn handle_get_stats(&self, args: &Value) -> Result<String> {
        let namespace = args["namespace"].as_str().map(|s| s.to_string());
        let stats = self.storage.get_stats(namespace.as_deref())?;
        Ok(serde_json::to_string_pretty(&stats)?)
    }
}

#[cfg(feature = "graph")]
impl MemoryHandler {
    fn graph(&self) -> Result<&GraphDB> {
        self.graph.as_deref().ok_or_else(|| anyhow::anyhow!("Graph not enabled"))
    }

    async fn handle_graph_create_entity(&self, args: &Value) -> Result<String> {
        let label = args["label"].as_str().unwrap_or("");
        let name = args["name"].as_str().unwrap_or("");
        // 属性值统一按字符串存储：数字/布尔转文本，null 丢弃，数组/对象序列化为 JSON
        let props: std::collections::HashMap<String, String> = args["properties"]
            .as_object().map(|o| {
                o.iter().filter_map(|(k, v)| {
                    let s = match v {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => n.to_string(),
                        Value::Bool(b) => b.to_string(),
                        Value::Null => return None,
                        other => other.to_string(),
                    };
                    Some((k.clone(), s))
                }).collect()
            })
            .unwrap_or_default();
        let status = graph::create_entity(self.graph()?, label, name, &props)?;
        tracing::info!(target: "graph::create_entity", label = %label, name = %name, status = %status, "graph entity created");
        Ok(json!({"status": status, "label": label, "name": name}).to_string())
    }

    async fn handle_graph_get_entity(&self, args: &Value) -> Result<String> {
        let label = args["label"].as_str().unwrap_or("");
        let name = args["name"].as_str().unwrap_or("");
        let entity = graph::get_entity(self.graph()?, label, name)?;
        Ok(json!({"found": entity.is_some(), "name": entity}).to_string())
    }

    async fn handle_graph_list_entities(&self, args: &Value) -> Result<String> {
        let label = args["label"].as_str().unwrap_or("");
        let limit = args["limit"].as_u64().unwrap_or(50) as usize;
        let entities = graph::list_entities(self.graph()?, label, limit)?;
        Ok(json!({"entities": entities, "count": entities.len()}).to_string())
    }

    async fn handle_graph_delete_entity(&self, args: &Value) -> Result<String> {
        let label = args["label"].as_str().unwrap_or("");
        let name = args["name"].as_str().unwrap_or("");
        let deleted = graph::delete_entity(self.graph()?, label, name)?;
        if deleted {
            tracing::info!(target: "graph::delete_entity", label = %label, name = %name, "graph entity deleted");
        }
        Ok(json!({"status": if deleted { "deleted" } else { "not_found" }}).to_string())
    }

    async fn handle_graph_create_relation(&self, args: &Value) -> Result<String> {
        let relation = args["relation"].as_str().unwrap_or("");
        let sl = args["source_label"].as_str().unwrap_or("");
        let sn = args["source_name"].as_str().unwrap_or("");
        let tl = args["target_label"].as_str().unwrap_or("");
        let tn = args["target_name"].as_str().unwrap_or("");
        let status = graph::create_relation(self.graph()?, relation, sl, sn, tl, tn)?;
        tracing::info!(target: "graph::create_relation", relation = %relation, source = %format!("{}:{}", sl, sn), target = %format!("{}:{}", tl, tn), status = %status, "graph relation created");
        Ok(json!({"status": status, "relation": relation}).to_string())
    }

    async fn handle_graph_link_memory(&self, args: &Value) -> Result<String> {
        let el = args["entity_label"].as_str().unwrap_or("");
        let en = args["entity_name"].as_str().unwrap_or("");
        let mid = args["memory_id"].as_str().unwrap_or("");
        let cp = args["content_preview"].as_str().unwrap_or("");
        let status = graph::link_memory(self.graph()?, el, en, mid, cp)?;
        tracing::info!(target: "graph::link_memory", entity = %format!("{}:{}", el, en), memory_id = %mid, status = %status, "memory linked to graph entity");
        Ok(json!({"status": status}).to_string())
    }

    async fn handle_graph_get_related_memories(&self, args: &Value) -> Result<String> {
        let label = args["label"].as_str().unwrap_or("");
        let name = args["name"].as_str().unwrap_or("");
        let ids = graph::get_related_memories(self.graph()?, label, name)?;
        Ok(json!({"memory_ids": ids, "count": ids.len()}).to_string())
    }

    async fn handle_graph_query(&self, args: &Value) -> Result<String> {
        let cypher = args["cypher"].as_str().unwrap_or("");
        let (columns, rows) = self.graph()?.query(cypher)?;
        tracing::info!(target: "graph::query", rows = rows.len(), cypher = %cypher.chars().take(100).collect::<String>(), "graph cypher query executed");
        let results: Vec<Value> = rows.iter().map(|row| {
            let mut obj = serde_json::Map::new();
            for (i, val) in row.iter().enumerate() {
                let key = columns.get(i).cloned().unwrap_or_else(|| format!("col{}", i));
                obj.insert(key, kuzu_value_to_json(val));
            }
            Value::Object(obj)
        }).collect();
        Ok(json!({"results": results, "count": results.len()}).to_string())
    }

    async fn handle_graph_aggregate(&self, args: &Value) -> Result<String> {
        let label = args["label"].as_str().unwrap_or("");
        let group_by = args["group_by"].as_str().unwrap_or("");
        let groups = graph::aggregate_by_property(self.graph()?, label, group_by)?;
        let result: Vec<_> = groups.iter().map(|(k, c)| json!({"key": k, "count": c})).collect();
        Ok(json!({"groups": result, "count": result.len()}).to_string())
    }

    async fn handle_graph_stats(&self, _args: &Value) -> Result<String> {
        let stats = graph::count_entities(self.graph()?)?;
        Ok(serde_json::to_string_pretty(&stats)?)
    }

    async fn handle_graph_checkpoint(&self, _args: &Value) -> Result<String> {
        self.graph()?.checkpoint()?;
        tracing::info!(target: "graph::checkpoint", "graph checkpoint completed");
        Ok(json!({"status": "checkpoint_completed"}).to_string())
    }
}

#[cfg(feature = "graph")]
fn kuzu_value_to_json(v: &kuzu::Value) -> Value {
    match v {
        kuzu::Value::Null(_) => Value::Null,
        kuzu::Value::Bool(b) => Value::Bool(*b),
        kuzu::Value::Int64(i) => Value::from(*i),
        kuzu::Value::Int32(i) => Value::from(*i),
        kuzu::Value::Int16(i) => Value::from(*i),
        kuzu::Value::UInt64(i) => Value::from(*i),
        kuzu::Value::UInt32(i) => Value::from(*i),
        kuzu::Value::Double(f) => json!(*f),
        kuzu::Value::Float(f) => json!(*f),
        kuzu::Value::String(s) => Value::String(s.clone()),
        kuzu::Value::List(_, l) => Value::Array(l.iter().map(kuzu_value_to_json).collect()),
        _ => Value::String(format!("{:?}", v)),
    }
}

/// 字符 bigram Jaccard 相似度（修复原按空格分词导致中文全部失效的问题）
fn text_similarity(query: &str, text: &str) -> f64 {
    let q = char_bigrams(query);
    let t = char_bigrams(text);
    if q.is_empty() {
        return 0.0;
    }
    let inter = q.intersection(&t).count();
    inter as f64 / (q.len() + t.len() - inter) as f64
}

/// 字符 bigram 哈希伪嵌入（L2 归一化）。
/// 写入与查询使用同一函数即构成端到端向量检索；接入真实 embedding 提供方后替换此函数即可。
fn pseudo_embedding(text: &str, dim: usize) -> Vec<f32> {
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
