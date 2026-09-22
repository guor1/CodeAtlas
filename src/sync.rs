//! Incremental rebuild: the full build pipeline with parse-cache reuse.
//!
//! A naive "re-index only changed files" sync is not sound here: reference
//! resolution, domain voting and trace building are global over the whole
//! project, so re-indexing one file without re-running them would leave stale
//! call edges and corrupt the very facts this tool exists to be right about.
//!
//! Instead `sync` runs the *identical* pipeline as `build` and only skips the
//! one step that is genuinely per-file and expensive: tree-sitter parsing.
//! Every file is re-hashed; a file whose content was parsed by a build of the
//! current tool version is reused from `parse_cache` rather than re-parsed.
//! The result is byte-for-byte what a full `build` would produce.
//!
//! The change detector is the stored SHA-256 of file content, **not** git
//! history — see `docs/roadmap.md` ("sync 的变更检测为什么不看 git"). Git log
//! is the slowest part of a build and reports "which commits touched a file",
//! which says nothing about whether the bytes changed since we last indexed
//! them, and misses uncommitted edits entirely.

use crate::build::BuildStats;
use crate::store::Store;
use crate::VERSION_MINOR;
use anyhow::Result;
use std::path::Path;

pub fn run(store: &Store, root: &Path) -> Result<BuildStats> {
    let project_id = store.project_id()?;

    // A tool upgrade changes the tree-sitter grammar and possibly the schema, so
    // parses cached by an older version must not be reused. `tool_version` is
    // written by `pipeline`, so a database that predates this feature (NULL)
    // also falls through to a full parse here.
    if store.tool_version(project_id)? != Some(*VERSION_MINOR) {
        eprintln!("工具版本已变更（或无缓存记录），丢弃解析缓存，按全量重建");
        store.reset_parse_cache()?;
    }

    crate::build::pipeline(store, root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// A one-file Java project, ready to build.
    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("src/main/java/a")).unwrap();
        std::fs::write(
            p.join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion><artifactId>x</artifactId></project>",
        )
        .unwrap();
        std::fs::write(
            p.join("src/main/java/a/A.java"),
            "package a;\n/** 特价活动 */\npublic class A { int f; }",
        )
        .unwrap();
        let store = Store::open(p).unwrap();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        store.ensure_project(&name, p).unwrap();
        (dir, store)
    }

    fn cache_rows(store: &Store, project_id: i64) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM parse_cache WHERE project_id = ?1",
                params![project_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn pipeline_populates_cache_and_reuses_it() {
        let (dir, store) = fixture();
        let pid = store.project_id().unwrap();

        let first = crate::build::pipeline(&store, dir.path()).unwrap();
        assert!(first.symbols > 0);
        assert_eq!(cache_rows(&store, pid), 1, "one java file → one cached parse");

        // A second pipeline run (no cache reset) must reuse the cached parse and
        // produce identical output, without growing the cache.
        let second = crate::build::pipeline(&store, dir.path()).unwrap();
        assert_eq!(second.symbols, first.symbols);
        assert_eq!(cache_rows(&store, pid), 1);
    }

    #[test]
    fn sync_resets_cache_when_version_changed() {
        let (dir, store) = fixture();
        let pid = store.project_id().unwrap();
        crate::build::pipeline(&store, dir.path()).unwrap();
        assert_eq!(store.tool_version(pid).unwrap(), Some(*VERSION_MINOR));

        // Forge a version mismatch: sync must discard the cache and repopulate.
        store.record_tool_version(pid, 999).unwrap();
        run(&store, dir.path()).unwrap();

        assert_eq!(store.tool_version(pid).unwrap(), Some(*VERSION_MINOR));
        assert_eq!(cache_rows(&store, pid), 1, "cache repopulated after reset");
    }

    #[test]
    fn sync_on_unchanged_tree_keeps_symbols_stable() {
        let (dir, store) = fixture();
        let before = crate::build::pipeline(&store, dir.path()).unwrap();
        let after = run(&store, dir.path()).unwrap();
        assert_eq!(after.symbols, before.symbols);
    }
}
