//! MCP (Model Context Protocol) server over stdio.
//!
//! Exposes the knowledge base to Claude Code as read-only tools, so an agent can
//! query the domain model without shelling out to `catlas query` — plus exactly
//! one write path, [`propose_insight`], which lands a session's business claim
//! as a `candidate` insight that only a human can promote (`catlas review`).
//! The rest of the messages a tool server needs are handled by hand against
//! `serde_json` — mirroring the project's no-heavy-dependency ethos (the LLM
//! client is a hand-rolled `curl` call for the same reason).
//!
//! Transport: JSON-RPC 2.0, one message per line on stdin/stdout. Notifications
//! (no `id`) get no reply; requests echo their `id` back verbatim.

use crate::search;
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::Path;

const PROTOCOL_VERSION: &str = "2025-06-18";

/// Serve MCP on stdio until the client closes stdin.
pub fn run(root: &Path) -> Result<()> {
    let store = Store::open_existing(root)?;
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle_message(&store, &line) {
            writeln!(out, "{resp}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Handle one incoming message, returning the JSON-RPC reply to write back.
///
/// `None` means "no reply": notifications and anything unparseable are answered
/// with silence, per the JSON-RPC convention.
fn handle_message(store: &Store, line: &str) -> Option<String> {
    let msg: Value = serde_json::from_str(line).ok()?;
    let id = msg.get("id").cloned();
    let method = msg.get("method")?.as_str()?;
    let params = msg.get("params").cloned();

    // Notifications carry no id; never reply to them.
    if id.is_none() {
        return None;
    }

    let result = match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "catlas", "version": env!("CARGO_PKG_VERSION") },
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tool_defs() }),
        "tools/call" => return Some(tool_call(store, id, params)),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") },
            })
            .to_string())
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string())
}

/// The tool catalogue, matching the three read-only handlers below.
fn tool_defs() -> Vec<Value> {
    vec![
        json!({
            "name": "search",
            "description": "全文检索知识库：符号、数据表、入口、业务领域、术语、git 提交信息。支持中文子串与英文标识符（不区分大小写）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "检索关键词，中文或代码标识符" },
                    "limit": { "type": "integer", "description": "最多返回条数", "default": 20 },
                },
                "required": ["query"],
            },
        }),
        json!({
            "name": "domains",
            "description": "列出已划分的业务领域：键、标签、置信度、文件/表/入口计数。",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "status",
            "description": "知识库各层统计：模块/文件/符号/数据表/入口/领域/链路/术语/文档等计数。",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "propose_insight",
            "description": "沉淀业务洞察：分析代码后得出的、已验证的业务结论（规则/坑/术语），\
                            附代码位置，人工确认后进入检索供后续使用。只提交你从代码中核实过的主张\
                            ——不确定的推断、猜测、待办不要提交，这不是笔记工具。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["business_rule", "landmine", "term"],
                              "description": "business_rule=业务规则, landmine=坑/易踩陷阱, term=业务术语" },
                    "title": { "type": "string", "description": "一句话结论（≤120 字符）" },
                    "body": { "type": "string", "description": "Markdown：条件、例外、适用范围" },
                    "domain": { "type": "string", "description": "所属领域 key（可选，见 domains 工具）" },
                    "evidence": {
                        "type": "array", "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "file": { "type": "string", "description": "仓库内相对路径（必填）" },
                                "symbol": { "type": "string", "description": "符号名/fqn（可选）" },
                                "lines": { "type": "string", "description": "行号或行号范围，如 120-145（可选）" },
                            },
                            "required": ["file"],
                        },
                        "description": "代码证据，至少一条——无证据的结论无法复核",
                    },
                },
                "required": ["kind", "title", "body", "evidence"],
            },
        }),
    ]
}

/// Dispatch `tools/call`, always replying (tool execution errors go in the result,
/// not the JSON-RPC layer).
fn tool_call(store: &Store, id: Option<Value>, params: Option<Value>) -> String {
    let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
    let err = |msg: String| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32602, "message": msg },
        })
        .to_string()
    };

    let Some(params) = params else { return err("missing params".into()) };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return err("missing params.name".into());
    };
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    // An unknown tool is a protocol error (the client named something the
    // server does not have); a tool that exists but fails at runtime reports
    // `isError: true` in its result instead.
    if !matches!(name, "search" | "domains" | "status" | "propose_insight") {
        return err(format!("Unknown tool: {name}"));
    }
    let outcome: Result<Value, String> = match name {
        "search" => search_tool(store, &args),
        "domains" => domains_tool(store),
        "status" => status_tool(store),
        "propose_insight" => propose_insight_tool(store, &args),
        _ => unreachable!("guarded above"),
    };

    match outcome {
        Ok(v) => {
            let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "null".into());
            reply(json!({ "content": [{ "type": "text", "text": text }], "isError": false }))
        }
        Err(msg) => reply(json!({
            "content": [{ "type": "text", "text": msg }],
            "isError": true,
        })),
    }
}

fn search_tool(store: &Store, args: &Value) -> Result<Value, String> {
    let q = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| "参数 query 缺失".to_string())?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .min(100) as usize;
    let project_id = store.project_id().map_err(|e| e.to_string())?;

    // Same lazily-populate path as `catlas query`: a knowledge base built before
    // search existed has an empty index until the first read fills it.
    let n: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM search_fts", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if n == 0 {
        search::rebuild(store, project_id).map_err(|e| e.to_string())?;
    }
    search::query(store, project_id, q, limit)
        .map(|hits| serde_json::to_value(&hits).unwrap_or(Value::Null))
        .map_err(|e| e.to_string())
}

fn domains_tool(store: &Store) -> Result<Value, String> {
    let project_id = store.project_id().map_err(|e| e.to_string())?;
    let rows: Vec<(String, Option<String>, f64, i64, i64, i64)> = store
        .conn
        .prepare(
            "SELECT d.key, d.label, d.confidence,
                (SELECT COUNT(*) FROM domain_members m WHERE m.domain_id=d.id AND m.kind='file'),
                (SELECT COUNT(*) FROM domain_members m WHERE m.domain_id=d.id AND m.kind='table'),
                (SELECT COUNT(*) FROM domain_members m WHERE m.domain_id=d.id AND m.kind='entrypoint')
             FROM domains d WHERE d.project_id = ?1 ORDER BY 4 DESC",
        )
        .map_err(|e| e.to_string())?
        .query_map(params![project_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())?;
    Ok(Value::Array(
        rows.iter()
            .map(|(k, l, c, f, t, e)| {
                json!({ "key": k, "label": l, "confidence": c, "files": f, "tables": t, "entrypoints": e })
            })
            .collect(),
    ))
}

fn status_tool(store: &Store) -> Result<Value, String> {
    let mut out = serde_json::Map::new();
    for (label, table) in [
        ("modules", "modules"),
        ("files", "files"),
        ("symbols", "symbols"),
        ("refs", "refs"),
        ("tables", "tables"),
        ("columns", "columns"),
        ("table_access", "table_access"),
        ("entrypoints", "entrypoints"),
        ("config_props", "config_props"),
        ("commits", "git_commits"),
        ("domains", "domains"),
        ("traces", "traces"),
        ("notes", "notes"),
        ("glossary", "glossary"),
    ] {
        let n = store.count(table).map_err(|e| e.to_string())?;
        out.insert(label.to_string(), Value::Number(n.into()));
    }
    // Insight review state: an operator checking whether there is anything to
    // confirm wants it in the same place as the other counts.
    let kinds = crate::insight::KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    for (label, status) in [("insights_pending", "candidate"), ("insights_confirmed", "confirmed")] {
        let n: i64 = store
            .conn
            .query_row(
                &format!("SELECT COUNT(*) FROM notes WHERE kind IN ({kinds}) AND status = ?1"),
                params![status],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        out.insert(label.to_string(), Value::Number(n.into()));
    }
    Ok(Value::Object(out))
}

/// The one write path in the server: a session proposes a business claim, it
/// lands as `candidate`, and only a human (via `catlas review`) can promote it.
fn propose_insight_tool(store: &Store, args: &Value) -> Result<Value, String> {
    let ins: crate::insight::Insight = serde_json::from_value(args.clone())
        .map_err(|e| format!("参数不合法：{e}"))?;
    let project_id = store.project_id().map_err(|e| e.to_string())?;
    let id = crate::insight::propose(store, project_id, &ins).map_err(|e| e.to_string())?;

    // Point the session at near-duplicates up front — the reviewer sees them
    // later, but the model can withdraw a redundant proposal immediately.
    let similar = crate::search::query(store, project_id, &ins.title, 5)
        .map(|hits| {
            hits.into_iter()
                .filter(|h| h.kind == "note")
                .map(|h| json!({ "title": h.title, "file": h.file }))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut out = json!({
        "id": id,
        "status": "candidate",
        "note": format!("已记录为候选洞察，等待人工确认（catlas review --accept {id}）后进入检索"),
    });
    if !similar.is_empty() {
        out["similar_confirmed"] = Value::Array(similar);
        out["note"] = json!("已记录为候选，但检索到相近的已有知识，确认时会提示复核是否重复");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_data() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let pid = store.ensure_project("p", dir.path()).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO files(project_id, path, lang, sha256, loc) VALUES (?1, 'a/A.java', 'java', 'x', 1)",
                params![pid],
            )
            .unwrap();
        let fid = store.conn.last_insert_rowid();
        store
            .conn
            .execute(
                "INSERT INTO symbols(project_id, file_id, kind, name, fqn, doc, start_line, end_line)
                 VALUES (?1, ?2, 'enum_member', 'TEJIA', 'a.A.TEJIA', '特价活动说明', 1, 1)",
                params![pid, fid],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO domains(project_id, key, label, confidence) VALUES (?1, 'coupon', '优惠券', 1.0)",
                params![pid],
            )
            .unwrap();
        (dir, store)
    }

    #[test]
    fn initialize_declares_protocol_and_tools_capability() {
        let (_d, store) = store_with_data();
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp: Value = serde_json::from_str(&handle_message(&store, line).unwrap()).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(resp["result"]["serverInfo"]["name"], "catlas");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_exposes_three_tools() {
        let (_d, store) = store_with_data();
        let line = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let resp: Value = serde_json::from_str(&handle_message(&store, line).unwrap()).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["search", "domains", "status", "propose_insight"]);
    }

    #[test]
    fn notifications_get_no_reply() {
        let (_d, store) = store_with_data();
        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(handle_message(&store, line).is_none());
    }

    #[test]
    fn search_tool_returns_a_hit() {
        let (_d, store) = store_with_data();
        let line = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"特价活动"}}}"#;
        let resp: Value = serde_json::from_str(&handle_message(&store, line).unwrap()).unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let hits: Vec<Value> = serde_json::from_str(text).unwrap();
        assert!(hits.iter().any(|h| h["title"].as_str().unwrap().contains("TEJIA")));
    }

    #[test]
    fn domains_tool_lists_domains() {
        let (_d, store) = store_with_data();
        let line = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"domains"}}"#;
        let resp: Value = serde_json::from_str(&handle_message(&store, line).unwrap()).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let doms: Vec<Value> = serde_json::from_str(text).unwrap();
        assert_eq!(doms[0]["key"], "coupon");
        assert_eq!(doms[0]["label"], "优惠券");
    }

    #[test]
    fn unknown_tool_is_a_protocol_error() {
        let (_d, store) = store_with_data();
        let line = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope"}}"#;
        let resp: Value = serde_json::from_str(&handle_message(&store, line).unwrap()).unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn propose_insight_lands_as_candidate() {
        let (_d, store) = store_with_data();
        let line = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{
            "name":"propose_insight",
            "arguments":{
                "kind":"landmine",
                "title":"优惠券核销不校验门店归属",
                "body":"核销入口只查状态，门店过滤在调用方，漏传即跨门店核销。",
                "evidence":[{"file":"a/A.java","symbol":"A.verify","lines":"10-20"}]
            }}}"#;
        let resp: Value = serde_json::from_str(&handle_message(&store, line).unwrap()).unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["status"], "candidate");
        assert!(v["note"].as_str().unwrap().contains("人工确认"));
    }

    #[test]
    fn propose_insight_without_evidence_is_a_tool_error() {
        let (_d, store) = store_with_data();
        let line = r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{
            "name":"propose_insight",
            "arguments":{"kind":"term","title":"无证据","body":"x","evidence":[]}}}"#;
        let resp: Value = serde_json::from_str(&handle_message(&store, line).unwrap()).unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("证据"), "{text}");
    }

    #[test]
    fn there_is_no_mcp_path_to_confirmation() {
        // The client that proposes must not be able to confirm: the write
        // surface of the server is proposal only.
        let (_d, store) = store_with_data();
        let line = r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{
            "name":"confirm_insight","arguments":{"id":1}}}"#;
        let resp: Value = serde_json::from_str(&handle_message(&store, line).unwrap()).unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn unknown_method_is_a_protocol_error() {
        let (_d, store) = store_with_data();
        let line = r#"{"jsonrpc":"2.0","id":6,"method":"bogus/method"}"#;
        let resp: Value = serde_json::from_str(&handle_message(&store, line).unwrap()).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }
}
