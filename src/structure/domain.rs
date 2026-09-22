//! Domain partitioning.
//!
//! Legacy projects have no explicit domain boundaries, but they leave four
//! independent traces of them: package names, table-name prefixes, which files
//! change together, and who calls whom. Each is individually unreliable —
//! packages lie after refactors, table prefixes collide, churn couples unrelated
//! code through shared utilities — so we combine them by weighted vote and let a
//! curated file override the result.

use crate::store::Store;
use crate::util::to_snake;
use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Package segments that describe layering or mechanics, never a business domain.
const INFRA_SEGMENTS: &[&str] = &[
    "impl", "util", "utils", "dto", "dtos", "bean", "beans", "vo", "po", "pojo",
    "entity", "common", "config", "constant", "constants", "enums", "interfaces",
    "service", "services", "model", "models", "web", "controller", "job", "jobs",
    "adapter", "pub", "mapper", "dao", "listener", "domain", "application",
    "infrastructure", "core", "api", "client", "remote", "facade", "manager",
    "helper", "support", "exception", "filter", "aspect", "handler", "convert",
    "converter", "builder", "factory", "request", "response", "query", "command",
    "event", "task", "test", "tests", "persistence", "repository", "resources",
    // Request/response shape suffixes, not subject matter.
    "req", "rsp", "res", "resp", "result", "param", "params", "form", "view",
    // Build and deployment layout that leaks in through non-Java files.
    "webapp", "web_inf", "meta_inf", "static", "assets", "jsp", "spock", "groovy",
    // Transport and integration mechanics. `dubbo` in particular collects every
    // RPC DTO in the project and would otherwise look like its largest domain,
    // while saying nothing about what the code does.
    "dubbo", "rpc", "http", "mq", "gateway", "spi", "external", "interceptor",
    "annotation", "excel", "excel_import", "excel_export", "import", "export",
];

/// Directory patterns whose files never describe business behaviour.
///
/// Tests and web assets would otherwise create domains of their own and pull
/// real files toward them through churn.
const EXCLUDED_PATHS: &[&str] = &["/src/test/", "/webapp/", "/WEB-INF/", "/META-INF/"];

/// Table prefix carried by every table in the projects we target.
const TABLE_PREFIX: &str = "t_";

/// Minimum files before a package segment is taken seriously as a domain.
const MIN_SEED_FILES: usize = 3;

/// Vote weights. Package placement is the strongest single signal but not
/// decisive on its own; the others exist to correct it where it has gone stale.
const W_PACKAGE: f64 = 3.0;
const W_TABLE: f64 = 2.0;
const W_CHURN: f64 = 1.0;
const W_CALL: f64 = 1.0;

/// Rounds of vote propagation. Two is enough to pull in a file's neighbours
/// without letting a hub file smear one domain across the project.
const PROPAGATION_ROUNDS: usize = 2;

/// Operator-maintained overrides, read from `.codeatlas/domains.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DomainConfig {
    /// Domain key → human label.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Domain key → alternative spellings to fold into it.
    #[serde(default)]
    pub aliases: BTreeMap<String, Vec<String>>,
    /// Domain key → path prefixes that belong to it regardless of voting.
    #[serde(default)]
    pub paths: BTreeMap<String, Vec<String>>,
    /// Domain key → table names that belong to it regardless of voting.
    #[serde(default)]
    pub tables: BTreeMap<String, Vec<String>>,
    /// Keys to drop entirely, for segments that survive the infra filter but
    /// still carry no business meaning in this codebase.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl DomainConfig {
    pub fn load(root: &std::path::Path) -> Result<Self> {
        let path = root.join(crate::store::CODEATLAS_DIR).join("domains.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)?;
        Ok(toml::from_str(&text)?)
    }

    /// Map a raw segment to its canonical domain key.
    ///
    /// Alias matching ignores separators, because the variants that actually
    /// occur differ only in where word boundaries were drawn: `groupBuying`,
    /// `groupbuying` and `group_buying` are one domain spelled three ways, and
    /// an operator should not have to enumerate all of them.
    fn canonical(&self, raw: &str) -> String {
        let snake = to_snake(raw);
        let flat = snake.replace('_', "");
        for (key, alts) in &self.aliases {
            let matches = flatten_eq(key, &flat)
                || alts.iter().any(|a| flatten_eq(a, &flat));
            if matches {
                return key.clone();
            }
        }
        snake
    }
}

/// Compare an alias spelling to an already-flattened segment.
fn flatten_eq(alias: &str, flat_segment: &str) -> bool {
    to_snake(alias).replace('_', "") == flat_segment
}

#[derive(Debug, Clone)]
pub struct DomainAssignment {
    pub key: String,
    pub files: BTreeSet<i64>,
    pub tables: BTreeSet<i64>,
    pub confidence: f64,
    /// Why this domain exists, for the rationale column.
    pub seeds: Vec<String>,
}

/// A file's identity as far as partitioning is concerned.
struct FileRow {
    id: i64,
    path: String,
    /// Package-path segments, lowercased and de-camelised.
    segments: Vec<String>,
}

fn load_files(store: &Store, project_id: i64) -> Result<Vec<FileRow>> {
    let mut stmt = store.conn.prepare(
        "SELECT id, path FROM files WHERE project_id = ?1 AND lang IN ('java', 'xml')",
    )?;
    let rows = stmt.query_map(params![project_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, path) = row?;
        if is_excluded(&path) {
            continue;
        }
        out.push(FileRow { id, segments: path_segments(&path), path });
    }
    Ok(out)
}

/// True for paths that hold no business behaviour.
fn is_excluded(path: &str) -> bool {
    let padded = format!("/{path}");
    EXCLUDED_PATHS.iter().any(|p| padded.contains(p))
}

/// Meaningful path segments of a source file, in order.
///
/// Everything up to and including `java`/`resources` is build layout, and the
/// filename itself is too specific to vote with.
fn path_segments(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let start = parts
        .iter()
        .rposition(|p| *p == "java" || *p == "resources")
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = parts.len().saturating_sub(1);
    parts[start.min(end)..end]
        .iter()
        .map(|s| to_snake(s))
        .filter(|s| !s.is_empty())
        .collect()
}

/// A segment present in this share of all files is the project's own namespace
/// (`com`, `yaoex`, `promotion`) rather than a domain within it. Detected from
/// the data because the namespace differs per project and hardcoding it would
/// make the tool non-portable — which is the whole point of the exercise.
///
/// Set high deliberately: one domain legitimately dominating a focused codebase
/// is common, so only near-universal segments are treated as namespace.
const NAMESPACE_SHARE: f64 = 0.85;

/// A segment appearing at the same path depth in nearly every file is namespace
/// regardless of count, which catches the leading `com/company/product` prefix
/// even in a single-domain project.
const NAMESPACE_PREFIX_SHARE: f64 = 0.9;

/// Segments that form the project's own namespace prefix.
///
/// A namespace segment occupies the same position in nearly every file's package
/// path. `coupon` may appear in most files of a coupon-centric project, but it
/// appears at varying depths; `com`/`yaoex`/`promotion` always lead.
fn namespace_segments(files: &[FileRow]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if files.is_empty() {
        return out;
    }
    let threshold = (files.len() as f64 * NAMESPACE_PREFIX_SHARE).ceil() as usize;
    let max_len = files.iter().map(|f| f.segments.len()).max().unwrap_or(0);
    for depth in 0..max_len {
        let mut at_depth: BTreeMap<&str, usize> = BTreeMap::new();
        for f in files {
            if let Some(seg) = f.segments.get(depth) {
                *at_depth.entry(seg.as_str()).or_default() += 1;
            }
        }
        match at_depth.iter().max_by_key(|(_, n)| **n) {
            Some((seg, n)) if *n >= threshold => {
                out.insert((*seg).to_string());
            }
            // The first depth without a dominant segment ends the common prefix.
            _ => break,
        }
    }
    out
}

/// Candidate domain keys: package segments that look like business vocabulary.
fn seed_candidates(files: &[FileRow], cfg: &DomainConfig) -> BTreeMap<String, usize> {
    let namespace = namespace_segments(files);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
        for seg in &f.segments {
            if INFRA_SEGMENTS.contains(&seg.as_str()) || namespace.contains(seg) {
                continue;
            }
            let key = cfg.canonical(seg);
            if cfg.exclude.contains(&key) || key.len() < 3 {
                continue;
            }
            *counts.entry(key).or_default() += 1;
        }
    }
    let ceiling = (files.len() as f64 * NAMESPACE_SHARE) as usize;
    counts.retain(|_, n| *n >= MIN_SEED_FILES && *n <= ceiling.max(MIN_SEED_FILES));
    fold_variants(counts)
}

/// Merge candidates that differ only in word boundaries or a plural.
///
/// The same domain is genuinely spelled several ways across a codebase this old:
/// `groupbuying`, `group_buying` and `groupBuy` are one thing, as are
/// `buytogether` and `buy_together`. Folding them automatically means an operator
/// only has to curate the cases the tool cannot see, not the obvious ones.
fn fold_variants(counts: BTreeMap<String, usize>) -> BTreeMap<String, usize> {
    let mut groups: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
    for (key, n) in counts {
        groups.entry(variant_shape(&key)).or_default().push((key, n));
    }

    let mut out = BTreeMap::new();
    for (_, mut members) in groups {
        // The most-used spelling wins, ties broken by name for determinism.
        members.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let total: usize = members.iter().map(|(_, n)| *n).sum();
        out.insert(members[0].0.clone(), total);
    }
    out
}

/// Group table names by their leading tokens, e.g. `t_coupon_template` → `coupon`.
fn table_groups(
    names: &[(i64, String)],
    seeds: &BTreeMap<String, usize>,
    cfg: &DomainConfig,
) -> BTreeMap<i64, String> {
    let variants = variant_index(seeds);
    let lookup = |tokens: &[&str]| -> Option<String> {
        let cand = cfg.canonical(&tokens.join("_"));
        // A table's wording rarely matches a package's exactly: `t_group_buying_*`
        // must still reach a `groupBuy` package.
        seeds
            .contains_key(&cand)
            .then(|| cand.clone())
            .or_else(|| variants.get(&variant_shape(&cand)).cloned())
    };

    let mut out = BTreeMap::new();
    for (id, name) in names {
        let bare = name.strip_prefix(TABLE_PREFIX).unwrap_or(name);
        let tokens: Vec<&str> = bare.split('_').collect();

        // Scan every contiguous run of tokens, longest and leftmost first. The
        // domain word is not always at the front: `t_promotion_defective_area`
        // names a `defective` table under a generic `promotion_` prefix, and
        // matching only on leading tokens leaves the majority of tables orphaned.
        let mut best: Option<(usize, usize, String)> = None;
        for len in (1..=tokens.len().min(3)).rev() {
            for start in 0..=tokens.len().saturating_sub(len) {
                if let Some(key) = lookup(&tokens[start..start + len]) {
                    // Prefer the longer match; among equals, the earlier one.
                    let better = best
                        .as_ref()
                        .is_none_or(|(l, s, _)| len > *l || (len == *l && start < *s));
                    if better {
                        best = Some((len, start, key));
                    }
                }
            }
            if best.is_some() {
                break;
            }
        }
        if let Some((_, _, key)) = best {
            out.insert(*id, key);
        }
    }
    out
}

/// Separator-free, de-suffixed form shared by spelling variants of one word.
fn variant_shape(key: &str) -> String {
    let flat = key.replace('_', "");
    // `buying` and `buy` share a stem; so do `coupons` and `coupon`.
    for suffix in ["ing", "es", "s"] {
        if let Some(stem) = flat.strip_suffix(suffix) {
            if stem.len() >= 4 {
                return stem.to_string();
            }
        }
    }
    flat
}

/// Variant shape → the seed key that survived folding.
fn variant_index(seeds: &BTreeMap<String, usize>) -> BTreeMap<String, String> {
    seeds.keys().map(|k| (variant_shape(k), k.clone())).collect()
}

/// Per-file vote tallies, one map of domain key → score.
type Votes = BTreeMap<i64, BTreeMap<String, f64>>;

fn add_vote(votes: &mut Votes, file: i64, key: &str, weight: f64) {
    *votes.entry(file).or_default().entry(key.to_string()).or_default() += weight;
}

/// The highest-scoring domain for a file, and its share of the total vote.
fn winner(scores: &BTreeMap<String, f64>) -> Option<(String, f64)> {
    let total: f64 = scores.values().sum();
    if total <= 0.0 {
        return None;
    }
    scores
        .iter()
        // Break ties on the key so a rebuild is deterministic.
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap().then(b.0.cmp(a.0)))
        .map(|(k, v)| (k.clone(), v / total))
}

/// Partition the project into domains and persist the result.
pub fn assign(store: &Store, project_id: i64, cfg: &DomainConfig) -> Result<Vec<DomainAssignment>> {
    let files = load_files(store, project_id)?;
    let seeds = seed_candidates(&files, cfg);
    if seeds.is_empty() {
        return Ok(Vec::new());
    }

    let tables: Vec<(i64, String)> = store
        .conn
        .prepare("SELECT id, name FROM tables WHERE project_id = ?1")?
        .query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let table_domain = table_groups(&tables, &seeds, cfg);

    // Variant spelling → surviving seed key, so votes for `groupbuying` and
    // `group_buy` land on the same domain instead of splitting it.
    let variant_map = variant_index(&seeds);
    let resolve_key = |seg: &str| -> Option<String> {
        let key = cfg.canonical(seg);
        if seeds.contains_key(&key) {
            return Some(key);
        }
        variant_map.get(&variant_shape(&key)).cloned()
    };

    let mut votes: Votes = BTreeMap::new();

    // 1. Package placement. Later segments are more specific, so they vote harder.
    for f in &files {
        let n = f.segments.len() as f64;
        for (i, seg) in f.segments.iter().enumerate() {
            let Some(key) = resolve_key(seg) else { continue };
            let specificity = (i as f64 + 1.0) / n.max(1.0);
            add_vote(&mut votes, f.id, &key, W_PACKAGE * specificity);
        }
    }

    // 2. Tables the file accesses.
    let access: Vec<(i64, i64)> = store
        .conn
        .prepare(
            "SELECT DISTINCT file_id, table_id FROM table_access
             WHERE project_id = ?1 AND file_id IS NOT NULL",
        )?
        .query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (file_id, table_id) in &access {
        if let Some(key) = table_domain.get(table_id) {
            add_vote(&mut votes, *file_id, key, W_TABLE);
        }
    }

    // 3 & 4. Neighbours: files that change together, and files that call each
    // other. Both are applied from a frozen snapshot each round, so influence
    // spreads one hop at a time instead of cascading within a round.
    let churn = churn_pairs(store, project_id)?;
    let calls = call_pairs(store, project_id)?;
    for _ in 0..PROPAGATION_ROUNDS {
        let snapshot: BTreeMap<i64, (String, f64)> = votes
            .iter()
            .filter_map(|(id, s)| winner(s).map(|w| (*id, w)))
            .collect();
        for (a, b, weight) in &churn {
            propagate(&mut votes, &snapshot, *a, *b, W_CHURN * weight);
        }
        for (a, b, weight) in &calls {
            propagate(&mut votes, &snapshot, *a, *b, W_CALL * weight);
        }
    }

    // Curated paths and tables override the vote entirely.
    for (key, prefixes) in &cfg.paths {
        for f in &files {
            if prefixes.iter().any(|p| f.path.starts_with(p.as_str())) {
                votes.entry(f.id).or_default().clear();
                add_vote(&mut votes, f.id, key, 1000.0);
            }
        }
    }

    let result = persist(store, project_id, cfg, &files, &votes, &tables, &table_domain, &seeds)?;
    relink_l2(store, project_id)?;
    Ok(result)
}

/// Re-point L2 rows at the domain rows this build just created.
///
/// `build` deletes and recreates `domains`, so any row id stored by a previous
/// `deepen` is stale — and row ids get reused, which is worse than dangling:
/// a dossier would silently attach to an unrelated domain. Matching on the domain
/// key instead means expensive model output survives an L0 refresh, and anything
/// whose domain genuinely disappeared is marked stale rather than shown as fact.
fn relink_l2(store: &Store, project_id: i64) -> Result<()> {
    let tx = store.conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE notes SET subject_id = (
             SELECT d.id FROM domains d
             WHERE d.project_id = notes.project_id AND d.key = notes.subject_key)
         WHERE project_id = ?1 AND subject_kind = 'domain' AND subject_key IS NOT NULL",
        params![project_id],
    )?;
    tx.execute(
        "UPDATE glossary SET domain_id = (
             SELECT d.id FROM domains d
             WHERE d.project_id = glossary.project_id AND d.key = glossary.domain_key)
         WHERE project_id = ?1 AND domain_key IS NOT NULL",
        params![project_id],
    )?;
    // A domain that no longer exists leaves its notes unanchored; flag them for
    // review instead of letting them render as current.
    tx.execute(
        "UPDATE notes SET status = 'stale'
         WHERE project_id = ?1 AND subject_kind = 'domain' AND subject_id IS NULL",
        params![project_id],
    )?;
    tx.commit()?;
    Ok(())
}

/// Propagate one neighbour's winning domain onto the other, both directions.
fn propagate(
    votes: &mut Votes,
    snapshot: &BTreeMap<i64, (String, f64)>,
    a: i64,
    b: i64,
    weight: f64,
) {
    if let Some((key, share)) = snapshot.get(&b) {
        add_vote(votes, a, key, weight * share);
    }
    if let Some((key, share)) = snapshot.get(&a) {
        add_vote(votes, b, key, weight * share);
    }
}

/// File pairs that change together, weighted by how often.
///
/// Commits touching many files are mostly merges and sweeping renames; they
/// couple everything to everything, so their influence is damped by size.
fn churn_pairs(store: &Store, project_id: i64) -> Result<Vec<(i64, i64, f64)>> {
    let mut by_commit: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let mut stmt = store.conn.prepare(
        "SELECT t.commit_id, t.file_id FROM git_touches t
         JOIN git_commits c ON c.id = t.commit_id
         WHERE c.project_id = ?1",
    )?;
    let rows = stmt.query_map(params![project_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (c, f) = row?;
        by_commit.entry(c).or_default().push(f);
    }

    let mut pairs: BTreeMap<(i64, i64), f64> = BTreeMap::new();
    for files in by_commit.values() {
        // A commit touching this many files says nothing about cohesion.
        if files.len() < 2 || files.len() > 20 {
            continue;
        }
        let w = 1.0 / (files.len() as f64 - 1.0);
        for (i, a) in files.iter().enumerate() {
            for b in &files[i + 1..] {
                let key = if a < b { (*a, *b) } else { (*b, *a) };
                *pairs.entry(key).or_default() += w;
            }
        }
    }
    // Normalise so a single shared commit cannot outvote package placement.
    let max = pairs.values().copied().fold(0.0f64, f64::max).max(1.0);
    Ok(pairs.into_iter().map(|((a, b), w)| (a, b, w / max)).collect())
}

/// File pairs connected by resolved call edges, weighted by edge count.
fn call_pairs(store: &Store, project_id: i64) -> Result<Vec<(i64, i64, f64)>> {
    let mut stmt = store.conn.prepare(
        "SELECT s1.file_id, s2.file_id, COUNT(*) FROM refs r
         JOIN symbols s1 ON s1.id = r.src_symbol_id
         JOIN symbols s2 ON s2.id = r.dst_symbol_id
         WHERE r.project_id = ?1 AND r.resolved = 1 AND r.confidence >= 0.6
           AND s1.file_id <> s2.file_id
         GROUP BY 1, 2",
    )?;
    let rows = stmt.query_map(params![project_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
    })?;
    let mut out = Vec::new();
    let mut max = 1.0f64;
    for row in rows {
        let (a, b, n) = row?;
        let w = n as f64;
        max = max.max(w);
        out.push((a, b, w));
    }
    for e in &mut out {
        e.2 /= max;
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn persist(
    store: &Store,
    project_id: i64,
    cfg: &DomainConfig,
    files: &[FileRow],
    votes: &Votes,
    tables: &[(i64, String)],
    table_domain: &BTreeMap<i64, String>,
    seeds: &BTreeMap<String, usize>,
) -> Result<Vec<DomainAssignment>> {
    let mut assignments: BTreeMap<String, DomainAssignment> = BTreeMap::new();
    let mut shares: BTreeMap<String, Vec<f64>> = BTreeMap::new();

    for f in files {
        let Some(scores) = votes.get(&f.id) else { continue };
        let Some((key, share)) = winner(scores) else { continue };
        assignments
            .entry(key.clone())
            .or_insert_with(|| DomainAssignment {
                key: key.clone(),
                files: BTreeSet::new(),
                tables: BTreeSet::new(),
                confidence: 0.0,
                seeds: Vec::new(),
            })
            .files
            .insert(f.id);
        shares.entry(key).or_default().push(share);
    }

    // Curated table overrides win; otherwise the prefix grouping decides.
    let mut curated_tables: BTreeMap<String, String> = BTreeMap::new();
    for (key, names) in &cfg.tables {
        for n in names {
            curated_tables.insert(n.to_ascii_lowercase(), key.clone());
        }
    }
    // Which domain's files actually touch each table. This catches the tables
    // whose names carry no domain word — `t_promotion_rule`, `t_cust_group` —
    // where usage is the only evidence of ownership available.
    let by_usage = tables_by_usage(store, project_id, &assignments)?;

    for (table_id, name) in tables {
        let key = curated_tables
            .get(&name.to_ascii_lowercase())
            .or_else(|| table_domain.get(table_id))
            .or_else(|| by_usage.get(table_id));
        if let Some(key) = key {
            if let Some(a) = assignments.get_mut(key) {
                a.tables.insert(*table_id);
            }
        }
    }

    for (key, a) in &mut assignments {
        let s = shares.get(key).map(|v| v.as_slice()).unwrap_or(&[]);
        // Mean vote share of the domain's own files: how cleanly it separates.
        a.confidence = if s.is_empty() {
            0.0
        } else {
            s.iter().sum::<f64>() / s.len() as f64
        };
        a.seeds = vec![format!("package segment `{key}` ({} files)", seeds.get(key).copied().unwrap_or(0))];
    }

    let tx = store.conn.unchecked_transaction()?;
    {
        let mut ins_dom = tx.prepare(
            "INSERT INTO domains(project_id, key, label, confidence, rationale_json, curated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut ins_mem = tx.prepare(
            "INSERT OR IGNORE INTO domain_members(domain_id, kind, ref_id, weight)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for a in assignments.values() {
            let curated = cfg.paths.contains_key(&a.key)
                || cfg.tables.contains_key(&a.key)
                || cfg.labels.contains_key(&a.key);
            ins_dom.execute(params![
                project_id,
                a.key,
                cfg.labels.get(&a.key),
                a.confidence,
                serde_json::to_string(&a.seeds)?,
                curated as i64,
            ])?;
            let domain_id = tx.last_insert_rowid();
            for f in &a.files {
                ins_mem.execute(params![domain_id, "file", f, 1.0])?;
            }
            for t in &a.tables {
                ins_mem.execute(params![domain_id, "table", t, 1.0])?;
            }
        }
    }
    tx.commit()?;

    link_entrypoints(store, project_id)?;
    compute_edges(store, project_id)?;
    Ok(assignments.into_values().collect())
}

/// Share of a table's accesses that must come from one domain before usage is
/// accepted as evidence of ownership. A table read by everyone — a config or
/// lookup table — is deliberately left unassigned rather than attributed to
/// whichever domain happens to query it most.
const USAGE_OWNERSHIP_SHARE: f64 = 0.7;

/// Table → owning domain, inferred from which domain's files access it.
fn tables_by_usage(
    store: &Store,
    project_id: i64,
    assignments: &BTreeMap<String, DomainAssignment>,
) -> Result<BTreeMap<i64, String>> {
    // File id → domain key, from the assignments just computed.
    let mut file_domain: BTreeMap<i64, &str> = BTreeMap::new();
    for (key, a) in assignments {
        for f in &a.files {
            file_domain.insert(*f, key.as_str());
        }
    }

    let mut counts: BTreeMap<i64, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut stmt = store.conn.prepare(
        "SELECT DISTINCT table_id, file_id FROM table_access
         WHERE project_id = ?1 AND file_id IS NOT NULL",
    )?;
    for row in stmt.query_map(params![project_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
    })? {
        let (table_id, file_id) = row?;
        if let Some(key) = file_domain.get(&file_id) {
            *counts.entry(table_id).or_default().entry(*key).or_default() += 1;
        }
    }

    let mut out = BTreeMap::new();
    for (table_id, per_domain) in counts {
        let total: usize = per_domain.values().sum();
        if total == 0 {
            continue;
        }
        // Deterministic on ties, same rule as `winner`.
        if let Some((key, n)) = per_domain
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        {
            if *n as f64 / total as f64 >= USAGE_OWNERSHIP_SHARE {
                out.insert(table_id, (*key).to_string());
            }
        }
    }
    Ok(out)
}

/// Attach entrypoints to the domain of the file that declares them.
fn link_entrypoints(store: &Store, project_id: i64) -> Result<()> {
    store.conn.execute(
        "INSERT OR IGNORE INTO domain_members(domain_id, kind, ref_id, weight)
         SELECT dm.domain_id, 'entrypoint', e.id, 1.0
         FROM entrypoints e
         JOIN domain_members dm ON dm.kind = 'file' AND dm.ref_id = e.file_id
         WHERE e.project_id = ?1 AND e.file_id IS NOT NULL",
        params![project_id],
    )?;
    Ok(())
}

/// Cross-domain coupling, counted over resolved call edges between files that
/// landed in different domains.
fn compute_edges(store: &Store, project_id: i64) -> Result<()> {
    store.conn.execute(
        "INSERT OR REPLACE INTO domain_edges(src_domain_id, dst_domain_id, kind, weight, evidence_json)
         SELECT d1.domain_id, d2.domain_id, 'call', COUNT(*),
                json_object('edges', COUNT(*))
         FROM refs r
         JOIN symbols s1 ON s1.id = r.src_symbol_id
         JOIN symbols s2 ON s2.id = r.dst_symbol_id
         JOIN domain_members d1 ON d1.kind = 'file' AND d1.ref_id = s1.file_id
         JOIN domain_members d2 ON d2.kind = 'file' AND d2.ref_id = s2.file_id
         WHERE r.project_id = ?1 AND r.resolved = 1 AND r.confidence >= 0.6
           AND d1.domain_id <> d2.domain_id
         GROUP BY d1.domain_id, d2.domain_id",
        params![project_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_drop_build_layout_and_filename() {
        assert_eq!(
            path_segments("promotion-service/src/main/java/com/yaoex/promotion/service/impl/coupon/CouponServiceImpl.java"),
            vec!["com", "yaoex", "promotion", "service", "impl", "coupon"]
        );
        assert_eq!(
            path_segments("promotion-model/src/main/resources/com/yaoex/promotion/model/defective/DefectiveDao.xml"),
            vec!["com", "yaoex", "promotion", "model", "defective"]
        );
    }

    #[test]
    fn camel_package_segments_normalize() {
        assert_eq!(
            path_segments("a/src/main/java/p/buyTogether/X.java"),
            vec!["p", "buy_together"]
        );
    }

    #[test]
    fn excludes_tests_and_web_assets() {
        assert!(is_excluded("promotion-service/src/test/groovy/spock/support/X.java"));
        assert!(is_excluded("promotion-web/src/main/webapp/WEB-INF/web.xml"));
        assert!(is_excluded("promotion-web/src/main/webapp/crossdomain.xml"));
        assert!(!is_excluded("promotion-service/src/main/java/p/coupon/A.java"));
    }

    #[test]
    fn project_namespace_segments_are_not_domains() {
        // Every file sits under `com/yaoex/promotion`, so those segments describe
        // the project, not a domain inside it. Only `coupon` and `draw` do.
        let paths: Vec<String> = (0..10)
            .map(|i| format!("m/src/main/java/com/yaoex/promotion/coupon/A{i}.java"))
            .chain((0..6).map(|i| {
                format!("m/src/main/java/com/yaoex/promotion/draw/B{i}.java")
            }))
            .collect();
        let files: Vec<FileRow> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| FileRow {
                id: i as i64,
                path: p.clone(),
                segments: path_segments(p),
            })
            .collect();

        let seeds = seed_candidates(&files, &DomainConfig::default());
        assert!(seeds.contains_key("coupon"));
        assert!(seeds.contains_key("draw"));
        assert!(!seeds.contains_key("promotion"), "namespace leaked in: {seeds:?}");
        assert!(!seeds.contains_key("yaoex"));
        assert!(!seeds.contains_key("com"));
    }

    #[test]
    fn response_shape_suffixes_are_not_domains() {
        // `bidding` must coexist with another domain, otherwise it is this
        // fixture's namespace rather than a domain within it.
        let paths: Vec<String> = (0..5)
            .map(|i| format!("m/src/main/java/p/bidding/rsp/R{i}.java"))
            .chain((0..5).map(|i| format!("m/src/main/java/p/coupon/C{i}.java")))
            .collect();
        let files: Vec<FileRow> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| FileRow {
                id: i as i64,
                path: p.clone(),
                segments: path_segments(p),
            })
            .collect();
        let seeds = seed_candidates(&files, &DomainConfig::default());
        assert!(!seeds.contains_key("rsp"), "shape suffix leaked in: {seeds:?}");
        assert!(seeds.contains_key("bidding"));
        assert!(seeds.contains_key("coupon"));
    }

    #[test]
    fn seeds_exclude_infra_segments_and_rare_ones() {
        let files: Vec<FileRow> = [
            "a/src/main/java/p/coupon/A.java",
            "a/src/main/java/p/coupon/B.java",
            "a/src/main/java/p/coupon/C.java",
            "a/src/main/java/p/util/D.java",
            "a/src/main/java/p/util/E.java",
            "a/src/main/java/p/util/F.java",
            "a/src/main/java/p/rare/G.java",
        ]
        .iter()
        .enumerate()
        .map(|(i, p)| FileRow {
            id: i as i64,
            path: p.to_string(),
            segments: path_segments(p),
        })
        .collect();

        let seeds = seed_candidates(&files, &DomainConfig::default());
        assert!(seeds.contains_key("coupon"));
        // Infrastructure segment, despite meeting the file threshold.
        assert!(!seeds.contains_key("util"));
        // Below the threshold.
        assert!(!seeds.contains_key("rare"));
    }

    #[test]
    fn aliases_fold_spelling_variants() {
        let mut cfg = DomainConfig::default();
        // One alias entry must cover every casing and separator variant, since
        // this project spells the same domain `groupbuying`, `groupBuying` and
        // `groupBuy` in different packages.
        cfg.aliases.insert("group_buy".into(), vec!["groupbuying".into()]);
        assert_eq!(cfg.canonical("groupBuying"), "group_buy");
        assert_eq!(cfg.canonical("groupbuying"), "group_buy");
        assert_eq!(cfg.canonical("group_buying"), "group_buy");
        assert_eq!(cfg.canonical("groupBuy"), "group_buy");
        assert_eq!(cfg.canonical("coupon"), "coupon");
    }

    #[test]
    fn canonical_key_itself_matches_its_variants() {
        let mut cfg = DomainConfig::default();
        cfg.aliases.insert("buy_together".into(), Vec::new());
        assert_eq!(cfg.canonical("buyTogether"), "buy_together");
    }

    #[test]
    fn spelling_variants_fold_into_one_domain() {
        let counts = BTreeMap::from([
            ("groupbuying".to_string(), 42usize),
            ("group_buy".to_string(), 18),
            ("buy_together".to_string(), 48),
            ("buytogether".to_string(), 4),
            ("coupon".to_string(), 230),
        ]);
        let folded = fold_variants(counts);
        // The dominant spelling survives and absorbs the others' counts.
        assert_eq!(folded.get("groupbuying"), Some(&60));
        assert_eq!(folded.get("buy_together"), Some(&52));
        assert_eq!(folded.get("coupon"), Some(&230));
        assert!(!folded.contains_key("group_buy"));
        assert!(!folded.contains_key("buytogether"));
    }

    #[test]
    fn variant_shape_ignores_separators_and_suffixes() {
        // Inputs are already snake_case keys, as produced by `to_snake`.
        assert_eq!(variant_shape("group_buying"), variant_shape("groupbuying"));
        assert_eq!(variant_shape("group_buying"), variant_shape(&to_snake("groupBuy")));
        assert_eq!(variant_shape("buy_together"), variant_shape("buytogether"));
        // Short words keep their suffix: `maps` must not collapse to `map`.
        assert_ne!(variant_shape("coupon"), variant_shape("draw"));
    }

    #[test]
    fn transport_segments_are_not_domains() {
        let paths: Vec<String> = (0..20)
            .map(|i| format!("m/src/main/java/p/dubbo/service/bean/D{i}.java"))
            .chain((0..8).map(|i| format!("m/src/main/java/p/coupon/C{i}.java")))
            .collect();
        let files: Vec<FileRow> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| FileRow {
                id: i as i64,
                path: p.clone(),
                segments: path_segments(p),
            })
            .collect();
        let seeds = seed_candidates(&files, &DomainConfig::default());
        assert!(!seeds.contains_key("dubbo"), "transport leaked in: {seeds:?}");
        assert!(seeds.contains_key("coupon"));
    }

    #[test]
    fn table_prefix_grouping_prefers_longest_match() {
        let mut seeds = BTreeMap::new();
        seeds.insert("group".to_string(), 5usize);
        seeds.insert("group_buying".to_string(), 5usize);
        seeds.insert("coupon".to_string(), 5usize);
        let names = vec![
            (1i64, "t_group_buying_product".to_string()),
            (2, "t_coupon_template".to_string()),
            (3, "t_unrelated_thing".to_string()),
        ];
        let g = table_groups(&names, &seeds, &DomainConfig::default());
        assert_eq!(g.get(&1).unwrap(), "group_buying");
        assert_eq!(g.get(&2).unwrap(), "coupon");
        assert!(!g.contains_key(&3));
    }

    #[test]
    fn domain_word_is_found_past_a_generic_prefix() {
        // `t_promotion_defective_*` is a `defective` table wearing a generic
        // `promotion_` prefix; matching only leading tokens would orphan it.
        let seeds = BTreeMap::from([
            ("defective".to_string(), 96usize),
            ("coupon".to_string(), 305),
        ]);
        let names = vec![
            (1i64, "t_promotion_defective".to_string()),
            (2, "t_promotion_defective_black_user".to_string()),
            (3, "t_activity_coupon_template".to_string()),
        ];
        let g = table_groups(&names, &seeds, &DomainConfig::default());
        assert_eq!(g.get(&1).unwrap(), "defective");
        assert_eq!(g.get(&2).unwrap(), "defective");
        assert_eq!(g.get(&3).unwrap(), "coupon");
    }

    #[test]
    fn earlier_match_wins_among_equal_lengths() {
        // Both words are seeds; the one nearer the front names the table.
        let seeds = BTreeMap::from([("coupon".to_string(), 9usize), ("product".to_string(), 9)]);
        let names = vec![(1i64, "t_coupon_product_rel".to_string())];
        let g = table_groups(&names, &seeds, &DomainConfig::default());
        assert_eq!(g.get(&1).unwrap(), "coupon");
    }

    #[test]
    fn winner_is_deterministic_on_ties() {
        let mut s = BTreeMap::new();
        s.insert("alpha".to_string(), 1.0);
        s.insert("beta".to_string(), 1.0);
        assert_eq!(winner(&s).unwrap().0, "alpha");
        assert!(winner(&BTreeMap::new()).is_none());
    }

    #[test]
    fn winner_reports_vote_share() {
        let mut s = BTreeMap::new();
        s.insert("a".to_string(), 3.0);
        s.insert("b".to_string(), 1.0);
        let (key, share) = winner(&s).unwrap();
        assert_eq!(key, "a");
        assert!((share - 0.75).abs() < 1e-9);
    }
}
