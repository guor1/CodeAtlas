//! Truth layer: SQLite storage for L0 evidence, L1 structure, L2 narrative.

pub mod model;
pub mod schema;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

use crate::extract::{git, walk};

pub const CODEATLAS_DIR: &str = ".codeatlas";
pub const DB_FILE: &str = "codeatlas.db";

pub struct Store {
    pub conn: Connection,
    pub root: PathBuf,
}

impl Store {
    /// Open (creating if needed) the database under `<root>/.codeatlas/codeatlas.db`.
    pub fn open(root: &Path) -> Result<Self> {
        let dir = root.join(CODEATLAS_DIR);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let db_path = dir.join(DB_FILE);
        let conn = Connection::open(&db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let store = Self { conn, root: root.to_path_buf() };
        store.migrate()?;
        Ok(store)
    }

    /// Find the project root owning a knowledge base, starting at `start` and
    /// walking up through its ancestors.
    ///
    /// Works the way `git` finds `.git/`, and for the same reason: a command is
    /// often run from a subdirectory. It matters most for `catlas mcp`, whose
    /// working directory is chosen by the MCP client (Claude Code inherits the
    /// directory it was launched from, not necessarily the project root), which
    /// is why the server can be registered without an explicit `--path`.
    pub fn find_root(start: &Path) -> Result<PathBuf> {
        for dir in start.ancestors() {
            if dir.join(CODEATLAS_DIR).join(DB_FILE).exists() {
                return Ok(dir.to_path_buf());
            }
        }
        anyhow::bail!(
            "在 {} 及其上级目录中找不到 {CODEATLAS_DIR}/{DB_FILE} — 先运行 `catlas init`",
            start.display()
        )
    }

    /// Open an existing database, failing if it has not been initialized.
    ///
    /// `root` may be any directory inside the project; the store's own `root`
    /// is the ancestor that actually holds `.codeatlas/`.
    pub fn open_existing(root: &Path) -> Result<Self> {
        Self::open(&Self::find_root(root)?)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(schema::DDL).context("applying schema")?;
        // The FTS table is recreated every open: it is a derived index, and this
        // is what lets an older database (whose table used a different tokenizer
        // or column set) be upgraded in place rather than erroring on `CREATE …
        // IF NOT EXISTS` against an incompatible existing table.
        self.conn.execute_batch("DROP TABLE IF EXISTS search_fts")?;
        self.conn.execute_batch(schema::FTS_DDL).context("creating search index")?;
        self.conn.execute_batch(schema::PARSE_CACHE_DDL).context("creating parse cache")?;
        self.conn.execute_batch(schema::GIT_CACHE_DDL).context("creating git cache")?;
        let current: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match current {
            None => {
                self.conn.execute(
                    "INSERT INTO schema_meta(key, value) VALUES ('version', ?1)",
                    params![schema::SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(v) => {
                let v: i64 = v.parse().unwrap_or(0);
                anyhow::ensure!(
                    v <= schema::SCHEMA_VERSION,
                    "database schema v{v} is newer than this build (v{}) — upgrade `catlas`",
                    schema::SCHEMA_VERSION
                );
            }
        }
        Ok(())
    }

    /// Insert the project row if absent, returning its id.
    pub fn ensure_project(&self, name: &str, root: &Path) -> Result<i64> {
        let existing: Option<i64> = self
            .conn
            .query_row("SELECT id FROM projects WHERE name = ?1", params![name], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(id) = existing {
            self.conn.execute(
                "UPDATE projects SET root = ?1 WHERE id = ?2",
                params![root.to_string_lossy(), id],
            )?;
            return Ok(id);
        }
        self.conn.execute(
            "INSERT INTO projects(name, root, created_at) VALUES (?1, ?2, ?3)",
            params![name, root.to_string_lossy(), crate::util::now_iso()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn project_id(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT id FROM projects LIMIT 1", [], |r| r.get(0))
            .context("no project row — run `catlas init`")
    }

    pub fn count(&self, table: &str) -> Result<i64> {
        // `table` is always a literal from our own call sites, never user input.
        let sql = format!("SELECT COUNT(*) FROM {table}");
        Ok(self.conn.query_row(&sql, [], |r| r.get(0))?)
    }

    /// Register files, returning `rel path → file id`.
    ///
    /// Re-hashes every file on disk (the content identity that both `build` and
    /// `sync` key their work on). `build` runs this on an empty table after
    /// `reset_layers`; `sync` re-runs the full pipeline, so the same wipe applies.
    pub fn index_files(
        &self,
        project_id: i64,
        root: &Path,
        files: &[walk::Found],
        modules: &std::collections::BTreeMap<String, i64>,
        histories: &std::collections::BTreeMap<String, git::FileHistory>,
    ) -> Result<std::collections::BTreeMap<String, i64>> {
        let tx = self.conn.unchecked_transaction()?;
        let mut ids = std::collections::BTreeMap::new();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO files(project_id, path, lang, module_id, sha256, loc,
                                   first_commit_at, last_commit_at, commit_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for f in files {
                let bytes = std::fs::read(&f.path)
                    .with_context(|| format!("reading {}", f.path.display()))?;
                let sha = crate::util::sha256_hex(&bytes);
                let loc = bytes.iter().filter(|b| **b == b'\n').count() as i64 + 1;
                let module_id = owning_module(&f.rel, modules);
                let h = histories.get(&f.rel);
                stmt.execute(params![
                    project_id,
                    f.rel,
                    f.lang.as_str(),
                    module_id,
                    sha,
                    loc,
                    h.and_then(|x| x.first_commit_at.clone()),
                    h.and_then(|x| x.last_commit_at.clone()),
                    h.map_or(0, |x| x.commit_count as i64),
                ])?;
                ids.insert(f.rel.clone(), tx.last_insert_rowid());
            }
        }
        tx.commit()?;
        let _ = root;
        Ok(ids)
    }

    /// Record the tool minor version that last produced a successful build, so
    /// `sync` can invalidate cached parses after a grammar change.
    pub fn record_tool_version(&self, project_id: i64, version: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tool_version(project_id, version) VALUES (?1, ?2)
             ON CONFLICT(project_id) DO UPDATE SET version = excluded.version",
            params![project_id, version],
        )?;
        Ok(())
    }

    pub fn tool_version(&self, project_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT version FROM tool_version WHERE project_id = ?1",
                params![project_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The parse result for `sha`, if it was stored by a build of the current
    /// tool version.
    pub fn cached_parse(&self, project_id: i64, sha: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT result_json FROM parse_cache WHERE project_id = ?1 AND sha256 = ?2",
                params![project_id, sha],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Store a validated parse result so `sync` can reuse it next time the file
    /// is unchanged.
    pub fn cache_parse(&self, project_id: i64, sha: &str, lang: &str, json: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO parse_cache(project_id, sha256, lang, result_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![project_id, sha, lang, json],
        )?;
        Ok(())
    }

    /// The cached git signals for `head`, if a build at this `GIT_LOG_LIMIT`
    /// stored them.
    pub fn cached_git(&self, head: &str, commit_log_limit: i64) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT file_history_json, commits_json FROM git_cache
                 WHERE head = ?1 AND commit_log_limit = ?2",
                params![head, commit_log_limit],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// The cached head for this `GIT_LOG_LIMIT`, if any — the "old" end of the
    /// `old..new` range `sync` ingests.
    pub fn cached_head(&self, commit_log_limit: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT head FROM git_cache WHERE commit_log_limit = ?1",
                params![commit_log_limit],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Store the two expensive git walks so a later `sync` at the same `HEAD`
    /// replays them instead of re-running them.
    pub fn cache_git(
        &self,
        head: &str,
        commit_log_limit: i64,
        file_history_json: &str,
        commits_json: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO git_cache(head, commit_log_limit, file_history_json, commits_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                head,
                commit_log_limit,
                file_history_json,
                commits_json,
                crate::util::now_iso()
            ],
        )?;
        Ok(())
    }

    /// Drop git-cache rows for every `HEAD` but the current one, keeping the
    /// cache to a single row so a long-lived project database does not grow
    /// one entry per commit forever.
    pub fn prune_git_cache(&self, keep_head: &str) -> Result<()> {
        self.conn.execute("DELETE FROM git_cache WHERE head <> ?1", params![keep_head])?;
        Ok(())
    }

    /// Wipe every derived layer for a project, keeping the project row and the
    /// LLM response cache (which is keyed by prompt hash and stays valid).
    pub fn reset_layers(&self, project_id: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for t in [
            "note_evidence",
            "glossary_links",
            "traces",
            "domain_edges",
            "domain_members",
            "table_access",
            "columns",
            "symbol_annotations",
            "refs",
            "entrypoints",
            "config_props",
            "git_touches",
        ] {
            tx.execute(&format!("DELETE FROM {t}"), [])?;
        }
        for t in ["domains", "tables", "symbols", "git_commits", "git_branches", "files", "modules"] {
            tx.execute(&format!("DELETE FROM {t} WHERE project_id = ?1"), params![project_id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Recreate the parse cache from scratch (see `PARSE_CACHE_DDL`).
    ///
    /// Called by a `--full` build so the cache cannot carry a parse produced by
    /// an older tree-sitter grammar into a newer build, and so it stays bounded
    /// to the files actually present.
    pub fn reset_parse_cache(&self) -> Result<()> {
        self.conn.execute_batch("DROP TABLE IF EXISTS parse_cache")?;
        self.conn.execute_batch(schema::PARSE_CACHE_DDL)?;
        Ok(())
    }
}

/// The most specific module whose directory contains this file.
fn owning_module(rel: &str, modules: &std::collections::BTreeMap<String, i64>) -> Option<i64> {
    let mut best: Option<(usize, i64)> = None;
    for (dir, id) in modules {
        let matches = dir == "." || rel.starts_with(&format!("{dir}/"));
        if matches {
            let depth = if dir == "." { 0 } else { dir.matches('/').count() + 1 };
            if best.is_none_or(|(d, _)| depth > d) {
                best = Some((depth, *id));
            }
        }
    }
    best.map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_root_walks_up_to_the_knowledge_base() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        Store::open(root).unwrap();
        let deep = root.join("svc/order/src/main/java");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(Store::find_root(&deep).unwrap(), root);
        assert_eq!(Store::find_root(root).unwrap(), root);
    }

    #[test]
    fn find_root_reports_the_directory_it_started_from() {
        let dir = tempfile::tempdir().unwrap();
        let err = Store::find_root(dir.path()).unwrap_err().to_string();
        assert!(err.contains(&dir.path().display().to_string()), "{err}");
        assert!(err.contains("catlas init"), "{err}");
    }

    #[test]
    fn open_existing_from_a_subdirectory_keeps_the_project_root() {
        let dir = tempfile::tempdir().unwrap();
        Store::open(dir.path()).unwrap();
        let sub = dir.path().join("module-a");
        std::fs::create_dir_all(&sub).unwrap();

        let store = Store::open_existing(&sub).unwrap();
        assert_eq!(store.root, dir.path());
    }
}
