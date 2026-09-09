//! 记忆 → 图谱实体的规则抽取引擎（纯字符串规则，无 kuzu 依赖）
//!
//! 抽取策略（两层规则）：
//! 1. 词典层：内置 + 配置追加的关键词规则，命中 content / topics 即产出 (label, entity)；
//! 2. 骨架层：非 default 命名空间自动视为一个 Project 实体，作为聚合骨架。
//!
//! 抽取结果只做确定性去重；label 合法性由调用侧（server.rs / main.rs，
//! graph feature 开启时）对照 `graph::NODE_LABELS` 再校验。

use serde::{Deserialize, Serialize};

/// 配置追加的抽取规则（对应 YAML `graph.rules`，追加到内置词典之后）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractRuleConf {
    pub label: String,
    pub entity: String,
    pub keywords: Vec<String>,
}

/// 抽取命中的实体（label 必须落入图谱节点白名单才有效）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityHit {
    pub label: String,
    pub name: String,
}

impl EntityHit {
    /// 落 SQLite entities 列的展示格式："label:name"
    pub fn display(&self) -> String {
        format!("{}:{}", self.label, self.name)
    }
}

/// 抽取规则（&str 版本，供内置默认词典用）
pub struct Rule<'a> {
    pub label: &'a str,
    pub entity: &'a str,
    pub keywords: &'a [&'a str],
}

/// 内置默认词典：针对本机现有记忆语料预置的稳定实体。
/// 关键词同时用于匹配 content 与 topics（大小写不敏感，子串命中）。
pub const DEFAULT_RULES: &[Rule] = &[
    Rule { label: "Project", entity: "Kuzu_agent_memory", keywords: &["kuzu_agent_memory", "agent-memory-rs", "agent memory"] },
    Rule { label: "Project", entity: "usb-agent-station", keywords: &["usb-agent-station", "usb_agent_station", "智能体工作站"] },
    Rule { label: "System", entity: "ZCode", keywords: &["zcode"] },
    Rule { label: "System", entity: "Kùzu", keywords: &["kuzu", "kùzu", "graph.kuzu"] },
    Rule { label: "System", entity: "agent-memory-server", keywords: &["agent-memory-server", "memory server"] },
    Rule { label: "System", entity: "macOS", keywords: &["macos", "launchd", "anaconda", "brew"] },
    Rule { label: "Interface", entity: "MCP", keywords: &["mcp", "json-rpc"] },
];

fn push_unique(hits: &mut Vec<EntityHit>, label: &str, name: &str) {
    let hit = EntityHit { label: label.to_string(), name: name.to_string() };
    if !hits.contains(&hit) {
        hits.push(hit);
    }
}

fn rule_matched(content_l: &str, topics: &[String], keywords: &[&str]) -> bool {
    keywords.iter().any(|k| {
        let k = k.to_lowercase();
        content_l.contains(&k) || topics.iter().any(|t| t.to_lowercase().contains(&k))
    })
}

/// 执行抽取，返回去重后的实体命中列表。
pub fn extract(
    content: &str,
    namespace: &str,
    topics: &[String],
    extra_rules: &[ExtractRuleConf],
) -> Vec<EntityHit> {
    let mut hits: Vec<EntityHit> = Vec::new();
    let content_l = content.to_lowercase();

    for rule in DEFAULT_RULES {
        if rule_matched(&content_l, topics, rule.keywords) {
            push_unique(&mut hits, rule.label, rule.entity);
        }
    }
    for rule in extra_rules {
        let keywords: Vec<&str> = rule.keywords.iter().map(|s| s.as_str()).collect();
        if rule_matched(&content_l, topics, &keywords) {
            push_unique(&mut hits, &rule.label, &rule.entity);
        }
    }

    // 骨架层：非 default 命名空间整体视为一个 Project 聚合实体
    if namespace != "default" && !namespace.is_empty() {
        push_unique(&mut hits, "Project", namespace);
    }

    hits
}
