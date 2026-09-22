//! The `deepen` pipeline: turn evidence packs into stored notes and glossary
//! entries, each carrying the digest of the evidence it was derived from.

use super::{client, pack, prompt};
use crate::search;
use crate::store::Store;
use crate::util;
use anyhow::{Context, Result};
use rusqlite::params;
use std::fmt::Write as _;

pub struct DeepenOptions {
    /// Restrict to these domain keys; empty means every domain.
    pub domains: Vec<String>,
    /// Print the plan and estimated cost without calling the API.
    pub dry_run: bool,
    /// Process at most this many domains, smallest-risk first.
    pub limit: Option<usize>,
    /// Skip domains that already have fresh notes.
    pub only_stale: bool,
    /// Minimum domain size worth spending tokens on.
    pub min_files: i64,
    /// Generate capability narratives for entrypoints instead of domain dossiers.
    pub capability: bool,
    /// Restrict capability generation to these entrypoint kinds (dubbo/http/job/mq).
    pub kinds: Vec<String>,
}

impl Default for DeepenOptions {
    fn default() -> Self {
        Self {
            domains: Vec::new(),
            dry_run: false,
            limit: None,
            only_stale: true,
            // A domain of one or two files is faster to read than to document.
            min_files: 3,
            capability: false,
            kinds: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct DeepenStats {
    pub domains_processed: usize,
    pub domains_skipped: usize,
    pub notes_written: usize,
    pub glossary_written: usize,
    /// Tokens actually billed by this run.
    pub input_tokens: usize,
    pub output_tokens: usize,
    /// Tokens the cache avoided spending.
    pub saved_input_tokens: usize,
    pub saved_output_tokens: usize,
    pub cache_hits: usize,
    pub failures: Vec<(String, String)>,
    /// Populated in dry-run mode.
    pub planned: Vec<(String, usize)>,
}

struct Candidate {
    id: i64,
    key: String,
    files: i64,
}

fn candidates(store: &Store, project_id: i64, opts: &DeepenOptions) -> Result<Vec<Candidate>> {
    let rows: Vec<Candidate> = store
        .conn
        .prepare(
            "SELECT d.id, d.key,
                    (SELECT COUNT(*) FROM domain_members m
                     WHERE m.domain_id = d.id AND m.kind = 'file')
             FROM domains d WHERE d.project_id = ?1
             ORDER BY 3 DESC",
        )?
        .query_map(params![project_id], |r| {
            Ok(Candidate { id: r.get(0)?, key: r.get(1)?, files: r.get(2)? })
        })?
        .collect::<rusqlite::Result<_>>()?;

    Ok(rows
        .into_iter()
        .filter(|c| opts.domains.is_empty() || opts.domains.contains(&c.key))
        .filter(|c| c.files >= opts.min_files)
        .collect())
}

pub fn run(
    store: &Store,
    cfg: &client::Config,
    opts: &DeepenOptions,
) -> Result<DeepenStats> {
    let project_id = store.project_id()?;
    let mut stats = DeepenStats::default();

    let mut todo = candidates(store, project_id, opts)?;
    if !opts.domains.is_empty() {
        // Surface a typo rather than silently doing nothing.
        for want in &opts.domains {
            if !todo.iter().any(|c| &c.key == want) {
                anyhow::bail!(
                    "找不到领域 `{want}`（或其文件数低于 {}）。用 `catlas domains` 查看可用领域",
                    opts.min_files
                );
            }
        }
    }
    if let Some(n) = opts.limit {
        todo.truncate(n);
    }

    for c in todo {
        if opts.only_stale && !opts.dry_run && is_fresh(store, project_id, c.id)? {
            stats.domains_skipped += 1;
            continue;
        }

        let pack = pack::build(store, project_id, c.id)
            .with_context(|| format!("为领域 `{}` 组装证据失败", c.key))?;

        if opts.dry_run {
            stats.planned.push((c.key.clone(), pack.estimated_tokens));
            continue;
        }

        // A failure on one domain must not abandon the rest of the run: these
        // are expensive, independent units of work.
        match process(store, cfg, project_id, &c, &pack, &mut stats) {
            Ok(()) => stats.domains_processed += 1,
            Err(e) => stats.failures.push((c.key.clone(), format!("{e:#}"))),
        }
    }
    // The search index is a derived view; refresh it so new notes and glossary
    // entries are findable without a rebuild.
    search::rebuild(store, project_id)?;
    Ok(stats)
}

/// An entrypoint worth writing a capability narrative for.
struct EntryCandidate {
    id: i64,
    subject_key: String,
}

/// Candidate entrypoints for capability generation.
///
/// An entrypoint with neither a trace nor a doc gives the model nothing to
/// ground a narrative on, so it is dropped rather than billed. The default
/// selection is entrypoints that reach a table (the "what gets written" fact is
/// the whole point); `--kind`/`--domain` narrow it further.
fn capability_candidates(
    store: &Store,
    project_id: i64,
    opts: &DeepenOptions,
) -> Result<Vec<EntryCandidate>> {
    let mut sql = String::from(
        "SELECT e.id, e.kind, COALESCE(e.addr, ''), e.doc,
                (SELECT COUNT(*) FROM traces t WHERE t.entrypoint_id = e.id)
         FROM entrypoints e
         WHERE e.project_id = ?1",
    );
    let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(project_id)];
    if !opts.kinds.is_empty() {
        sql.push_str(" AND e.kind IN (");
        for (i, k) in opts.kinds.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push('?');
            args.push(Box::new(k.clone()));
        }
        sql.push(')');
    }
    if !opts.domains.is_empty() {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM domain_members m
                          WHERE m.kind = 'entrypoint' AND m.ref_id = e.id
                            AND m.domain_id IN
                              (SELECT id FROM domains d
                               WHERE d.project_id = e.project_id AND d.key IN (",
        );
        for (i, d) in opts.domains.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push('?');
            args.push(Box::new(d.clone()));
        }
        sql.push_str(")))");
    }

    let mut stmt = store.conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, kind, addr, doc, traces) = row?;
        let has_doc = doc.is_some_and(|d| !d.trim().is_empty());
        let has_trace = traces > 0;
        // With no explicit filters, keep only entrypoints that reach a table;
        // a targeted run may still want the documented-but-traceless ones.
        let targeted = !opts.kinds.is_empty() || !opts.domains.is_empty();
        if has_trace || (targeted && has_doc) {
            out.push(EntryCandidate { id, subject_key: format!("{kind}:{addr}") });
        }
    }
    Ok(out)
}

/// True when this entrypoint already has a fresh capability note for the
/// current evidence.
fn is_capability_fresh(store: &Store, project_id: i64, subject_key: &str, digest: &str) -> Result<bool> {
    let n: i64 = store.conn.query_row(
        "SELECT COUNT(*) FROM notes
         WHERE project_id = ?1 AND kind = 'capability' AND subject_key = ?2
           AND status = 'fresh' AND source_digest = ?3",
        params![project_id, subject_key, digest],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Capability-generation pass: one narrative per selected entrypoint.
pub fn run_capabilities(
    store: &Store,
    cfg: &client::Config,
    opts: &DeepenOptions,
) -> Result<DeepenStats> {
    let project_id = store.project_id()?;
    let mut stats = DeepenStats::default();

    let mut todo = capability_candidates(store, project_id, opts)?;
    if let Some(n) = opts.limit {
        todo.truncate(n);
    }

    for c in todo {
        let pack = pack::build_capability(store, project_id, c.id)
            .with_context(|| format!("为入口 `{}` 组装证据失败", c.subject_key))?;

        if opts.dry_run {
            stats.planned.push((c.subject_key.clone(), pack.estimated_tokens));
            continue;
        }
        if opts.only_stale && is_capability_fresh(store, project_id, &c.subject_key, &pack.source_digest)? {
            stats.domains_skipped += 1;
            continue;
        }
        match process_capability(store, cfg, project_id, &c, &pack, &mut stats) {
            Ok(()) => stats.domains_processed += 1,
            Err(e) => stats.failures.push((c.subject_key.clone(), format!("{e:#}"))),
        }
    }
    search::rebuild(store, project_id)?;
    Ok(stats)
}

fn process_capability(
    store: &Store,
    cfg: &client::Config,
    project_id: i64,
    c: &EntryCandidate,
    pack: &pack::CapabilityPack,
    stats: &mut DeepenStats,
) -> Result<()> {
    // Replace any prior narrative for this entrypoint; a stale one lingers
    // otherwise, and `subject_key` is what makes it stable across rebuilds.
    store.conn.execute(
        "DELETE FROM notes WHERE project_id = ?1 AND kind = 'capability' AND subject_key = ?2",
        params![project_id, c.subject_key],
    )?;

    let cap = complete_parsed::<prompt::CapabilityResponse>(
        store,
        cfg,
        &prompt::capability_system(),
        &pack.body,
        stats,
    )?;
    let body = render_capability(&cap);
    store.conn.execute(
        "INSERT INTO notes(project_id, kind, subject_kind, subject_id, subject_key, title,
                           body_md, status, model, prompt_hash, source_digest, generated_at)
         VALUES (?1, 'capability', 'entrypoint', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            project_id,
            c.id,
            c.subject_key,
            c.subject_key,
            body,
            capability_status(&cap),
            cfg.model,
            util::digest_parts([prompt::capability_system().as_str(), pack.body.as_str()]),
            pack.source_digest,
            util::now_iso(),
        ],
    )?;
    stats.notes_written += 1;
    Ok(())
}

/// Flag thin capability output for review, same spirit as [`note_status`].
fn capability_status(c: &prompt::CapabilityResponse) -> &'static str {
    if c.summary.trim().is_empty() || c.summary.contains("UNKNOWN") {
        "needs_review"
    } else {
        "fresh"
    }
}

/// Render a capability narrative as Markdown for the reference page.
fn render_capability(c: &prompt::CapabilityResponse) -> String {
    let mut s = String::new();
    if !c.summary.trim().is_empty() {
        let _ = writeln!(s, "{}\n", c.summary.trim());
    }
    if !c.inputs.is_empty() {
        let _ = writeln!(s, "**入参**\n");
        let _ = writeln!(s, "| 参数 | 含义 | 必填 |");
        let _ = writeln!(s, "|---|---|---|");
        for i in &c.inputs {
            let _ = writeln!(
                s,
                "| `{}` | {} | {} |",
                crate::render::cell(&i.name),
                crate::render::cell(&i.meaning),
                if i.required { "是" } else { "否" }
            );
        }
        let _ = writeln!(s);
    }
    if !c.behavior.trim().is_empty() {
        let _ = writeln!(s, "**执行过程**\n\n{}\n", c.behavior.trim());
    }
    if !c.side_effects.is_empty() {
        let _ = writeln!(s, "**副作用**\n");
        for e in &c.side_effects {
            if !e.trim().is_empty() {
                let _ = writeln!(s, "- {}", e.trim());
            }
        }
        let _ = writeln!(s);
    }
    if !c.rules.is_empty() {
        let _ = writeln!(s, "**前置条件 / 约束**\n");
        for r in &c.rules {
            if !r.trim().is_empty() {
                let _ = writeln!(s, "- {}", r.trim());
            }
        }
        let _ = writeln!(s);
    }
    if !c.caveats.is_empty() {
        let _ = writeln!(s, "**易踩的坑**\n");
        for w in &c.caveats {
            if !w.trim().is_empty() {
                let _ = writeln!(s, "- {}", w.trim());
            }
        }
        let _ = writeln!(s);
    }
    s
}

/// True when this domain already has notes derived from the current evidence.
fn is_fresh(store: &Store, project_id: i64, domain_id: i64) -> Result<bool> {
    let digest = pack::build(store, project_id, domain_id)?.source_digest;
    let n: i64 = store.conn.query_row(
        "SELECT COUNT(*) FROM notes
         WHERE project_id = ?1 AND subject_kind = 'domain' AND subject_id = ?2
           AND status = 'fresh' AND source_digest = ?3",
        params![project_id, domain_id, digest],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn process(
    store: &Store,
    cfg: &client::Config,
    project_id: i64,
    c: &Candidate,
    pack: &pack::Pack,
    stats: &mut DeepenStats,
) -> Result<()> {
    // Replace prior output for this domain so a rebuild does not accumulate
    // stale entries. Glossary rows need this as much as notes do: terms dropped
    // by a later run would otherwise linger forever, and a term the model no
    // longer supports is exactly the kind of stale claim this design exists to
    // prevent. `note_evidence` and `glossary_links` cascade on delete.
    store.conn.execute(
        "DELETE FROM notes WHERE project_id = ?1 AND subject_kind = 'domain' AND subject_id = ?2",
        params![project_id, c.id],
    )?;
    store.conn.execute(
        "DELETE FROM glossary WHERE project_id = ?1 AND domain_id = ?2",
        params![project_id, c.id],
    )?;

    let dossier = complete_parsed::<prompt::DossierResponse>(
        store,
        cfg,
        &prompt::dossier_system(),
        &pack.body,
        stats,
    )?;
    let body = render_dossier(&dossier);
    store.conn.execute(
        "INSERT INTO notes(project_id, kind, subject_kind, subject_id, subject_key, title,
                           body_md, status, model, prompt_hash, source_digest, generated_at)
         VALUES (?1, 'dossier', 'domain', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            project_id,
            c.id,
            c.key,
            "领域解读",
            body,
            note_status(&dossier),
            cfg.model,
            util::digest_parts([prompt::dossier_system().as_str(), pack.body.as_str()]),
            pack.source_digest,
            util::now_iso(),
        ],
    )?;
    let note_id = store.conn.last_insert_rowid();
    record_evidence(store, note_id, c.id)?;
    stats.notes_written += 1;

    let glossary = complete_parsed::<prompt::GlossaryResponse>(
        store,
        cfg,
        &prompt::glossary_system(),
        &pack.body,
        stats,
    )?;
    let prompt_hash = util::digest_parts([prompt::glossary_system().as_str(), pack.body.as_str()]);
    for term in &glossary.terms {
        if term.term.trim().is_empty() {
            continue;
        }
        store.conn.execute(
            "INSERT OR REPLACE INTO glossary(project_id, term, normalized, definition_md,
                                             aliases_json, scope, domain_id, domain_key, status,
                                             model, prompt_hash, source_digest)
             VALUES (?1, ?2, ?3, ?4, ?5, 'project', ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                project_id,
                term.term.trim(),
                util::to_snake(&term.term),
                term.definition,
                serde_json::to_string(&term.aliases)?,
                c.id,
                c.key,
                confidence_status(term.confidence.as_deref()),
                cfg.model,
                prompt_hash,
                pack.source_digest,
            ],
        )?;
        let gid = store.conn.last_insert_rowid();
        for r in &term.code_refs {
            store.conn.execute(
                "INSERT INTO glossary_links(glossary_id, kind, ref_text) VALUES (?1, 'code', ?2)",
                params![gid, r],
            )?;
        }
        stats.glossary_written += 1;
    }
    Ok(())
}

/// Note status: flag output that leaned on UNKNOWN for human review.
fn note_status(d: &prompt::DossierResponse) -> &'static str {
    let thin = d.responsibility.trim().is_empty()
        || d.responsibility.contains("UNKNOWN") && d.rules.is_empty();
    if thin { "needs_review" } else { "fresh" }
}

fn confidence_status(c: Option<&str>) -> &'static str {
    match c.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("low") => "needs_review",
        _ => "fresh",
    }
}

/// Link a note to the evidence rows it was derived from, so a reader can audit
/// any claim and a future `sync` can tell what to invalidate.
fn record_evidence(store: &Store, note_id: i64, domain_id: i64) -> Result<()> {
    store.conn.execute(
        "INSERT INTO note_evidence(note_id, kind, ref_id)
         SELECT ?1, m.kind, m.ref_id FROM domain_members m
         WHERE m.domain_id = ?2 AND m.kind IN ('table', 'entrypoint')",
        params![note_id, domain_id],
    )?;
    Ok(())
}

/// Call the model and parse the response, retrying once on a parse failure.
fn complete_parsed<T: for<'de> serde::Deserialize<'de>>(
    store: &Store,
    cfg: &client::Config,
    system: &str,
    user: &str,
    stats: &mut DeepenStats,
) -> Result<T> {
    let mut last_err = None;
    for attempt in 0..2 {
        // On the retry, restate the format requirement rather than resending an
        // identical prompt, which would hit the cache and fail the same way.
        let effective_system = if attempt == 0 {
            system.to_string()
        } else {
            format!("{system}\n\n重要：上一次响应不是合法 JSON。只输出 JSON 对象本身，不要有任何其它内容。")
        };
        // Always cache under the original prompt, so a retry's good response is
        // reused by later runs instead of being filed under a key nobody asks for.
        let done = client::complete_keyed(store, cfg, &effective_system, user, system)?;
        // Cached responses cost nothing; counting them as spend would misreport
        // the one number an operator uses to decide whether to keep going.
        if done.cached {
            stats.cache_hits += 1;
            stats.saved_input_tokens += done.usage.input_tokens;
            stats.saved_output_tokens += done.usage.output_tokens;
        } else {
            stats.input_tokens += done.usage.input_tokens;
            stats.output_tokens += done.usage.output_tokens;
        }
        match prompt::parse_json::<T>(&done.text) {
            Ok(v) => {
                // Cache only what parsed: a truncated response must not be
                // replayed on every future run.
                done.persist(store)?;
                return Ok(v);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.expect("loop runs at least once"))
}

/// Render a dossier as the Markdown that lands in the domain page.
fn render_dossier(d: &prompt::DossierResponse) -> String {
    let mut s = String::new();
    if !d.responsibility.trim().is_empty() {
        let _ = writeln!(s, "{}\n", d.responsibility.trim());
    }

    if !d.key_concepts.is_empty() {
        let _ = writeln!(s, "### 核心概念\n");
        for c in &d.key_concepts {
            let _ = writeln!(s, "- **{}** — {}", c.name, c.description.trim());
        }
        let _ = writeln!(s);
    }

    if let Some(lc) = &d.lifecycle {
        if !lc.description.trim().is_empty() || !lc.states.is_empty() {
            let _ = writeln!(s, "### 生命周期\n");
            if !lc.description.trim().is_empty() {
                let _ = writeln!(s, "{}\n", lc.description.trim());
            }
            if !lc.states.is_empty() {
                let _ = writeln!(s, "| 状态 | 取值 | 含义 | 可流转到 |");
                let _ = writeln!(s, "|---|---|---|---|");
                for st in &lc.states {
                    let _ = writeln!(
                        s,
                        "| {} | {} | {} | {} |",
                        crate::render::cell(&st.state),
                        st.code_value.as_deref().unwrap_or("—"),
                        crate::render::cell(&st.meaning),
                        if st.transitions_to.is_empty() {
                            "—".to_string()
                        } else {
                            st.transitions_to.join("、")
                        }
                    );
                }
                let _ = writeln!(s);
            }
        }
    }

    if !d.rules.is_empty() {
        let _ = writeln!(s, "### 业务规则\n");
        for r in &d.rules {
            let mark = match r.confidence.as_deref() {
                Some("low") => " *(依据较弱，需确认)*",
                Some("medium") => " *(依据一般)*",
                _ => "",
            };
            let _ = writeln!(s, "- {}{mark}", r.rule.trim());
            if let Some(e) = r.evidence.as_deref().filter(|e| !e.trim().is_empty()) {
                let _ = writeln!(s, "  - 依据：{}", e.trim());
            }
        }
        let _ = writeln!(s);
    }

    if !d.landmines.is_empty() {
        let _ = writeln!(s, "### ⚠️ 已知陷阱\n");
        for l in &d.landmines {
            let _ = writeln!(s, "- **{}**", l.issue.trim());
            if !l.why.trim().is_empty() {
                let _ = writeln!(s, "  - 原因：{}", l.why.trim());
            }
            if let Some(e) = l.evidence.as_deref().filter(|e| !e.trim().is_empty()) {
                let _ = writeln!(s, "  - 依据：{}", e.trim());
            }
        }
        let _ = writeln!(s);
    }

    if !d.open_questions.is_empty() {
        let _ = writeln!(s, "### 待确认\n");
        for q in &d.open_questions {
            let _ = writeln!(s, "- {}", q.trim());
        }
        let _ = writeln!(s);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dossier_renders_all_sections() {
        let d: prompt::DossierResponse = prompt::parse_json(
            r#"{
              "responsibility": "管理优惠券的创建与发放。",
              "key_concepts": [{"name":"券模板","description":"一批券的共同配置"}],
              "lifecycle": {"description":"券从创建到过期。","states":[
                {"state":"生效中","code_value":"0","meaning":"可领取","transitions_to":["取消"]}]},
              "rules": [{"rule":"同一活动不可与满减叠加","evidence":"ConstantPromotion 注释","confidence":"high"},
                        {"rule":"弱依据规则","confidence":"low"}],
              "landmines": [{"issue":"命名与行为不一致","why":"历史遗留","evidence":"注释"}],
              "open_questions": ["黑名单优先级由谁决定？"]
            }"#,
        )
        .unwrap();
        let md = render_dossier(&d);
        assert!(md.contains("管理优惠券的创建与发放"));
        assert!(md.contains("### 核心概念"));
        assert!(md.contains("| 生效中 | 0 | 可领取 | 取消 |"));
        assert!(md.contains("### 业务规则"));
        assert!(md.contains("依据：ConstantPromotion 注释"));
        // Weak claims must be visibly marked, not presented as fact.
        assert!(md.contains("依据较弱，需确认"));
        assert!(md.contains("### ⚠️ 已知陷阱"));
        assert!(md.contains("### 待确认"));
    }

    #[test]
    fn sparse_dossier_renders_without_empty_headings() {
        let d: prompt::DossierResponse =
            prompt::parse_json(r#"{"responsibility":"只有职责说明。"}"#).unwrap();
        let md = render_dossier(&d);
        assert!(md.contains("只有职责说明"));
        assert!(!md.contains("### 核心概念"));
        assert!(!md.contains("### 生命周期"));
    }

    #[test]
    fn thin_output_is_flagged_for_review() {
        let thin: prompt::DossierResponse =
            prompt::parse_json(r#"{"responsibility":"UNKNOWN"}"#).unwrap();
        assert_eq!(note_status(&thin), "needs_review");
        let ok: prompt::DossierResponse =
            prompt::parse_json(r#"{"responsibility":"清楚的职责说明"}"#).unwrap();
        assert_eq!(note_status(&ok), "fresh");
    }

    #[test]
    fn low_confidence_terms_are_flagged() {
        assert_eq!(confidence_status(Some("low")), "needs_review");
        assert_eq!(confidence_status(Some("HIGH")), "fresh");
        assert_eq!(confidence_status(None), "fresh");
    }

    #[test]
    fn capability_renders_sections() {
        let c: prompt::CapabilityResponse = prompt::parse_json(
            r#"{
              "summary": "查询某活动下的缺陷商品清单。",
              "inputs": [{"name":"promotionId","meaning":"活动ID","required":true}],
              "behavior": "按活动 ID 查缺陷表后返回。",
              "side_effects": ["读 t_promotion_defective"],
              "rules": ["活动必须处于生效中"],
              "caveats": ["同名概念在不同模块含义不同"]
            }"#,
        )
        .unwrap();
        let md = render_capability(&c);
        assert!(md.contains("查询某活动下的缺陷商品清单"));
        assert!(md.contains("| `promotionId` | 活动ID | 是 |"));
        assert!(md.contains("**执行过程**"));
        assert!(md.contains("读 t_promotion_defective"));
        assert!(md.contains("**前置条件 / 约束**"));
        assert!(md.contains("**易踩的坑**"));
    }

    #[test]
    fn sparse_capability_renders_without_empty_headings() {
        let c: prompt::CapabilityResponse = prompt::parse_json(r#"{"summary":"只有摘要。"}"#).unwrap();
        let md = render_capability(&c);
        assert!(md.contains("只有摘要"));
        assert!(!md.contains("**入参**"));
        assert!(!md.contains("**副作用**"));
        assert!(!md.contains("**易踩的坑**"));
    }

    #[test]
    fn thin_capability_is_flagged_for_review() {
        let thin: prompt::CapabilityResponse = prompt::parse_json(r#"{"summary":"UNKNOWN"}"#).unwrap();
        assert_eq!(capability_status(&thin), "needs_review");
        let ok: prompt::CapabilityResponse = prompt::parse_json(r#"{"summary":"清楚的说明"}"#).unwrap();
        assert_eq!(capability_status(&ok), "fresh");
    }
}
