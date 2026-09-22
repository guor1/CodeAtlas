//! Evidence packs: the context assembled for one L2 task.
//!
//! A pack is a budgeted digest of L0/L1 facts for one domain. Budgeting matters
//! more than completeness: a 300k-token dump of a large domain would be both
//! expensive and worse, because the signal (enum comments, table structure,
//! traces) gets buried in boilerplate. So each section has its own allowance and
//! the highest-signal evidence is selected first.

use crate::store::Store;
use crate::util::{estimate_tokens, has_cjk, truncate_chars};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use std::fmt::Write as _;

/// Per-section character allowances. Chinese comments cost roughly 2.5 bytes per
/// token, so these are deliberately conservative.
struct Budget {
    enums: usize,
    tables: usize,
    entrypoints: usize,
    traces: usize,
    commits: usize,
    source: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            enums: 12_000,
            tables: 14_000,
            entrypoints: 8_000,
            traces: 8_000,
            commits: 4_000,
            source: 20_000,
        }
    }
}

pub struct Pack {
    pub domain_key: String,
    pub domain_id: i64,
    pub body: String,
    /// Digest of the evidence, so a note can be invalidated when it changes.
    pub source_digest: String,
    pub estimated_tokens: usize,
}

/// The evidence assembled for one entrypoint's capability narrative.
pub struct CapabilityPack {
    /// `"{kind}:{addr}"` — stable across rebuilds, unlike the entrypoint row id.
    pub subject_key: String,
    pub body: String,
    pub source_digest: String,
    pub estimated_tokens: usize,
}

/// Build the evidence pack for one entrypoint.
///
/// Unlike [`build`], this targets a single entrypoint and centres on what a
/// *caller* needs: the signature, the call path, and the tables it ends up
/// writing. The digest covers all of it, so an edited method or a new hop
/// invalidates the note exactly like a domain dossier.
pub fn build_capability(store: &Store, project_id: i64, entrypoint_id: i64) -> Result<CapabilityPack> {
    let (kind, addr, name, doc, config_json, symbol_id): (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
    ) = store.conn.query_row(
        "SELECT kind, COALESCE(addr, ''), name, doc, config_json, symbol_id
         FROM entrypoints WHERE project_id = ?1 AND id = ?2",
        params![project_id, entrypoint_id],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;

    let subject_key = format!("{kind}:{addr}");
    let mut s = String::new();
    writeln!(s, "# 入口：{addr}")?;
    writeln!(s, "类型：`{kind}`\n")?;
    if let Some(d) = doc.as_deref().filter(|d| !d.trim().is_empty()) {
        writeln!(s, "## 说明\n\n{d}\n")?;
    }
    if let Some(cfg) = config_json.as_deref() {
        if !cfg.trim().is_empty() && cfg != "null" {
            writeln!(s, "## 配置\n\n```json\n{cfg}\n```\n")?;
        }
    }
    if let Some(sid) = symbol_id {
        let (sig, sdoc): (Option<String>, Option<String>) = store
            .conn
            .query_row(
                "SELECT signature, doc FROM symbols WHERE id = ?1",
                params![sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None));
        writeln!(s, "## 方法签名\n\n`{}`", name)?;
        if let Some(sig) = sig.as_deref().filter(|x| !x.trim().is_empty()) {
            let _ = write!(s, "\n\n```java\n{sig}\n```");
        }
        if let Some(d) = sdoc.as_deref().filter(|d| !d.trim().is_empty()) {
            let _ = writeln!(s, "\n\n{d}");
        }
        let _ = writeln!(s);
    }
    // Call paths and the tables they reach, the substance a caller cares about.
    let mut seen_tables: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    let mut trace_body = String::new();
    let mut stmt = store.conn.prepare(
        "SELECT path_json, tables_json FROM traces WHERE entrypoint_id = ?1 ORDER BY depth LIMIT 24",
    )?;
    for row in stmt.query_map(params![entrypoint_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (path_json, tables_json) = row?;
        let Ok(path) = serde_json::from_str::<crate::structure::trace::TracePath>(&path_json)
        else {
            continue;
        };
        let ids: std::collections::BTreeMap<String, Vec<String>> =
            serde_json::from_str(&tables_json).unwrap_or_default();
        for id in ids.keys() {
            if let Ok(i) = id.parse::<i64>() {
                seen_tables.insert(i);
            }
        }
        let _ = writeln!(trace_body, "- {}", path.labels.join(" → "));
    }
    if !trace_body.is_empty() {
        let _ = writeln!(s, "\n## 调用链路\n\n{trace_body}");
    }
    if !seen_tables.is_empty() {
        let mut tbody = String::new();
        for tid in &seen_tables {
            let name: String = store
                .conn
                .query_row("SELECT name FROM tables WHERE id = ?1", params![tid], |r| r.get(0))?;
            let _ = writeln!(tbody, "\n### `{name}`");
            let cols: Vec<(String, Option<String>, Option<String>, i64)> = store
                .conn
                .prepare(
                    "SELECT name, java_type, doc, is_pk FROM columns
                     WHERE table_id = ?1 ORDER BY is_pk DESC, name",
                )?
                .query_map(params![tid], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            if cols.is_empty() {
                let _ = writeln!(tbody, "（字段未知）");
                continue;
            }
            for (col, ty, doc, is_pk) in cols {
                let pk = if is_pk == 1 { "（主键）" } else { "" };
                match doc {
                    Some(d) => {
                        let _ = writeln!(tbody, "- `{col}`{pk} {} — {}", ty.as_deref().unwrap_or(""), one_line(&d));
                    }
                    None => {
                        let _ = writeln!(tbody, "- `{col}`{pk} {}", ty.as_deref().unwrap_or(""));
                    }
                }
            }
        }
        let _ = writeln!(s, "\n## 触达表\n{tbody}");
    }

    let body = s;
    Ok(CapabilityPack {
        subject_key,
        source_digest: crate::util::sha256_hex(body.as_bytes()),
        estimated_tokens: estimate_tokens(&body),
        body,
    })
}

pub fn build(store: &Store, project_id: i64, domain_id: i64) -> Result<Pack> {
    let budget = Budget::default();
    let (key, label, confidence) = store.conn.query_row(
        "SELECT key, label, confidence FROM domains WHERE id = ?1",
        params![domain_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, f64>(2)?,
            ))
        },
    )?;

    let mut s = String::new();
    writeln!(s, "# 领域：{}", label.as_deref().unwrap_or(&key))?;
    writeln!(s, "领域键：`{key}`（自动划分，置信度 {confidence:.2}）\n")?;

    let sections = [
        ("## 状态与常量定义\n\n这些注释是业务状态语义的原始出处，优先据此还原生命周期。\n\n\
          标注「跨领域共享」的类型被全项目使用，其中只有与本领域直接相关的取值才属于本领域，\
          不要把其它取值当作本领域的概念收录。",
         enums(store, domain_id, budget.enums)?),
        ("## 数据表结构\n\n项目无 DDL，以下结构由 mapper XML 与实体注解反推。",
         tables(store, domain_id, budget.tables)?),
        ("## 对外入口", entrypoints(store, domain_id, budget.entrypoints)?),
        ("## 入口到数据表的调用链路\n\n已包含接口到实现的动态分发跳转。",
         traces(store, project_id, domain_id, budget.traces)?),
        ("## 相关 git 提交\n\n提交信息用团队自己的语言描述变更意图，是术语和业务规则的重要线索。",
         commits(store, domain_id, budget.commits)?),
        ("## 关键源码节选", source(store, domain_id, budget.source)?),
    ];
    for (heading, body) in sections {
        if body.trim().is_empty() {
            continue;
        }
        writeln!(s, "{heading}\n\n{body}")?;
    }

    Ok(Pack {
        domain_key: key,
        domain_id,
        source_digest: crate::util::sha256_hex(s.as_bytes()),
        estimated_tokens: estimate_tokens(&s),
        body: s,
    })
}

/// Enum members and documented constants — the densest business signal available.
///
/// Project-wide types are flagged rather than dropped. A file like
/// `PromotionTypeEnum` gets voted into whichever domain happens to win, and its
/// twenty unrelated activity types would then be attributed to that one domain.
/// The enum is still needed as context — it says where this domain sits among the
/// others — so it is labelled instead of removed.
fn enums(store: &Store, domain_id: i64, budget: usize) -> Result<String> {
    let rows: Vec<(String, String, Option<String>, Option<String>, i64)> = store
        .conn
        .prepare(
            "SELECT parent.name, s.name, s.signature, s.doc,
                    -- How many other domains reference this type by name.
                    (SELECT COUNT(DISTINCT m2.domain_id)
                     FROM symbols s2
                     JOIN domain_members m2 ON m2.kind = 'file' AND m2.ref_id = s2.file_id
                     WHERE s2.name = parent.name AND s2.kind IN ('enum', 'class', 'interface'))
             FROM symbols s
             JOIN symbols parent ON parent.id = s.parent_id
             JOIN domain_members m ON m.kind = 'file' AND m.ref_id = s.file_id
             WHERE m.domain_id = ?1 AND s.kind IN ('enum_member', 'constant')
             ORDER BY parent.name, s.start_line",
        )?
        .query_map(params![domain_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    // Documented entries first, Chinese-documented ones above the rest: an
    // undocumented constant tells the model nothing it cannot see elsewhere.
    let mut sorted = rows;
    sorted.sort_by_key(|(owner, _, _, doc, _)| {
        let rank = match doc.as_deref() {
            Some(d) if has_cjk(d) => 0,
            Some(_) => 1,
            None => 2,
        };
        (rank, owner.clone())
    });

    let mut s = String::new();
    let mut current = String::new();
    for (owner, name, sig, doc, domain_span) in sorted {
        if s.len() >= budget {
            writeln!(s, "…（其余常量已省略）")?;
            break;
        }
        if owner != current {
            if domain_span > 1 {
                writeln!(s, "\n### {owner}（跨领域共享，被 {domain_span} 个领域引用）")?;
            } else {
                writeln!(s, "\n### {owner}")?;
            }
            current = owner;
        }
        let value = sig
            .as_deref()
            .and_then(|x| x.split_once('='))
            .map(|(_, v)| v.trim().trim_end_matches(';').to_string());
        match (value, doc) {
            (Some(v), Some(d)) => writeln!(s, "- `{name}` = {v} — {}", one_line(&d))?,
            (Some(v), None) => writeln!(s, "- `{name}` = {v}")?,
            (None, Some(d)) => writeln!(s, "- `{name}` — {}", one_line(&d))?,
            (None, None) => writeln!(s, "- `{name}`")?,
        }
    }
    Ok(s)
}

fn one_line(doc: &str) -> String {
    doc.lines()
        .filter(|l| !l.trim_start().starts_with('@'))
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn tables(store: &Store, domain_id: i64, budget: usize) -> Result<String> {
    let tables: Vec<(i64, String)> = store
        .conn
        .prepare(
            "SELECT t.id, t.name FROM tables t
             JOIN domain_members m ON m.kind = 'table' AND m.ref_id = t.id
             WHERE m.domain_id = ?1 ORDER BY t.name",
        )?
        .query_map(params![domain_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut s = String::new();
    for (id, name) in tables {
        if s.len() >= budget {
            writeln!(s, "…（其余表已省略）")?;
            break;
        }
        writeln!(s, "\n### `{name}`")?;
        let cols: Vec<(String, Option<String>, Option<String>, i64)> = store
            .conn
            .prepare(
                "SELECT name, java_type, doc, is_pk FROM columns
                 WHERE table_id = ?1 ORDER BY is_pk DESC, name",
            )?
            .query_map(params![id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        if cols.is_empty() {
            writeln!(s, "（字段未知：该表仅出现在 SQL 语句中，没有对应的 resultMap）")?;
            continue;
        }
        for (col, ty, doc, is_pk) in cols {
            let pk = if is_pk == 1 { "（主键）" } else { "" };
            match doc {
                Some(d) => writeln!(
                    s,
                    "- `{col}`{pk} {} — {}",
                    ty.as_deref().unwrap_or(""),
                    one_line(&d)
                )?,
                None => writeln!(s, "- `{col}`{pk} {}", ty.as_deref().unwrap_or(""))?,
            }
        }
    }
    Ok(s)
}

fn entrypoints(store: &Store, domain_id: i64, budget: usize) -> Result<String> {
    let rows: Vec<(String, String, Option<String>)> = store
        .conn
        .prepare(
            "SELECT e.kind, e.addr, e.doc FROM entrypoints e
             JOIN domain_members m ON m.kind = 'entrypoint' AND m.ref_id = e.id
             WHERE m.domain_id = ?1
             ORDER BY (e.doc IS NULL), e.kind, e.addr",
        )?
        .query_map(params![domain_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut s = String::new();
    for (kind, addr, doc) in rows {
        if s.len() >= budget {
            writeln!(s, "…（其余入口已省略）")?;
            break;
        }
        match doc {
            Some(d) => writeln!(s, "- [{kind}] `{addr}` — {}", one_line(&d))?,
            None => writeln!(s, "- [{kind}] `{addr}`")?,
        }
    }
    Ok(s)
}

fn traces(store: &Store, project_id: i64, domain_id: i64, budget: usize) -> Result<String> {
    let rows: Vec<(String, String, String)> = store
        .conn
        .prepare(
            "SELECT e.addr, t.path_json, t.tables_json FROM traces t
             JOIN entrypoints e ON e.id = t.entrypoint_id
             JOIN domain_members m ON m.kind = 'entrypoint' AND m.ref_id = e.id
             WHERE t.project_id = ?1 AND m.domain_id = ?2
             ORDER BY t.depth, e.addr",
        )?
        .query_map(params![project_id, domain_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut s = String::new();
    for (addr, path_json, tables_json) in rows {
        if s.len() >= budget {
            writeln!(s, "…（其余链路已省略）")?;
            break;
        }
        let Ok(path) = serde_json::from_str::<crate::structure::trace::TracePath>(&path_json)
        else {
            continue;
        };
        let ids: std::collections::BTreeMap<String, Vec<String>> =
            serde_json::from_str(&tables_json).unwrap_or_default();
        let names: Vec<String> = ids
            .iter()
            .filter_map(|(id, ops)| {
                let id: i64 = id.parse().ok()?;
                let name: String = store
                    .conn
                    .query_row("SELECT name FROM tables WHERE id = ?1", params![id], |r| {
                        r.get(0)
                    })
                    .ok()?;
                Some(format!("{name}({})", ops.join("/")))
            })
            .collect();
        writeln!(s, "- `{addr}`")?;
        writeln!(s, "  {}", path.labels.join(" → "))?;
        if !names.is_empty() {
            writeln!(s, "  触达表：{}", names.join("、"))?;
        }
    }
    Ok(s)
}

/// Commit subjects touching this domain's files, most frequent words first.
fn commits(store: &Store, domain_id: i64, budget: usize) -> Result<String> {
    let rows: Vec<(String, String)> = store
        .conn
        .prepare(
            "SELECT DISTINCT c.subject, SUBSTR(c.authored_at, 1, 7) FROM git_commits c
             JOIN git_touches t ON t.commit_id = c.id
             JOIN domain_members m ON m.kind = 'file' AND m.ref_id = t.file_id
             WHERE m.domain_id = ?1 AND c.subject IS NOT NULL
             ORDER BY c.authored_at DESC
             LIMIT 200",
        )?
        .query_map(params![domain_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut s = String::new();
    for (subject, month) in rows {
        if s.len() >= budget {
            break;
        }
        writeln!(s, "- {month} {}", truncate_chars(subject.trim(), 80))?;
    }
    Ok(s)
}

/// Source of the domain's most-churned files, which is where the business logic
/// that keeps changing lives.
fn source(store: &Store, domain_id: i64, budget: usize) -> Result<String> {
    let rows: Vec<(String, i64)> = store
        .conn
        .prepare(
            "SELECT f.path, f.commit_count FROM files f
             JOIN domain_members m ON m.kind = 'file' AND m.ref_id = f.id
             WHERE m.domain_id = ?1 AND f.lang = 'java'
             ORDER BY f.commit_count DESC LIMIT 6",
        )?
        .query_map(params![domain_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let root: String = store.conn.query_row(
        "SELECT root FROM projects LIMIT 1",
        [],
        |r| r.get(0),
    )?;

    let mut s = String::new();
    let per_file = budget / rows.len().max(1);
    for (path, churn) in rows {
        if s.len() >= budget {
            break;
        }
        let full = std::path::Path::new(&root).join(&path);
        let Ok(text) = std::fs::read_to_string(&full) else { continue };
        writeln!(s, "\n### `{path}`（改动 {churn} 次）\n")?;
        writeln!(s, "```java")?;
        writeln!(s, "{}", truncate_chars(&text, per_file))?;
        writeln!(s, "```")?;
    }
    Ok(s)
}
