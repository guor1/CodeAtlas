//! L0 build pipeline: walk the project, run every probe, persist the evidence.

mod java_pass;
mod xml_pass;

use crate::extract::{git, maven, walk};
use crate::search;
use crate::structure::{domain, trace};
use crate::store::Store;
use crate::util;
use anyhow::Result;
use rusqlite::params;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct BuildStats {
    pub modules: usize,
    pub files: usize,
    pub symbols: usize,
    pub refs: usize,
    pub refs_resolved: usize,
    pub tables: usize,
    pub columns: usize,
    pub table_access: usize,
    pub entrypoints: usize,
    pub config_props: usize,
    pub commits: usize,
    pub parse_errors: usize,
    pub domains: usize,
    pub traces: usize,
}

/// How much git history to mine for vocabulary. Subjects saturate well before
/// this on the repositories we target, and a full walk of a decade-old history
/// costs minutes for no extra signal.
const GIT_LOG_LIMIT: usize = 4000;

/// The two git signals the pipeline needs, with a `HEAD`-keyed cache in front.
///
/// Git history is immutable once `HEAD` stops moving, so the expensive walks
/// ([`git::log`], [`git::file_history`]) are replayed from `git_cache` when the
/// head is unchanged and only re-run when a commit lands. `git_branches` is
/// deliberately *not* cached — remote branch pointers move on `git fetch` without
/// `HEAD` changing, and it is a single fast command anyway.
fn git_signals(
    store: &Store,
    root: &Path,
) -> (
    BTreeMap<String, git::FileHistory>,
    Vec<git::Commit>,
) {
    let empty = (BTreeMap::new(), Vec::new());
    if !git::is_repo(root) {
        return empty;
    }
    let Some(head) = git::head(root) else { return empty };
    let limit = GIT_LOG_LIMIT as i64;
    if let Ok(Some((hist_json, commits_json))) = store.cached_git(&head, limit) {
        if let (Ok(hist), Ok(commits)) = (
            serde_json::from_str::<BTreeMap<String, git::FileHistory>>(&hist_json),
            serde_json::from_str::<Vec<git::Commit>>(&commits_json),
        ) {
            return (hist, commits);
        }
    }
    let hist = git::file_history(root).unwrap_or_default();
    let commits = git::log(root, GIT_LOG_LIMIT).unwrap_or_default();
    // Best effort: a cache write that fails must not fail the build.
    if let (Ok(hj), Ok(cj)) = (serde_json::to_string(&hist), serde_json::to_string(&commits)) {
        if store.cache_git(&head, limit, &hj, &cj).is_ok() {
            let _ = store.prune_git_cache(&head);
        }
    }
    (hist, commits)
}

pub fn run(store: &Store, root: &Path) -> Result<BuildStats> {
    // A full build re-parses everything and its parse results are authoritative;
    // start the cache over so it cannot carry a parse from an older grammar.
    store.reset_parse_cache()?;
    pipeline(store, root)
}

/// The build pipeline, shared by `build` (cache reset first) and `sync` (cache
/// retained, so unchanged files are never re-parsed).
pub fn pipeline(store: &Store, root: &Path) -> Result<BuildStats> {
    let mut stats = BuildStats::default();
    let project_id = store.project_id()?;

    let run_id = start_run(store, project_id, "build")?;
    store.reset_layers(project_id)?;

    let files = walk::scan(root);
    let modules = index_modules(store, project_id, root, &files)?;
    stats.modules = modules.len();

    let (histories, commits) = git_signals(store, root);

    let file_ids = index_files(store, project_id, root, &files, &modules, &histories)?;
    stats.files = file_ids.len();

    let java = java_pass::run(store, project_id, root, &files, &file_ids, &mut stats)?;
    xml_pass::run(store, project_id, root, &files, &file_ids, &java, &mut stats)?;

    if git::is_repo(root) {
        index_git(store, project_id, root, &file_ids, &commits, &mut stats)?;
    }

    // L1 runs in the same pass: it is pure computation over what L0 just wrote,
    // so there is no reason to make the operator ask for it separately.
    let cfg = domain::DomainConfig::load(root)?;
    stats.domains = domain::assign(store, project_id, &cfg)?.len();
    stats.traces = trace::build(store, project_id)?;
    // Search index is a derived view too; rebuild it here so `query` works
    // immediately after every build without a separate step.
    search::rebuild(store, project_id)?;

    finish_run(store, run_id, &stats)?;
    store.record_tool_version(project_id, *crate::VERSION_MINOR)?;
    Ok(stats)
}

fn start_run(store: &Store, project_id: i64, kind: &str) -> Result<i64> {
    store.conn.execute(
        "INSERT INTO build_runs(project_id, kind, started_at, tool_version)
         VALUES (?1, ?2, ?3, ?4)",
        params![project_id, kind, util::now_iso(), env!("CARGO_PKG_VERSION")],
    )?;
    Ok(store.conn.last_insert_rowid())
}

fn finish_run(store: &Store, run_id: i64, stats: &BuildStats) -> Result<()> {
    store.conn.execute(
        "UPDATE build_runs SET finished_at = ?1, stats_json = ?2 WHERE id = ?3",
        params![util::now_iso(), serde_json::to_string(stats)?, run_id],
    )?;
    Ok(())
}

/// Register Maven modules, deriving the tree from each POM's `<modules>`.
fn index_modules(
    store: &Store,
    project_id: i64,
    root: &Path,
    files: &[walk::Found],
) -> Result<BTreeMap<String, i64>> {
    let poms: Vec<&walk::Found> = files
        .iter()
        .filter(|f| f.rel == "pom.xml" || f.rel.ends_with("/pom.xml"))
        .collect();

    // Insert parents before children so `parent_id` can be filled in one pass.
    let mut ordered: Vec<(&walk::Found, maven::Pom)> = Vec::new();
    for f in poms {
        let text = std::fs::read_to_string(&f.path).unwrap_or_default();
        if let Ok(pom) = maven::parse(&text) {
            ordered.push((f, pom));
        }
    }
    ordered.sort_by_key(|(f, _)| f.rel.matches('/').count());

    let tx = store.conn.unchecked_transaction()?;
    let mut ids: BTreeMap<String, i64> = BTreeMap::new();
    for (f, pom) in &ordered {
        // Module path is the POM's directory, relative to the root.
        let dir = f.rel.strip_suffix("pom.xml").unwrap_or("").trim_end_matches('/');
        let dir = if dir.is_empty() { ".".to_string() } else { dir.to_string() };
        let name = pom
            .artifact_id
            .clone()
            .unwrap_or_else(|| Path::new(&dir).file_name().map_or(dir.clone(), |n| n.to_string_lossy().into()));
        let parent_dir = parent_module_dir(&dir, &ids);
        let parent_id = parent_dir.and_then(|d| ids.get(&d).copied());
        tx.execute(
            "INSERT OR IGNORE INTO modules(project_id, name, path, packaging, parent_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![project_id, name, dir, pom.packaging, parent_id],
        )?;
        let id: i64 = tx.query_row(
            "SELECT id FROM modules WHERE project_id = ?1 AND path = ?2",
            params![project_id, dir],
            |r| r.get(0),
        )?;
        ids.insert(dir, id);
    }
    tx.commit()?;
    let _ = root;
    Ok(ids)
}

/// The nearest already-registered ancestor directory of `dir`.
fn parent_module_dir(dir: &str, known: &BTreeMap<String, i64>) -> Option<String> {
    if dir == "." {
        return None;
    }
    let mut cur = Path::new(dir).parent();
    while let Some(p) = cur {
        let key = if p.as_os_str().is_empty() {
            ".".to_string()
        } else {
            p.to_string_lossy().replace('\\', "/")
        };
        if known.contains_key(&key) {
            return Some(key);
        }
        if key == "." {
            return None;
        }
        cur = p.parent();
    }
    None
}

fn index_files(
    store: &Store,
    project_id: i64,
    root: &Path,
    files: &[walk::Found],
    modules: &BTreeMap<String, i64>,
    histories: &BTreeMap<String, git::FileHistory>,
) -> Result<BTreeMap<String, i64>> {
    store.index_files(project_id, root, files, modules, histories)
}

fn index_git(
    store: &Store,
    project_id: i64,
    root: &Path,
    file_ids: &BTreeMap<String, i64>,
    commits: &[git::Commit],
    stats: &mut BuildStats,
) -> Result<()> {
    let branches = git::branches(root).unwrap_or_default();

    let tx = store.conn.unchecked_transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT OR IGNORE INTO git_commits(project_id, sha, authored_at, subject)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut touch = tx.prepare(
            "INSERT OR IGNORE INTO git_touches(commit_id, file_id) VALUES (?1, ?2)",
        )?;
        for c in commits {
            ins.execute(params![project_id, c.sha, c.authored_at, c.subject])?;
            let commit_id = tx.last_insert_rowid();
            for f in &c.files {
                // Paths of files deleted since the commit have no row; skip them.
                if let Some(fid) = file_ids.get(f) {
                    touch.execute(params![commit_id, fid])?;
                }
            }
        }
        let mut br = tx.prepare(
            "INSERT OR IGNORE INTO git_branches(project_id, name) VALUES (?1, ?2)",
        )?;
        for b in &branches {
            br.execute(params![project_id, b])?;
        }
    }
    tx.commit()?;
    stats.commits = commits.len();
    Ok(())
}
