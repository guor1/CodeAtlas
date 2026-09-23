//! Insights: business claims captured from working sessions, human-confirmed
//! before they become searchable knowledge.
//!
//! An insight is what a Claude Code session (or a person) learns while working
//! on the code — "cancelling an order does NOT release its coupon", "the -1
//! there is legacy compatibility" — knowledge that is expensive to rediscover
//! and currently evaporates with the session.
//!
//! The lifecycle is deliberately asymmetric, and mirrors how review works in
//! the rest of the tool:
//!
//! * [`propose`] writes a `candidate` row. Nothing machine-written enters the
//!   search index — a candidate the model keeps restating must not accumulate
//!   the appearance of confirmation.
//! * [`review`] is the human gate, exposed only through the CLI. There is
//!   deliberately no MCP confirm tool: the client that proposes cannot be the
//!   one that confirms.
//! * `confirmed` rows are indexed by [`crate::search`] like any other note;
//!   `rejected` rows stay for audit but are never indexed.
//!
//! Insights live in `notes` rows whose `kind` is one of [`KINDS`] and whose
//! `subject_kind` is `'insight'` — the deepen pipeline's DELETEs key on
//! `subject_kind = 'domain'/'entrypoint'` and never touch them. They carry no
//! `source_digest`: invalidation is a human act, not automatic.

use crate::store::Store;
use anyhow::{Context, Result };
use rusqlite::params;
use serde::Deserialize;

/// The three insight kinds worth keeping, in the spirit of the dossier model.
pub const KINDS: &[&str] = &["business_rule", "landmine", "term"];

/// Human label for a kind, matching the tone of [`crate::search::label`].
pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "business_rule" => "业务规则",
        "landmine" => "坑",
        "term" => "术语",
        _ => "洞察",
    }
}

/// One code location backing a claim: enough for a reviewer to jump there.
#[derive(Debug, Clone, Deserialize)]
pub struct Evidence {
    /// Repository-relative file path, required.
    pub file: String,
    /// Symbol (fqn or simple name) when known.
    pub symbol: Option<String>,
    /// Line range when known, e.g. "120-145".
    pub lines: Option<String>,
}

/// An incoming insight as proposed by a session.
#[derive(Debug, Deserialize)]
pub struct Insight {
    pub kind: String,
    /// One-sentence claim. Short by design: it doubles as the search title.
    pub title: String,
    /// Conditions, exceptions and unknowns as Markdown.
    pub body: String,
    /// Domain key to file the insight under, when the domain is known.
    pub domain: Option<String>,
    /// Code locations. At least one is required — an unverifiable claim
    /// cannot be reviewed, and confirmation without verification is theatre.
    pub evidence: Vec<Evidence>,
}

/// Validate and insert, returning the new note's row id.
///
/// Validation failures say exactly what is missing so an MCP client can fix
/// the call and retry without a human in the loop.
pub fn propose(store: &Store, project_id: i64, ins: &Insight) -> Result<i64> {
    if !KINDS.contains(&ins.kind.as_str()) {
        anyhow::bail!("kind 必须是 {} 之一，收到 `{}`", KINDS.join("/"), ins.kind);
    }
    let title = ins.title.trim();
    if title.is_empty() {
        anyhow::bail!("title 不能为空：用一句话概括这个结论");
    }
    if title.chars().count() > 120 {
        anyhow::bail!("title 过长（{} 字符），压缩到 120 以内——它同时是检索标题", title.chars().count());
    }
    let body = ins.body.trim();
    if body.is_empty() {
        anyhow::bail!("body 不能为空：写清条件、例外或适用范围");
    }
    let evidence: Vec<&Evidence> = ins
        .evidence
        .iter()
        .filter(|e| !e.file.trim().is_empty())
        .collect();
    if evidence.is_empty() {
        anyhow::bail!("至少一条证据：evidence[].file 不能为空——没有代码位置的结论无法复核");
    }

    let domain_key = ins.domain.as_deref().map(str::trim).filter(|d| !d.is_empty());
    let domain_id: Option<i64> = match domain_key {
        Some(key) => Some(
            store
                .conn
                .query_row(
                    "SELECT id FROM domains WHERE project_id = ?1 AND key = ?2",
                    params![project_id, key],
                    |r| r.get(0),
                )
                .with_context(|| format!("领域 `{key}` 不存在，用 `catlas domains` 查看"))?,
        ),
        None => None,
    };

    store
        .conn
        .execute(
            "INSERT INTO notes(project_id, kind, subject_kind, subject_id, subject_key, title,
                               body_md, status, model, generated_at)
             VALUES (?1, ?2, 'insight', ?3, ?4, ?5, ?6, 'candidate', 'claude-session', ?7)",
            params![
                project_id,
                ins.kind,
                domain_id,
                domain_key.unwrap_or(""),
                title,
                body,
                crate::util::now_iso(),
            ],
        )
        .context("写入洞察失败")?;
    let note_id = store.conn.last_insert_rowid();

    for e in evidence {
        // ref_text carries the whole location: ref_id would dangle on the next
        // `build`, which recreates file rows.
        let loc = match (&e.symbol, &e.lines) {
            (Some(s), Some(l)) => format!("{}:{s}:{l}", e.file.trim()),
            (Some(s), None) => format!("{}:{s}", e.file.trim()),
            (None, Some(l)) => format!("{}:{l}", e.file.trim()),
            (None, None) => e.file.trim().to_string(),
        };
        store.conn.execute(
            "INSERT INTO note_evidence(note_id, kind, ref_text) VALUES (?1, 'file', ?2)",
            params![note_id, loc],
        )?;
    }
    Ok(note_id)
}

/// A listed insight, with everything the reviewer needs on one screen.
#[derive(Debug)]
pub struct InsightRow {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub status: String,
    pub domain: Option<String>,
    pub body: String,
    pub created: String,
    /// Evidence locations as `path:symbol:lines` strings.
    pub evidence: Vec<String>,
}

/// List insights by status: `candidate` (default), `confirmed`, `rejected`, `all`.
pub fn list(store: &Store, project_id: i64, status: &str) -> Result<Vec<InsightRow>> {
    let (filter, need_status): (&str, bool) = match status {
        "all" => ("", false),
        "candidate" | "confirmed" | "rejected" => (" AND status = ?2", true),
        other => anyhow::bail!("status 必须是 candidate/confirmed/rejected/all，收到 `{other}`"),
    };
    let sql = format!(
        "SELECT n.id, n.kind, n.title, n.status, n.subject_key, n.body_md, n.generated_at
         FROM notes n
         WHERE n.project_id = ?1 AND n.kind IN ({kinds}){filter}
         ORDER BY n.id DESC",
        kinds = KINDS.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(","),
    );
    let mut stmt = store.conn.prepare(&sql)?;
    let rows = if need_status {
        stmt.query_map(params![project_id, status], map_row)?
    } else {
        stmt.query_map(params![project_id], map_row)?
    };
    let mut out: Vec<InsightRow> = rows.collect::<rusqlite::Result<_>>()?;
    for r in &mut out {
        let mut ev = store.conn.prepare(
            "SELECT ref_text FROM note_evidence WHERE note_id = ?1 ORDER BY rowid",
        )?;
        r.evidence = ev
            .query_map(params![r.id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
    }
    Ok(out)
}

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<InsightRow> {
    let domain: Option<String> = r.get(4)?;
    Ok(InsightRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        title: r.get(2)?,
        status: r.get(3)?,
        domain: domain.filter(|d| !d.is_empty()),
        body: r.get(5)?,
        created: r.get(6)?,
        evidence: Vec::new(),
    })
}

/// Human verdict on one insight: `accept` promotes it into the search index,
/// otherwise it is rejected but kept for audit.
///
/// Returns whether the row existed, so the CLI can name an unknown id instead
/// of reporting success for a no-op.
pub fn review(store: &Store, project_id: i64, id: i64, accept: bool) -> Result<bool> {
    let next = if accept { "confirmed" } else { "rejected" };
    let kinds = KINDS.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(",");
    let n = store.conn.execute(
        &format!(
            "UPDATE notes SET status = ?1
             WHERE id = ?2 AND project_id = ?3 AND kind IN ({kinds}) AND status = 'candidate'"
        ),
        params![next, id, project_id],
    )?;
    if n == 0 {
        return Ok(false);
    }
    // The FTS index is a derived view rebuilt on demand; refresh it so an
    // accepted insight is searchable immediately (and a rejected one, were it
    // indexed, would drop out).
    if accept {
        crate::search::rebuild(store, project_id)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Store, i64) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let pid = store.ensure_project("p", dir.path()).unwrap();
        (dir, store, pid)
    }

    fn sample(kind: &str) -> Insight {
        Insight {
            kind: kind.into(),
            title: "待支付订单主动取消不释放优惠券".into(),
            body: "仅用户主动取消走 `releaseCoupon=false` 分支；超时关闭路径未验证。".into(),
            domain: None,
            evidence: vec![Evidence {
                file: "src/main/java/com/demo/OrderService.java".into(),
                symbol: Some("OrderService.cancel".into()),
                lines: Some("120-145".into()),
            }],
        }
    }

    #[test]
    fn propose_list_review_round_trip() {
        let (_d, store, pid) = setup();
        let id = propose(&store, pid, &sample("business_rule")).unwrap();

        let pending = list(&store, pid, "candidate").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].evidence, vec!["src/main/java/com/demo/OrderService.java:OrderService.cancel:120-145"]);

        assert!(review(&store, pid, id, true).unwrap());
        assert!(list(&store, pid, "candidate").unwrap().is_empty());
        let done = list(&store, pid, "confirmed").unwrap();
        assert_eq!(done.len(), 1);
        // Already confirmed: a second verdict is not a silent no-op.
        assert!(!review(&store, pid, id, true).unwrap());
    }

    #[test]
    fn rejected_stays_for_audit_but_out_of_search() {
        let (_d, store, pid) = setup();
        let a = propose(&store, pid, &sample("landmine")).unwrap();
        let b = propose(&store, pid, &sample("business_rule")).unwrap();
        assert!(review(&store, pid, a, false).unwrap());
        assert!(review(&store, pid, b, true).unwrap());

        crate::search::rebuild(&store, pid).unwrap();
        let hits = crate::search::query(&store, pid, "优惠券", 10).unwrap();
        let ids: Vec<i64> = hits.iter().filter(|h| h.kind == "note").map(|h| h.subject_id).collect();
        assert!(ids.contains(&b), "confirmed insight should be indexed");
        assert!(!ids.contains(&a), "rejected insight must not be indexed");
        // Still listed for audit.
        assert_eq!(list(&store, pid, "rejected").unwrap().len(), 1);
    }

    #[test]
    fn candidate_is_not_indexed_until_confirmed() {
        let (_d, store, pid) = setup();
        let id = propose(&store, pid, &sample("term")).unwrap();
        crate::search::rebuild(&store, pid).unwrap();
        let hits = crate::search::query(&store, pid, "优惠券", 10).unwrap();
        assert!(
            !hits.iter().filter(|h| h.kind == "note").any(|h| h.subject_id == id),
            "a candidate must not appear in search before human confirmation"
        );
    }

    #[test]
    fn validation_rejects_missing_evidence_and_bad_kind() {
        let (_d, store, pid) = setup();
        let mut no_evidence = sample("business_rule");
        no_evidence.evidence.clear();
        let err = propose(&store, pid, &no_evidence).unwrap_err().to_string();
        assert!(err.contains("证据"), "{err}");

        let mut blank_file = sample("business_rule");
        blank_file.evidence[0].file = "  ".into();
        assert!(propose(&store, pid, &blank_file).is_err());

        let err = propose(&store, pid, &sample("opinion")).unwrap_err().to_string();
        assert!(err.contains("business_rule"), "{err}");

        let mut no_title = sample("term");
        no_title.title = "  ".into();
        assert!(propose(&store, pid, &no_title).is_err());

        let mut long_title = sample("term");
        long_title.title = "长".repeat(121);
        assert!(propose(&store, pid, &long_title).is_err());
    }

    #[test]
    fn unknown_domain_is_rejected_with_the_key() {
        let (_d, store, pid) = setup();
        let mut ins = sample("business_rule");
        ins.domain = Some("no-such-domain".into());
        let err = propose(&store, pid, &ins).unwrap_err().to_string();
        assert!(err.contains("no-such-domain"), "{err}");
    }

    #[test]
    fn review_names_an_unknown_id() {
        let (_d, store, pid) = setup();
        assert!(!review(&store, pid, 999, true).unwrap());
    }
}
