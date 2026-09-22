//! Per-domain pages and the domain map.
//!
//! Without L2 notes these pages still carry the domain's tables, entrypoints,
//! state enums and hot files — enough to orient someone in an unfamiliar area.
//! When notes exist they are placed above the evidence, not instead of it.

use super::{cell, summarize, write};
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use std::fmt::Write as _;
use std::path::Path;

struct DomainRow {
    id: i64,
    key: String,
    label: Option<String>,
    confidence: f64,
    files: i64,
    tables: i64,
    entrypoints: i64,
}

fn load(store: &Store, project_id: i64) -> Result<Vec<DomainRow>> {
    let rows = store
        .conn
        .prepare(
            "SELECT d.id, d.key, d.label, d.confidence,
                    (SELECT COUNT(*) FROM domain_members m WHERE m.domain_id = d.id AND m.kind = 'file'),
                    (SELECT COUNT(*) FROM domain_members m WHERE m.domain_id = d.id AND m.kind = 'table'),
                    (SELECT COUNT(*) FROM domain_members m WHERE m.domain_id = d.id AND m.kind = 'entrypoint')
             FROM domains d WHERE d.project_id = ?1
             ORDER BY 5 DESC, d.key",
        )?
        .query_map(params![project_id], |r| {
            Ok(DomainRow {
                id: r.get(0)?,
                key: r.get(1)?,
                label: r.get(2)?,
                confidence: r.get(3)?,
                files: r.get(4)?,
                tables: r.get(5)?,
                entrypoints: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The domain map, embedded in the README.
pub fn index_section(store: &Store, project_id: i64) -> Result<String> {
    let mut s = String::new();
    let domains = load(store, project_id)?;
    writeln!(s, "## 业务领域地图\n")?;
    writeln!(
        s,
        "领域由包结构、表名前缀、git 共变和调用关系四路信号加权投票得出，\
         `置信度` 是该领域文件投票的平均集中度——偏低说明这块代码的归属本身就是模糊的，\
         值得人工确认。想覆盖自动结果，编辑 `.codeatlas/domains.toml` 后重新 `catlas build`。\n"
    )?;
    writeln!(s, "| 领域 | 文件 | 表 | 入口 | 置信度 |")?;
    writeln!(s, "|---|---|---|---|---|")?;
    for d in &domains {
        let title = d.label.clone().unwrap_or_else(|| d.key.clone());
        writeln!(
            s,
            "| [{title}](domains/{}.md) | {} | {} | {} | {:.2} |",
            d.key, d.files, d.tables, d.entrypoints, d.confidence
        )?;
    }
    Ok(s)
}

pub fn write_all(store: &Store, project_id: i64, out_dir: &Path) -> Result<usize> {
    let dir = out_dir.join("domains");
    let mut n = 0;
    for d in load(store, project_id)? {
        n += write(&dir.join(format!("{}.md", d.key)), &page(store, project_id, &d)?)?;
    }
    Ok(n)
}

fn page(store: &Store, project_id: i64, d: &DomainRow) -> Result<String> {
    let mut s = String::new();
    let title = d.label.clone().unwrap_or_else(|| d.key.clone());
    writeln!(s, "# {title}\n")?;
    writeln!(
        s,
        "> 领域键 `{}` · {} 个文件 · {} 张表 · {} 个入口 · 划分置信度 {:.2}\n",
        d.key, d.files, d.tables, d.entrypoints, d.confidence
    )?;

    // L2 narrative first when present, since it explains what the evidence means.
    let notes: Vec<(String, String)> = store
        .conn
        .prepare(
            "SELECT COALESCE(title, kind), body_md FROM notes
             WHERE project_id = ?1 AND subject_kind = 'domain' AND subject_id = ?2
             ORDER BY id",
        )?
        .query_map(params![project_id, d.id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if notes.is_empty() {
        writeln!(
            s,
            "*尚未生成领域解读。运行 `catlas deepen --domain {}` 后本节会填入职责、\
             生命周期、业务规则与已知陷阱。以下是确定性事实。*\n",
            d.key
        )?;
    } else {
        for (title, body) in notes {
            writeln!(s, "## {title}\n\n{body}\n")?;
        }
        writeln!(s, "---\n")?;
    }

    s.push_str(&tables_section(store, d.id)?);
    s.push_str(&entrypoints_section(store, d.id)?);
    s.push_str(&enums_section(store, d.id)?);
    s.push_str(&traces_section(store, project_id, d.id)?);
    s.push_str(&hot_files_section(store, d.id)?);
    Ok(s)
}

fn tables_section(store: &Store, domain_id: i64) -> Result<String> {
    let mut s = String::new();
    let rows: Vec<(String, i64)> = store
        .conn
        .prepare(
            "SELECT t.name, (SELECT COUNT(*) FROM columns c WHERE c.table_id = t.id)
             FROM tables t
             JOIN domain_members m ON m.kind = 'table' AND m.ref_id = t.id
             WHERE m.domain_id = ?1 ORDER BY t.name",
        )?
        .query_map(params![domain_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        return Ok(s);
    }
    writeln!(s, "## 数据表\n")?;
    for (name, cols) in rows {
        writeln!(s, "- `{name}`（{cols} 字段）→ 详见 [reference/tables.md](../reference/tables.md)")?;
    }
    writeln!(s)?;
    Ok(s)
}

fn entrypoints_section(store: &Store, domain_id: i64) -> Result<String> {
    let mut s = String::new();
    let rows: Vec<(String, String, Option<String>)> = store
        .conn
        .prepare(
            "SELECT e.kind, e.addr, e.doc FROM entrypoints e
             JOIN domain_members m ON m.kind = 'entrypoint' AND m.ref_id = e.id
             WHERE m.domain_id = ?1 ORDER BY e.kind, e.addr",
        )?
        .query_map(params![domain_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        return Ok(s);
    }
    writeln!(s, "## 入口\n")?;
    writeln!(s, "| 类型 | 入口 | 说明 |")?;
    writeln!(s, "|---|---|---|")?;
    for (kind, addr, doc) in rows {
        writeln!(s, "| {} | `{}` | {} |", kind_label(&kind), cell(&addr), summarize(doc.as_deref(), 60))?;
    }
    writeln!(s)?;
    Ok(s)
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "dubbo" => "Dubbo",
        "http" => "HTTP",
        "job" => "定时",
        "mq" => "消息",
        _ => "其它",
    }
}

/// Enums and constant groups owned by the domain.
///
/// In these projects the state machine lives in an enum or a `static final`
/// block with a Chinese comment per value, and that comment is usually the only
/// written description of the business states.
fn enums_section(store: &Store, domain_id: i64) -> Result<String> {
    let mut s = String::new();
    let rows: Vec<(String, String, Option<String>, Option<String>)> = store
        .conn
        .prepare(
            "SELECT parent.name, s.name, s.signature, s.doc
             FROM symbols s
             JOIN symbols parent ON parent.id = s.parent_id
             JOIN domain_members m ON m.kind = 'file' AND m.ref_id = s.file_id
             WHERE m.domain_id = ?1 AND s.kind IN ('enum_member', 'constant')
               AND s.doc IS NOT NULL
             ORDER BY parent.name, s.start_line",
        )?
        .query_map(params![domain_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        return Ok(s);
    }

    writeln!(s, "## 状态与常量\n")?;
    writeln!(s, "取自代码注释，是业务状态语义的原始出处。\n")?;
    let mut current = String::new();
    let mut shown = 0usize;
    for (owner, name, sig, doc) in rows {
        // Long constant classes would swamp the page; the reference sheets and
        // `catlas query` cover the full set.
        if shown >= 80 {
            writeln!(s, "\n*（其余常量请用 `catlas query` 检索）*\n")?;
            break;
        }
        if owner != current {
            writeln!(s, "\n**{owner}**\n")?;
            current = owner;
        }
        let value = sig
            .as_deref()
            .and_then(|x| x.split_once('='))
            .map(|(_, v)| v.trim().to_string());
        match value {
            Some(v) => writeln!(s, "- `{name}` = `{v}` — {}", summarize(doc.as_deref(), 60))?,
            None => writeln!(s, "- `{name}` — {}", summarize(doc.as_deref(), 60))?,
        }
        shown += 1;
    }
    writeln!(s)?;
    Ok(s)
}

/// Entrypoint-to-table paths, the flows that make a domain legible.
fn traces_section(store: &Store, project_id: i64, domain_id: i64) -> Result<String> {
    let mut s = String::new();
    let rows: Vec<(String, String, String, String)> = store
        .conn
        .prepare(
            "SELECT e.kind, e.addr, t.path_json, t.tables_json
             FROM traces t
             JOIN entrypoints e ON e.id = t.entrypoint_id
             JOIN domain_members m ON m.kind = 'entrypoint' AND m.ref_id = e.id
             WHERE t.project_id = ?1 AND m.domain_id = ?2
             ORDER BY e.addr, t.depth
             LIMIT 30",
        )?
        .query_map(params![project_id, domain_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        return Ok(s);
    }

    writeln!(s, "## 关键链路\n")?;
    writeln!(s, "从入口到数据表的调用路径，含接口到实现的动态分发跳转。\n")?;
    for (kind, addr, path_json, tables_json) in rows {
        let Ok(path) = serde_json::from_str::<crate::structure::trace::TracePath>(&path_json)
        else {
            continue;
        };
        let tables: std::collections::BTreeMap<String, Vec<String>> =
            serde_json::from_str(&tables_json).unwrap_or_default();
        let table_names: Vec<String> = tables
            .keys()
            .filter_map(|id| {
                let id: i64 = id.parse().ok()?;
                store
                    .conn
                    .query_row(
                        "SELECT name FROM tables WHERE id = ?1",
                        params![id],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
            })
            .collect();

        writeln!(s, "**[{}] `{}`**\n", kind_label(&kind), cell(&addr))?;
        writeln!(s, "```")?;
        for (i, label) in path.labels.iter().enumerate() {
            writeln!(s, "{}{}", "  ".repeat(i), method_label(label))?;
        }
        writeln!(s, "```")?;
        if !table_names.is_empty() {
            writeln!(s, "→ 触达表：{}\n", table_names.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join("、"))?;
        }
    }
    Ok(s)
}

/// `a.b.C#m` rendered as `C#m`, which is what a reader needs in a path listing.
fn method_label(fqn: &str) -> String {
    match fqn.rsplit_once('.') {
        Some((_, tail)) => tail.to_string(),
        None => fqn.to_string(),
    }
}

/// Files changed most often in the mined history: where the risk lives.
fn hot_files_section(store: &Store, domain_id: i64) -> Result<String> {
    let mut s = String::new();
    let rows: Vec<(String, i64, Option<String>)> = store
        .conn
        .prepare(
            "SELECT f.path, f.commit_count, f.last_commit_at FROM files f
             JOIN domain_members m ON m.kind = 'file' AND m.ref_id = f.id
             WHERE m.domain_id = ?1 AND f.commit_count > 0
             ORDER BY f.commit_count DESC LIMIT 10",
        )?
        .query_map(params![domain_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        return Ok(s);
    }
    writeln!(s, "## 改动最频繁的文件\n")?;
    writeln!(s, "改动次数高说明业务在持续演进，也说明这些文件最容易踩坑。\n")?;
    writeln!(s, "| 文件 | 改动次数 | 最近改动 |")?;
    writeln!(s, "|---|---|---|")?;
    for (path, count, last) in rows {
        let last = last.as_deref().unwrap_or("—");
        let last = last.split('T').next().unwrap_or(last);
        writeln!(s, "| `{path}` | {count} | {last} |")?;
    }
    writeln!(s)?;
    Ok(s)
}
