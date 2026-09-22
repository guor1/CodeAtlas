//! Truth layer: SQLite storage for L0 evidence, L1 structure, L2 narrative.

pub mod model;
pub mod schema;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

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

    /// Open an existing database, failing if it has not been initialized.
    pub fn open_existing(root: &Path) -> Result<Self> {
        let db_path = root.join(CODEATLAS_DIR).join(DB_FILE);
        anyhow::ensure!(
            db_path.exists(),
            "no knowledge base at {} — run `catlas init` first",
            db_path.display()
        );
        Self::open(root)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(schema::DDL).context("applying schema")?;
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
}
