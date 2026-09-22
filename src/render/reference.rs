//! Reference sheets: tables, entrypoints, jobs, MQ consumers.
//!
//! These are pure L0 projections, so they are available before any model has
//! been called — which is the point of the two-stage design. For a project with
//! no documentation, a correct table and entrypoint inventory is already the
//! single most useful artefact.

use super::{cell, summarize, write};
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use std::fmt::Write as _;
use std::path::Path;

pub fn write_all(store: &Store, project_id: i64, out_dir: &Path) -> Result<usize> {
    let dir = out_dir.join("reference");
    let mut n = 0;
    n += write(&dir.join("tables.md"), &tables(store, project_id)?)?;
    n += write(&dir.join("entrypoints.md"), &entrypoints(store, project_id)?)?;
    n += write(&dir.join("jobs.md"), &jobs(store, project_id)?)?;
    n += write(&dir.join("mq.md"), &mq(store, project_id)?)?;
    Ok(n)
}

fn tables(store: &Store, project_id: i64) -> Result<String> {
    let mut s = String::new();
    writeln!(s, "# 数据表清单\n")?;
    writeln!(
        s,
        "表结构来自 mapper XML 的 `resultMap` 与 `@TableName` 注解反推——\
         项目中没有 DDL 文件，这是唯一可得的结构描述。字段含义取自对应 PO 的字段注释。\n"
    )?;

    let rows: Vec<(i64, String, String, Option<String>)> = store
        .conn
        .prepare(
            "SELECT t.id, t.name, t.source, d.key
             FROM tables t
             LEFT JOIN domain_members m ON m.kind = 'table' AND m.ref_id = t.id
             LEFT JOIN domains d ON d.id = m.domain_id
             WHERE t.project_id = ?1
             ORDER BY t.name",
        )?
        .query_map(params![project_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    writeln!(s, "共 {} 张表。\n", rows.len())?;
    writeln!(s, "| 表名 | 所属领域 | 字段数 | 读 | 写 | 来源 |")?;
    writeln!(s, "|---|---|---|---|---|---|")?;
    for (id, name, source, domain) in &rows {
        let cols: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM columns WHERE table_id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        let reads: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM table_access WHERE table_id = ?1 AND op = 'select'",
            params![id],
            |r| r.get(0),
        )?;
        let writes: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM table_access WHERE table_id = ?1 AND op <> 'select'",
            params![id],
            |r| r.get(0),
        )?;
        writeln!(
            s,
            "| [`{name}`](#{anchor}) | {} | {cols} | {reads} | {writes} | {} |",
            domain.as_deref().unwrap_or("—"),
            source_label(source),
            anchor = anchor(name),
        )?;
    }

    writeln!(s, "\n## 字段明细\n")?;
    for (id, name, _, _) in &rows {
        let cols: Vec<(String, Option<String>, Option<String>, Option<String>, i64)> = store
            .conn
            .prepare(
                "SELECT name, prop_name, java_type, doc, is_pk FROM columns
                 WHERE table_id = ?1 ORDER BY is_pk DESC, name",
            )?
            .query_map(params![id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        if cols.is_empty() {
            continue;
        }
        writeln!(s, "### `{name}`\n")?;
        writeln!(s, "| 字段 | Java 属性 | 类型 | 说明 |")?;
        writeln!(s, "|---|---|---|---|")?;
        for (col, prop, ty, doc, is_pk) in cols {
            let pk = if is_pk == 1 { " 🔑" } else { "" };
            writeln!(
                s,
                "| `{col}`{pk} | {} | {} | {} |",
                prop.as_deref().unwrap_or("—"),
                cell(ty.as_deref().unwrap_or("—")),
                summarize(doc.as_deref(), 60),
            )?;
        }
        writeln!(s)?;
    }
    Ok(s)
}

fn source_label(source: &str) -> &'static str {
    match source {
        "mybatis_result_map" => "resultMap",
        "mybatis_statement" => "SQL 语句",
        "mybatis_plus_annotation" => "@TableName",
        _ => "—",
    }
}

/// GitHub-style heading anchor for a table name.
fn anchor(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "")
}

fn entrypoints(store: &Store, project_id: i64) -> Result<String> {
    let mut s = String::new();
    writeln!(s, "# 入口清单\n")?;
    writeln!(
        s,
        "系统可被外部触达的全部入口。Dubbo 接口取自 `<dubbo:service>` 声明，\
         HTTP 路由取自 Spring MVC 注解，两者都包含那些看起来无人调用的入口——\
         老项目里「看起来没用」和「真的没用」是两件事。\n"
    )?;

    for (kind, title) in [("dubbo", "Dubbo 接口"), ("http", "HTTP 路由")] {
        let rows: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>)> =
            store
                .conn
                .prepare(
                    "SELECT e.addr, e.name, e.doc, d.key, e.config_json
                     FROM entrypoints e
                     LEFT JOIN domain_members m ON m.kind = 'entrypoint' AND m.ref_id = e.id
                     LEFT JOIN domains d ON d.id = m.domain_id
                     WHERE e.project_id = ?1 AND e.kind = ?2
                     ORDER BY e.addr",
                )?
                .query_map(params![project_id, kind], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<rusqlite::Result<_>>()?;

        writeln!(s, "## {title}（{}）\n", rows.len())?;
        if kind == "dubbo" {
            writeln!(s, "| 接口 | 领域 | 超时 | 说明 |")?;
            writeln!(s, "|---|---|---|---|")?;
            for (addr, _, doc, domain, cfg) in rows {
                let timeout = json_field(cfg.as_deref(), "timeout");
                writeln!(
                    s,
                    "| `{}` | {} | {} | {} |",
                    cell(&addr),
                    domain.as_deref().unwrap_or("—"),
                    timeout.unwrap_or_else(|| "—".into()),
                    summarize(doc.as_deref(), 70),
                )?;
            }
        } else {
            writeln!(s, "| 路由 | 领域 | 权限 | 处理方法 | 说明 |")?;
            writeln!(s, "|---|---|---|---|---|")?;
            for (addr, name, doc, domain, cfg) in rows {
                let guards = json_array(cfg.as_deref(), "guards");
                writeln!(
                    s,
                    "| `{}` | {} | {} | `{}` | {} |",
                    cell(&addr),
                    domain.as_deref().unwrap_or("—"),
                    if guards.is_empty() { "—".to_string() } else { cell(&guards.join(", ")) },
                    cell(short_fqn(name.as_deref().unwrap_or(""))),
                    summarize(doc.as_deref(), 50),
                )?;
            }
        }
        writeln!(s)?;
    }
    Ok(s)
}

fn jobs(store: &Store, project_id: i64) -> Result<String> {
    let mut s = String::new();
    writeln!(s, "# 定时任务\n")?;
    writeln!(
        s,
        "调度由 XXL-Job 触发。注意：部分老任务没有 `@XxlJob` 注解，\
         而是以 Dubbo 接口形式暴露给调度器，因此这里同时列出两类。\n"
    )?;
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>)> = store
        .conn
        .prepare(
            "SELECT e.addr, e.name, e.doc, d.key FROM entrypoints e
             LEFT JOIN domain_members m ON m.kind = 'entrypoint' AND m.ref_id = e.id
             LEFT JOIN domains d ON d.id = m.domain_id
             WHERE e.project_id = ?1 AND e.kind = 'job' ORDER BY e.addr",
        )?
        .query_map(params![project_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    writeln!(s, "共 {} 个。\n", rows.len())?;
    writeln!(s, "| 任务 | 领域 | 实现 | 说明 |")?;
    writeln!(s, "|---|---|---|---|")?;
    for (addr, name, doc, domain) in rows {
        writeln!(
            s,
            "| `{}` | {} | `{}` | {} |",
            cell(&addr),
            domain.as_deref().unwrap_or("—"),
            cell(short_fqn(name.as_deref().unwrap_or(""))),
            summarize(doc.as_deref(), 70),
        )?;
    }
    Ok(s)
}

fn mq(store: &Store, project_id: i64) -> Result<String> {
    let mut s = String::new();
    writeln!(s, "# 消息消费\n")?;
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>)> = store
        .conn
        .prepare(
            "SELECT e.addr, e.name, e.doc, d.key, e.config_json FROM entrypoints e
             LEFT JOIN domain_members m ON m.kind = 'entrypoint' AND m.ref_id = e.id
             LEFT JOIN domains d ON d.id = m.domain_id
             WHERE e.project_id = ?1 AND e.kind = 'mq' ORDER BY e.name",
        )?
        .query_map(params![project_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let unresolved = rows
        .iter()
        .filter(|(addr, ..)| addr.contains("${"))
        .count();
    writeln!(s, "共 {} 个消费者。\n", rows.len())?;
    if unresolved > 0 {
        writeln!(
            s,
            "> 其中 {unresolved} 个 topic 仍显示为 `${{占位符}}`：这些值定义在外部配置中心，\
             仓库里没有对应的 properties 文件，需要到配置中心查实际 topic。\n"
        )?;
    }
    writeln!(s, "| Topic | 领域 | 监听器 | 说明 |")?;
    writeln!(s, "|---|---|---|---|")?;
    for (addr, name, doc, domain, _) in rows {
        writeln!(
            s,
            "| `{}` | {} | `{}` | {} |",
            cell(&addr),
            domain.as_deref().unwrap_or("—"),
            cell(short_fqn(name.as_deref().unwrap_or(""))),
            summarize(doc.as_deref(), 60),
        )?;
    }
    Ok(s)
}

/// Last two dotted segments, enough to identify a type without the namespace.
fn short_fqn(fqn: &str) -> &str {
    match fqn.rsplit_once('.') {
        Some((_, tail)) => tail,
        None => fqn,
    }
}

fn json_field(json: Option<&str>, key: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json?).ok()?;
    match v.get(key)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn json_array(json: Option<&str>, key: &str) -> Vec<String> {
    let Some(json) = json else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return Vec::new() };
    v.get(key)
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}
