//! XML and properties pass: schema from mappers, entrypoints from Dubbo/Spring.

use crate::build::BuildStats;
use crate::build::java_pass::JavaIndex;
use crate::extract::{dubbo_xml, mybatis, properties, spring, walk};
use crate::store::Store;
use crate::store::model::EntrypointKind;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use std::collections::BTreeMap;
use std::path::Path;

pub fn run(
    store: &Store,
    project_id: i64,
    root: &Path,
    files: &[walk::Found],
    file_ids: &BTreeMap<String, i64>,
    java: &JavaIndex,
    stats: &mut BuildStats,
) -> Result<()> {
    let props = index_properties(store, project_id, files, file_ids, stats)?;
    index_mappers(store, project_id, files, file_ids, java, stats)?;
    index_table_annotations(store, project_id, java, stats)?;
    index_dubbo(store, project_id, files, file_ids, java, stats)?;
    index_mq(store, project_id, files, file_ids, java, &props, stats)?;
    let _ = root;
    Ok(())
}

fn index_properties(
    store: &Store,
    project_id: i64,
    files: &[walk::Found],
    file_ids: &BTreeMap<String, i64>,
    stats: &mut BuildStats,
) -> Result<BTreeMap<String, String>> {
    let mut merged = BTreeMap::new();
    let tx = store.conn.unchecked_transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO config_props(project_id, file_id, key, value) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for f in files.iter().filter(|f| f.lang == walk::Lang::Properties) {
            let Ok(text) = std::fs::read_to_string(&f.path) else { continue };
            let parsed = properties::parse(&text);
            let file_id = file_ids.get(&f.rel);
            for (k, v) in &parsed {
                ins.execute(params![project_id, file_id, k, v])?;
                stats.config_props += 1;
                // Environment-specific files repeat keys; first writer wins, and
                // the per-file rows above keep every variant visible.
                merged.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    tx.commit()?;
    Ok(merged)
}

/// Insert a table if absent and return `(id, inserted)`.
///
/// The same table is referenced by many mappers, so callers must count only the
/// first insertion to report a true table total.
fn upsert_table(
    tx: &rusqlite::Transaction<'_>,
    project_id: i64,
    name: &str,
    source: &str,
    evidence_file_id: Option<&i64>,
) -> Result<(i64, bool)> {
    if let Some(id) = tx
        .query_row(
            "SELECT id FROM tables WHERE project_id = ?1 AND name = ?2",
            params![project_id, name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        return Ok((id, false));
    }
    tx.execute(
        "INSERT INTO tables(project_id, name, source, evidence_file_id) VALUES (?1, ?2, ?3, ?4)",
        params![project_id, name, source, evidence_file_id],
    )?;
    Ok((tx.last_insert_rowid(), true))
}

/// Tables, columns and DAO-method-level access from mapper XML.
///
/// Note on attribution: a table reached by many mappers is recorded once with
/// whichever file the build saw first as its evidence. That is deliberate — the
/// `table_access` rows carry the full picture of who touches it.
fn index_mappers(
    store: &Store,
    project_id: i64,
    files: &[walk::Found],
    file_ids: &BTreeMap<String, i64>,
    java: &JavaIndex,
    stats: &mut BuildStats,
) -> Result<()> {
    let tx = store.conn.unchecked_transaction()?;
    let mut new_tables = 0usize;
    {
        for f in files.iter().filter(|f| f.lang == walk::Lang::Xml) {
            let Ok(text) = std::fs::read_to_string(&f.path) else { continue };
            // Only mapper files declare a namespace with statements in them.
            if !text.contains("<mapper") {
                continue;
            }
            let Ok(m) = mybatis::parse(&text) else { continue };
            if m.statements.is_empty() && m.result_maps.is_empty() {
                continue;
            }
            let file_id = file_ids.get(&f.rel);

            // Column dictionary, attributed to the table its statements read.
            for rm in &m.result_maps {
                let Some(table) = m.table_for_result_map(&rm.id) else { continue };
                let (table_id, inserted) =
                    upsert_table(&tx, project_id, &table, "mybatis_result_map", file_id)?;
                new_tables += inserted as usize;
                // The PO's field docs are the only column descriptions available.
                let po_docs = rm
                    .type_fqn
                    .as_deref()
                    .map(|fqn| field_docs(&tx, java, fqn))
                    .transpose()?
                    .unwrap_or_default();
                for map in &rm.mappings {
                    // `execute` returns rows affected by this statement; the
                    // connection-wide `changes()` counter would over-count.
                    let n = tx.execute(
                        "INSERT OR IGNORE INTO columns(table_id, name, prop_name, java_type, doc, is_pk)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            table_id,
                            map.column,
                            map.property,
                            po_docs.get(&map.property).map(|(t, _)| t.clone()),
                            po_docs.get(&map.property).and_then(|(_, d)| d.clone()),
                            map.is_pk as i64,
                        ],
                    )?;
                    stats.columns += n;
                }
            }

            // Statement-level access, attributed to the DAO method when we can
            // find it: that is what makes call-graph traces reach a table.
            for st in &m.statements {
                let dao_method_id = m
                    .namespace
                    .as_deref()
                    .and_then(|ns| java.methods.get(&format!("{ns}#{}", st.id)).copied());
                for (table, op) in &st.tables {
                    let (table_id, inserted) =
                        upsert_table(&tx, project_id, table, "mybatis_statement", file_id)?;
                    new_tables += inserted as usize;
                    tx.execute(
                        "INSERT INTO table_access(project_id, table_id, symbol_id, file_id, op)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![project_id, table_id, dao_method_id, file_id, op.as_str()],
                    )?;
                    stats.table_access += 1;
                }
            }
        }
    }
    tx.commit()?;
    stats.tables += new_tables;
    Ok(())
}

/// Declared type and Javadoc for each persistent field of a PO class.
///
/// Restricted to instance fields on purpose. `static final` members of a PO are
/// status constants (`STATUS_NORMAL = 0`), not columns, and admitting them
/// invents database columns that do not exist — exactly the kind of confident
/// falsehood this knowledge base must not contain. `serialVersionUID` is
/// excluded for the same reason.
fn field_docs(
    tx: &rusqlite::Transaction<'_>,
    java: &JavaIndex,
    type_fqn: &str,
) -> Result<BTreeMap<String, (String, Option<String>)>> {
    let mut out = BTreeMap::new();
    if !java.types.contains_key(type_fqn) {
        return Ok(out);
    }
    let mut stmt = tx.prepare(
        "SELECT name, signature, doc FROM symbols
         WHERE fqn LIKE ?1 ESCAPE '\\' AND kind = 'field'
           AND name <> 'serialVersionUID'",
    )?;
    // `_` is a LIKE wildcard; escaping keeps `My_Type` from matching `MyXType`.
    let like = format!("{}#%", type_fqn.replace('_', "\\_"));
    let rows = stmt.query_map(params![like], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (name, sig, doc) = row?;
        let ty = sig
            .as_deref()
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("")
            .to_string();
        out.insert(name, (ty, doc));
    }
    Ok(out)
}

/// Tables named by MyBatis-Plus `@TableName`, which mapper XML never mentions
/// because those DAOs generate their SQL at runtime.
fn index_table_annotations(
    store: &Store,
    project_id: i64,
    java: &JavaIndex,
    stats: &mut BuildStats,
) -> Result<()> {
    let tx = store.conn.unchecked_transaction()?;
    let mut new_tables = 0usize;
    {
        let mut q = tx.prepare(
            "SELECT s.id, s.fqn, a.args_json, s.file_id
             FROM symbol_annotations a
             JOIN symbols s ON s.id = a.symbol_id
             WHERE a.name = 'TableName' AND s.project_id = ?1",
        )?;
        let rows: Vec<(i64, Option<String>, Option<String>, i64)> = q
            .query_map(params![project_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;

        for (sym_id, fqn, args, file_id) in rows {
            let Some(args) = args else { continue };
            let map: BTreeMap<String, String> = serde_json::from_str(&args).unwrap_or_default();
            let Some(name) = map.get("value") else { continue };
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            let (table_id, inserted) =
                upsert_table(&tx, project_id, &name, "mybatis_plus_annotation", Some(&file_id))?;
            new_tables += inserted as usize;
            // The annotated entity's fields describe the columns; MyBatis-Plus
            // maps camelCase to snake_case unless told otherwise.
            if let Some(fqn) = fqn.as_deref() {
                for (field, (ty, doc)) in field_docs(&tx, java, fqn)? {
                    let column = crate::util::to_snake(&field);
                    let n = tx.execute(
                        "INSERT OR IGNORE INTO columns(table_id, name, prop_name, java_type, doc, is_pk)
                         VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                        params![table_id, column, field, ty, doc],
                    )?;
                    stats.columns += n;
                }
            }
            // The entity itself is the evidence of access; concrete operations
            // come from the service methods that use it.
            tx.execute(
                "INSERT INTO table_access(project_id, table_id, symbol_id, file_id, op)
                 VALUES (?1, ?2, ?3, ?4, 'select')",
                params![project_id, table_id, sym_id, file_id],
            )?;
            stats.table_access += 1;
        }
    }
    tx.commit()?;
    stats.tables += new_tables;
    Ok(())
}

/// Dubbo entrypoints from `<dubbo:service>` declarations.
fn index_dubbo(
    store: &Store,
    project_id: i64,
    files: &[walk::Found],
    file_ids: &BTreeMap<String, i64>,
    java: &JavaIndex,
    stats: &mut BuildStats,
) -> Result<()> {
    let tx = store.conn.unchecked_transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO entrypoints(project_id, kind, name, addr, symbol_id, file_id,
                                     config_json, doc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for f in files.iter().filter(|f| f.lang == walk::Lang::Xml) {
            let Ok(text) = std::fs::read_to_string(&f.path) else { continue };
            if !text.contains("dubbo:service") {
                continue;
            }
            let Ok(d) = dubbo_xml::parse(&text) else { continue };
            let file_id = file_ids.get(&f.rel);
            for s in &d.services {
                let symbol_id = java.types.get(&s.interface).copied();
                let cfg = serde_json::json!({
                    "ref": s.reference,
                    "timeout": s.timeout,
                    "retries": s.retries,
                    "version": s.version,
                    "group": s.group,
                });
                // Prefer the XML comment; fall back to the interface's Javadoc.
                let doc = s
                    .doc
                    .clone()
                    .or_else(|| java.type_docs.get(&s.interface).cloned());
                let short = s.interface.rsplit('.').next().unwrap_or(&s.interface);
                ins.execute(params![
                    project_id,
                    EntrypointKind::Dubbo.as_str(),
                    s.interface,
                    short,
                    symbol_id,
                    file_id,
                    cfg.to_string(),
                    doc,
                ])?;
                stats.entrypoints += 1;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// MQ consumers, pairing each listener class with its resolved topic.
fn index_mq(
    store: &Store,
    project_id: i64,
    files: &[walk::Found],
    file_ids: &BTreeMap<String, i64>,
    java: &JavaIndex,
    props: &BTreeMap<String, String>,
    stats: &mut BuildStats,
) -> Result<()> {
    let tx = store.conn.unchecked_transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO entrypoints(project_id, kind, name, addr, symbol_id, file_id,
                                     config_json, doc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for f in files.iter().filter(|f| f.lang == walk::Lang::Xml) {
            let Ok(text) = std::fs::read_to_string(&f.path) else { continue };
            if !text.contains("<bean") {
                continue;
            }
            let Ok(s) = spring::parse(&text) else { continue };
            let bindings = s.mq_bindings();
            if bindings.is_empty() {
                continue;
            }
            let file_id = file_ids.get(&f.rel);
            for (class, topic_expr) in bindings {
                let topic = properties::resolve(&topic_expr, props);
                let symbol_id = java.types.get(&class).copied();
                // A listener's own file is more useful than the wiring XML.
                let listener_file = java
                    .type_files
                    .get(&class)
                    .and_then(|rel| file_ids.get(rel))
                    .or(file_id);
                let cfg = serde_json::json!({
                    "topic_expr": topic_expr,
                    "resolved": topic != topic_expr,
                    "wiring": f.rel,
                });
                ins.execute(params![
                    project_id,
                    EntrypointKind::Mq.as_str(),
                    class,
                    topic,
                    symbol_id,
                    listener_file,
                    cfg.to_string(),
                    java.type_docs.get(&class),
                ])?;
                stats.entrypoints += 1;
            }
        }
    }
    tx.commit()?;
    Ok(())
}
