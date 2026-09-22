//! Full-text search over the knowledge base.
//!
//! `query` searches the `search_fts` index, a derived view over L0/L1/L2 — the
//! same status as the Markdown projection: deletable, rebuildable, never the
//! source of truth. The index uses FTS5's `trigram` tokenizer so that English
//! identifiers (`coupon`) and unsegmented Chinese terms (`特价`) both match as
//! substrings, which the default word tokenizer cannot do for CJK text.

use crate::store::Store;
use crate::util::truncate_chars;
use anyhow::{Context, Result};
use rusqlite::params;

/// One search hit, flattened for display or `--json` output.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Hit {
    /// Coarse category for display: file / symbol / table / entrypoint / domain / note / glossary / commit.
    pub kind: String,
    /// Finer subtype: a symbol's kind, an entrypoint's transport, a file's lang.
    pub subject_kind: String,
    /// Row id in the source table (symbols / tables / entrypoints / …), kept so
    /// a future command can jump straight to the record.
    pub subject_id: i64,
    /// Primary identifier: fqn, table name, term, path, addr, …
    pub title: String,
    /// Truncated descriptive text (Javadoc, definition, column list, …).
    pub snippet: String,
    /// Source file, when the subject lives in one.
    pub file: Option<String>,
}

/// Wipe and refill the search index from the current contents of the database.
///
/// Called at the end of `build` and `deepen`, and lazily by `query` when the
/// index is empty (e.g. a knowledge base built before search existed). The
/// wipe-and-refill is one transaction, so a reader never sees a half-built index.
pub fn rebuild(store: &Store, project_id: i64) -> Result<usize> {
    let tx = store.conn.unchecked_transaction()?;
    tx.execute("DELETE FROM search_fts", [])?;
    let mut n = 0usize;

    // Shared sink: every subject is flattened to the same seven columns, which
    // is what keeps the query side (and a future MCP surface) simple.
    let put = |ins: &mut rusqlite::Statement,
                   title: &str,
                   body: &str,
                   kind: &str,
                   subject_kind: &str,
                   subject_id: i64,
                   file: Option<&str>|
     -> Result<()> {
        ins.execute(params![
            title, body, kind, subject_kind, subject_id, project_id, file
        ])?;
        Ok(())
    };

    {
        let mut ins = tx.prepare(
            "INSERT INTO search_fts(title, body, kind, subject_kind, subject_id, project_id, file_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;

        // Files, indexed by path so a file-name search finds the file itself.
        {
            let mut stmt = tx.prepare("SELECT id, path, lang FROM files WHERE project_id = ?1")?;
            let rows: Vec<(i64, String, Option<String>)> = stmt
                .query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, path, lang) in rows {
                put(&mut ins, "", "", "file", lang.as_deref().unwrap_or(""), id, Some(&path))?;
                n += 1;
            }
        }

        // Symbols: identifier plus kind, Javadoc and signature.
        {
            let mut stmt = tx.prepare(
                "SELECT s.id, COALESCE(s.fqn, s.name), s.kind, s.doc, s.signature, f.path
                 FROM symbols s LEFT JOIN files f ON f.id = s.file_id
                 WHERE s.project_id = ?1",
            )?;
            let rows: Vec<(i64, String, String, Option<String>, Option<String>, Option<String>)> = stmt
                .query_map(params![project_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            for (id, title, kind, doc, sig, file) in rows {
                // Enum members store their constructor arguments as the
                // "signature" (e.g. `(0)`); that is value noise, not a signature,
                // so drop it from the searchable text. Methods and fields keep
                // theirs (`Coupon findById(Long id)`), which is real signal.
                let sig = sig.filter(|s| !s.starts_with('('));
                let body = doc.into_iter().chain(sig).collect::<Vec<_>>().join("\n");
                put(&mut ins, &title, &body, "symbol", &kind, id, file.as_deref())?;
                n += 1;
            }
        }

        // Tables, with their columns folded into the body so a column-name
        // search reaches its table.
        {
            let mut stmt = tx.prepare(
                "SELECT t.id, t.name, f.path FROM tables t
                 LEFT JOIN files f ON f.id = t.evidence_file_id
                 WHERE t.project_id = ?1 ORDER BY t.name",
            )?;
            let tables: Vec<(i64, String, Option<String>)> = stmt
                .query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut cols = tx.prepare(
                "SELECT name, java_type, doc FROM columns WHERE table_id = ?1 ORDER BY is_pk DESC, name",
            )?;
            for (id, name, file) in tables {
                let mut body = String::new();
                for row in cols.query_map(params![id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?))
                })? {
                    let (cname, ty, doc) = row?;
                    let ty = ty.unwrap_or_default();
                    if body.is_empty() {
                        body = format!("{cname} {ty}");
                    } else {
                        body.push_str(&format!("；{cname} {ty}"));
                    }
                    if let Some(d) = doc.filter(|d| !d.trim().is_empty()) {
                        body.push_str(&format!(" {d}"));
                    }
                }
                put(&mut ins, &name, &body, "table", "", id, file.as_deref())?;
                n += 1;
            }
        }

        // Entrypoints: addr is the headline, name and doc the searchable body.
        {
            let mut stmt = tx.prepare(
                "SELECT e.id, e.kind, e.name, e.addr, e.doc, f.path FROM entrypoints e
                 LEFT JOIN files f ON f.id = e.file_id
                 WHERE e.project_id = ?1",
            )?;
            let rows: Vec<(i64, String, String, Option<String>, Option<String>, Option<String>)> = stmt
                .query_map(params![project_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            for (id, kind, name, addr, doc, file) in rows {
                let title = addr.clone().unwrap_or_else(|| name.clone());
                let body = std::iter::once(name).chain(doc).collect::<Vec<_>>().join("\n");
                put(&mut ins, &title, &body, "entrypoint", &kind, id, file.as_deref())?;
                n += 1;
            }
        }

        // Domains, glossary, notes and commit subjects round out the vocabulary.
        {
            let mut stmt = tx.prepare(
                "SELECT id, key, label, rationale_json FROM domains WHERE project_id = ?1",
            )?;
            let rows: Vec<(i64, String, Option<String>, Option<String>)> = stmt
                .query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, key, label, rationale) in rows {
                let title = label.clone().unwrap_or_else(|| key.clone());
                let body = std::iter::once(key).chain(rationale).collect::<Vec<_>>().join("\n");
                put(&mut ins, &title, &body, "domain", "", id, None)?;
                n += 1;
            }
        }
        {
            let mut stmt = tx.prepare(
                "SELECT id, COALESCE(title, kind), kind, body_md FROM notes WHERE project_id = ?1",
            )?;
            let rows: Vec<(i64, String, String, String)> = stmt
                .query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, title, kind, body) in rows {
                put(&mut ins, &title, &body, "note", &kind, id, None)?;
                n += 1;
            }
        }
        {
            let mut stmt = tx.prepare(
                "SELECT id, term, definition_md, aliases_json FROM glossary WHERE project_id = ?1",
            )?;
            let rows: Vec<(i64, String, Option<String>, Option<String>)> = stmt
                .query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, term, definition, aliases) in rows {
                let aliases: Vec<String> = aliases
                    .as_deref()
                    .and_then(|j| serde_json::from_str(j).ok())
                    .unwrap_or_default();
                let body = definition
                    .into_iter()
                    .chain(aliases.into_iter())
                    .collect::<Vec<_>>()
                    .join("\n");
                put(&mut ins, &term, &body, "glossary", "", id, None)?;
                n += 1;
            }
        }
        {
            let mut stmt = tx.prepare(
                "SELECT id, subject FROM git_commits WHERE project_id = ?1 AND subject IS NOT NULL",
            )?;
            let rows: Vec<(i64, String)> = stmt
                .query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, subject) in rows {
                put(&mut ins, &subject, "", "commit", "", id, None)?;
                n += 1;
            }
        }
    }
    tx.commit()?;
    Ok(n)
}

/// Search the index, ranked by relevance, best match first.
pub fn query(store: &Store, project_id: i64, text: &str, limit: usize) -> Result<Vec<Hit>> {
    let q = text.split_whitespace().collect::<Vec<_>>().join(" ");
    anyhow::ensure!(!q.is_empty(), "查询词为空");
    // Trigram needs ≥3 contiguous characters to form a single token, so a one-
    // or two-character query would match nothing — an especially bad failure for
    // CJK, where two characters is a complete business term (特价). Those short
    // queries fall back to a substring scan over the same columns.
    if q.chars().count() < 3 {
        return like_query(store, project_id, &q, limit);
    }
    let fts = to_fts_query(&q);
    let mut stmt = store
        .conn
        .prepare(
            "SELECT kind, subject_kind, subject_id, title, body, file_path
             FROM search_fts WHERE search_fts MATCH ?1 AND project_id = ?2
             ORDER BY bm25(search_fts) LIMIT ?3",
        )
        .context("查询失败——索引是否已构建？运行 `catlas build` 后再试")?;
    let rows = stmt.query_map(params![fts, project_id, limit as i64], |r| {
        Ok(Hit {
            kind: r.get(0)?,
            subject_kind: r.get(1)?,
            subject_id: r.get(2)?,
            title: r.get(3)?,
            snippet: truncate_chars(&r.get::<_, String>(4)?, 160),
            file: r.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Substring scan for queries too short for trigram (1–2 characters).
///
/// Runs only for short queries, where a full scan of the index is cheap relative
/// to the value of actually finding the term. Matches the same two columns the
/// FTS path indexes (`title`, `body`) so a short query and a long query see a
/// consistent surface.
fn like_query(store: &Store, project_id: i64, q: &str, limit: usize) -> Result<Vec<Hit>> {
    let pattern = format!("%{}%", escape_like(q));
    let mut stmt = store.conn.prepare(
        "SELECT kind, subject_kind, subject_id, title, body, file_path
         FROM search_fts
         WHERE project_id = ?1 AND (title LIKE ?2 ESCAPE '\\' OR body LIKE ?2 ESCAPE '\\')
         ORDER BY title LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![project_id, pattern, limit as i64], |r| {
        Ok(Hit {
            kind: r.get(0)?,
            subject_kind: r.get(1)?,
            subject_id: r.get(2)?,
            title: r.get(3)?,
            snippet: truncate_chars(&r.get::<_, String>(4)?, 160),
            file: r.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Escape a literal for use inside a `LIKE` pattern, so `%`, `_` and `\` in the
/// query match themselves rather than acting as wildcards.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Wrap the query as an FTS5 phrase so none of its characters are parsed as
/// match syntax. Inside a quoted phrase only `"` is special, doubled to escape.
fn to_fts_query(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// Human label for a hit, e.g. `符号·方法` or `入口·Dubbo`.
pub fn label(kind: &str, subject_kind: &str) -> String {
    let (head, sub) = match kind {
        "symbol" => ("符号", symbol_label(subject_kind)),
        "entrypoint" => ("入口", entrypoint_label(subject_kind)),
        "file" => ("文件", file_label(subject_kind)),
        "table" => ("数据表", ""),
        "domain" => ("领域", ""),
        "note" => ("文档", subject_kind),
        "glossary" => ("术语", ""),
        "commit" => ("提交", ""),
        _ => (kind, subject_kind),
    };
    if sub.is_empty() {
        head.to_string()
    } else {
        format!("{head}·{sub}")
    }
}

fn symbol_label(kind: &str) -> &str {
    match kind {
        "class" => "类",
        "interface" => "接口",
        "enum" => "枚举",
        "enum_member" => "枚举值",
        "method" => "方法",
        "field" => "字段",
        "constant" => "常量",
        "annotation" => "注解",
        "record" => "记录",
        _ => kind,
    }
}

fn entrypoint_label(kind: &str) -> &str {
    match kind {
        "dubbo" => "Dubbo",
        "http" => "HTTP",
        "job" => "任务",
        "mq" => "MQ",
        _ => kind,
    }
}

fn file_label(lang: &str) -> &str {
    match lang {
        "java" => "Java",
        "xml" => "XML",
        "properties" => "properties",
        "yaml" => "YAML",
        "sql" => "SQL",
        _ => lang,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn store_with_symbol() -> (tempfile::TempDir, Store, i64) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let pid = store.ensure_project("p", dir.path()).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO files(project_id, path, lang, sha256, loc) VALUES (?1, 'a/CouponEnum.java', 'java', 'x', 1)",
                params![pid],
            )
            .unwrap();
        let fid = store.conn.last_insert_rowid();
        store
            .conn
            .execute(
                "INSERT INTO symbols(project_id, file_id, kind, name, fqn, doc, start_line, end_line)
                 VALUES (?1, ?2, 'enum_member', 'TEJIA', 'a.CouponEnum.TEJIA', '特价活动说明', 1, 1)",
                params![pid, fid],
            )
            .unwrap();
        (dir, store, pid)
    }

    #[test]
    fn rebuild_indexes_and_query_finds_cjk_substring() {
        let (_d, store, pid) = store_with_symbol();
        let n = rebuild(&store, pid).unwrap();
        assert!(n >= 1);
        // The doc carries a 4+ char CJK run, which trigram can match.
        let hits = query(&store, pid, "特价活动", 10).unwrap();
        assert!(hits.iter().any(|h| h.title.contains("TEJIA")));
        assert_eq!(hits[0].file.as_deref(), Some("a/CouponEnum.java"));
        // A 2-char CJK query is below trigram's floor; it now falls back to a
        // substring scan and still finds the term.
        let hits = query(&store, pid, "特价", 10).unwrap();
        assert!(hits.iter().any(|h| h.title.contains("TEJIA")));
        // Single character works too.
        assert!(query(&store, pid, "价", 10).unwrap().iter().any(|h| h.title.contains("TEJIA")));
    }

    #[test]
    fn short_query_escapes_like_wildcards() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let pid = store.ensure_project("p", dir.path()).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO search_fts(title, body, kind, subject_kind, subject_id, project_id, file_path)
                 VALUES ('x%y', '', 'symbol', 'class', 1, ?1, NULL)",
                params![pid],
            )
            .unwrap();
        // A literal `%` in the query must not be treated as a wildcard.
        assert_eq!(query(&store, pid, "%", 10).unwrap()[0].title, "x%y");
        assert!(query(&store, pid, "z", 10).unwrap().is_empty());
    }

    #[test]
    fn query_is_case_insensitive_for_identifiers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let pid = store.ensure_project("p", dir.path()).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO search_fts(title, body, kind, subject_kind, subject_id, project_id, file_path)
                 VALUES ('CouponService', '', 'symbol', 'interface', 1, ?1, NULL)",
                params![pid],
            )
            .unwrap();
        let hits = query(&store, pid, "coupon", 10).unwrap();
        assert!(hits.iter().any(|h| h.title == "CouponService"));
    }

    #[test]
    fn fts_query_escapes_double_quotes() {
        assert_eq!(to_fts_query("a\"b"), "\"a\"\"b\"");
        assert_eq!(to_fts_query("特价"), "\"特价\"");
    }

    #[test]
    fn empty_query_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let pid = store.ensure_project("p", dir.path()).unwrap();
        assert!(query(&store, pid, "   ", 10).is_err());
    }

    #[test]
    fn labels_map_kinds() {
        assert_eq!(label("symbol", "method"), "符号·方法");
        assert_eq!(label("entrypoint", "dubbo"), "入口·Dubbo");
        assert_eq!(label("table", ""), "数据表");
        assert_eq!(label("glossary", ""), "术语");
    }
}
