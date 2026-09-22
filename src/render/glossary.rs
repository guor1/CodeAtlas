//! The glossary page: business vocabulary mapped to the code that implements it.
//!
//! For a project whose domain language is Chinese and whose identifiers are
//! English, this mapping is the single most reusable artefact: it is what lets a
//! newcomer — or an agent — go from a term in a ticket to the code that handles it.

use super::{cell, write};
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use std::fmt::Write as _;
use std::path::Path;

pub fn write_all(store: &Store, project_id: i64, out_dir: &Path) -> Result<usize> {
    write(&out_dir.join("glossary.md"), &page(store, project_id)?)
}

fn page(store: &Store, project_id: i64) -> Result<String> {
    let rows: Vec<(i64, String, Option<String>, Option<String>, Option<String>, String)> = store
        .conn
        .prepare(
            "SELECT g.id, g.term, g.definition_md, g.aliases_json, d.key, g.status
             FROM glossary g
             LEFT JOIN domains d ON d.id = g.domain_id
             WHERE g.project_id = ?1
             ORDER BY d.key, g.term",
        )?
        .query_map(params![project_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut s = String::new();
    writeln!(s, "# 业务术语表\n")?;

    if rows.is_empty() {
        writeln!(
            s,
            "尚未生成。运行 `catlas deepen` 后，这里会列出项目里的业务术语\
             及其对应的代码标识符、枚举值和表名。\n"
        )?;
        return Ok(s);
    }

    writeln!(
        s,
        "术语由大模型从代码注释、枚举定义、表结构和 git 提交信息中提取，\
         并给出对应的代码位置。标记为「待确认」的条目依据较弱，使用前请核对代码。\n"
    )?;

    let mut current: Option<String> = None;
    for (id, term, definition, aliases, domain, status) in &rows {
        let key = domain.clone();
        if key != current {
            writeln!(s, "\n## {}\n", key.as_deref().unwrap_or("通用"))?;
            writeln!(s, "| 术语 | 含义 | 别名 | 对应代码 |")?;
            writeln!(s, "|---|---|---|---|")?;
            current = key;
        }
        let alias_list: Vec<String> = aliases
            .as_deref()
            .and_then(|j| serde_json::from_str::<Vec<String>>(j).ok())
            .unwrap_or_default();
        // Look up links by this row's id. Matching on the term instead would
        // merge the links of same-named terms from other domains, and DISTINCT
        // guards against a term the model listed twice in one response.
        let mut refs: Vec<String> = store
            .conn
            .prepare(
                "SELECT DISTINCT ref_text FROM glossary_links
                 WHERE glossary_id = ?1 AND ref_text IS NOT NULL
                 ORDER BY ref_text",
            )?
            .query_map(params![id], |r| r.get::<_, Option<String>>(0))?
            .filter_map(|r| r.ok().flatten())
            .collect();
        refs.dedup();

        let flag = if status == "needs_review" { "（待确认）" } else { "" };
        writeln!(
            s,
            "| **{}**{flag} | {} | {} | {} |",
            cell(term),
            cell(definition.as_deref().unwrap_or("—")),
            if alias_list.is_empty() { "—".into() } else { cell(&alias_list.join("、")) },
            if refs.is_empty() {
                "—".to_string()
            } else {
                refs.iter().map(|r| format!("`{}`", cell(r))).collect::<Vec<_>>().join(" ")
            },
        )?;
    }
    writeln!(s, "\n共 {} 条术语。", rows.len())?;
    Ok(s)
}
