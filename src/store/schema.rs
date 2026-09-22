//! SQLite schema. Truth layer for all three knowledge levels.
//!
//! Every table carries `project_id` so a future `catlas federate` can ATTACH
//! several project databases into one hub without a schema change.

pub const SCHEMA_VERSION: i64 = 2;

pub const DDL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS schema_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- ---------------------------------------------------------------- meta
CREATE TABLE IF NOT EXISTS projects (
  id           INTEGER PRIMARY KEY,
  name         TEXT NOT NULL UNIQUE,
  root         TEXT NOT NULL,
  repo_url     TEXT,
  primary_lang TEXT,
  created_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS build_runs (
  id          INTEGER PRIMARY KEY,
  project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL,
  started_at  TEXT NOT NULL,
  finished_at TEXT,
  tool_version TEXT NOT NULL,
  stats_json  TEXT
);
-- `version` holds the tool's minor version as of the last successful build
-- (populated by `finish_run` from the lib crate's `VERSION_MINOR`). `sync` uses
-- it to know which parses in `parse_cache` were produced by a compatible build.
-- v2 databases from the schema_meta migration have no such row; the first build
-- writes it, and until then `version` is NULL, which `sync` treats as "reparse".
CREATE TABLE IF NOT EXISTS tool_version (
  project_id INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
  version    INTEGER
);

-- ---------------------------------------------------------------- L0
CREATE TABLE IF NOT EXISTS modules (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  path       TEXT NOT NULL,
  packaging  TEXT,
  parent_id  INTEGER REFERENCES modules(id),
  UNIQUE(project_id, path)
);
CREATE TABLE IF NOT EXISTS files (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  path       TEXT NOT NULL,
  lang       TEXT,
  module_id  INTEGER REFERENCES modules(id),
  sha256     TEXT NOT NULL,
  loc        INTEGER NOT NULL DEFAULT 0,
  first_commit_at TEXT,
  last_commit_at  TEXT,
  commit_count    INTEGER NOT NULL DEFAULT 0,
  UNIQUE(project_id, path)
);
CREATE INDEX IF NOT EXISTS idx_files_module ON files(module_id);
CREATE INDEX IF NOT EXISTS idx_files_lang ON files(project_id, lang);

CREATE TABLE IF NOT EXISTS symbols (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  file_id    INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL,
  name       TEXT NOT NULL,
  fqn        TEXT,
  signature  TEXT,
  start_line INTEGER NOT NULL,
  end_line   INTEGER NOT NULL,
  doc        TEXT,
  visibility TEXT,
  parent_id  INTEGER REFERENCES symbols(id)
);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);
CREATE INDEX IF NOT EXISTS idx_symbols_fqn ON symbols(project_id, fqn);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(project_id, name);
CREATE INDEX IF NOT EXISTS idx_symbols_kind ON symbols(project_id, kind);

CREATE TABLE IF NOT EXISTS symbol_annotations (
  id        INTEGER PRIMARY KEY,
  symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
  name      TEXT NOT NULL,
  args_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_annot_symbol ON symbol_annotations(symbol_id);
CREATE INDEX IF NOT EXISTS idx_annot_name ON symbol_annotations(name);

CREATE TABLE IF NOT EXISTS refs (
  id             INTEGER PRIMARY KEY,
  project_id     INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  src_symbol_id  INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
  dst_symbol_id  INTEGER REFERENCES symbols(id) ON DELETE SET NULL,
  dst_fqn_raw    TEXT,
  kind           TEXT NOT NULL,
  resolved       INTEGER NOT NULL DEFAULT 0,
  confidence     REAL NOT NULL DEFAULT 0.0
);
CREATE INDEX IF NOT EXISTS idx_refs_src ON refs(src_symbol_id);
CREATE INDEX IF NOT EXISTS idx_refs_dst ON refs(dst_symbol_id);
CREATE TABLE IF NOT EXISTS tables (
  id                INTEGER PRIMARY KEY,
  project_id        INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name              TEXT NOT NULL,
  source            TEXT NOT NULL,
  evidence_file_id  INTEGER REFERENCES files(id) ON DELETE SET NULL,
  UNIQUE(project_id, name)
);

CREATE TABLE IF NOT EXISTS columns (
  id        INTEGER PRIMARY KEY,
  table_id  INTEGER NOT NULL REFERENCES tables(id) ON DELETE CASCADE,
  name      TEXT NOT NULL,
  prop_name TEXT,
  java_type TEXT,
  doc       TEXT,
  is_pk     INTEGER NOT NULL DEFAULT 0,
  UNIQUE(table_id, name)
);

CREATE TABLE IF NOT EXISTS table_access (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  table_id   INTEGER NOT NULL REFERENCES tables(id) ON DELETE CASCADE,
  symbol_id  INTEGER REFERENCES symbols(id) ON DELETE CASCADE,
  file_id    INTEGER REFERENCES files(id) ON DELETE CASCADE,
  op         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_taccess_table ON table_access(table_id);
CREATE INDEX IF NOT EXISTS idx_taccess_symbol ON table_access(symbol_id);
CREATE INDEX IF NOT EXISTS idx_taccess_file ON table_access(file_id);

CREATE TABLE IF NOT EXISTS entrypoints (
  id          INTEGER PRIMARY KEY,
  project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL,
  name        TEXT NOT NULL,
  addr        TEXT,
  symbol_id   INTEGER REFERENCES symbols(id) ON DELETE SET NULL,
  file_id     INTEGER REFERENCES files(id) ON DELETE SET NULL,
  config_json TEXT,
  doc         TEXT
);
CREATE INDEX IF NOT EXISTS idx_entry_kind ON entrypoints(project_id, kind);
CREATE INDEX IF NOT EXISTS idx_entry_symbol ON entrypoints(symbol_id);

CREATE TABLE IF NOT EXISTS config_props (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  file_id    INTEGER REFERENCES files(id) ON DELETE CASCADE,
  key        TEXT NOT NULL,
  value      TEXT
);
CREATE INDEX IF NOT EXISTS idx_cfg_key ON config_props(project_id, key);

CREATE TABLE IF NOT EXISTS git_commits (
  id          INTEGER PRIMARY KEY,
  project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  sha         TEXT NOT NULL,
  authored_at TEXT,
  subject     TEXT,
  UNIQUE(project_id, sha)
);

CREATE TABLE IF NOT EXISTS git_touches (
  commit_id INTEGER NOT NULL REFERENCES git_commits(id) ON DELETE CASCADE,
  file_id   INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  PRIMARY KEY(commit_id, file_id)
);

CREATE TABLE IF NOT EXISTS git_branches (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  UNIQUE(project_id, name)
);
-- ---------------------------------------------------------------- L1
CREATE TABLE IF NOT EXISTS domains (
  id             INTEGER PRIMARY KEY,
  project_id     INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  key            TEXT NOT NULL,
  label          TEXT,
  confidence     REAL NOT NULL DEFAULT 0.0,
  rationale_json TEXT,
  curated        INTEGER NOT NULL DEFAULT 0,
  UNIQUE(project_id, key)
);

CREATE TABLE IF NOT EXISTS domain_members (
  domain_id INTEGER NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  kind      TEXT NOT NULL,
  ref_id    INTEGER NOT NULL,
  weight    REAL NOT NULL DEFAULT 1.0,
  PRIMARY KEY(domain_id, kind, ref_id)
);
CREATE INDEX IF NOT EXISTS idx_dmember_ref ON domain_members(kind, ref_id);

CREATE TABLE IF NOT EXISTS domain_edges (
  src_domain_id INTEGER NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  dst_domain_id INTEGER NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  kind          TEXT NOT NULL,
  weight        REAL NOT NULL DEFAULT 0.0,
  evidence_json TEXT,
  PRIMARY KEY(src_domain_id, dst_domain_id, kind)
);

CREATE TABLE IF NOT EXISTS traces (
  id            INTEGER PRIMARY KEY,
  project_id    INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  entrypoint_id INTEGER NOT NULL REFERENCES entrypoints(id) ON DELETE CASCADE,
  path_json     TEXT NOT NULL,
  depth         INTEGER NOT NULL,
  tables_json   TEXT
);
CREATE INDEX IF NOT EXISTS idx_trace_entry ON traces(entrypoint_id);

-- ---------------------------------------------------------------- L2
CREATE TABLE IF NOT EXISTS notes (
  id            INTEGER PRIMARY KEY,
  project_id    INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  kind          TEXT NOT NULL,
  subject_kind  TEXT,
  subject_id    INTEGER,
  -- Stable identity of the subject, e.g. a domain key. `subject_id` is a row id
  -- that `build` recreates, so it cannot be relied on across rebuilds: L2 output
  -- is expensive and must survive an L0 refresh.
  subject_key   TEXT,
  title         TEXT,
  body_md       TEXT NOT NULL,
  status        TEXT NOT NULL DEFAULT 'fresh',
  model         TEXT,
  prompt_hash   TEXT,
  source_digest TEXT,
  generated_at  TEXT
);
CREATE INDEX IF NOT EXISTS idx_notes_subject ON notes(project_id, subject_kind, subject_id);
CREATE INDEX IF NOT EXISTS idx_notes_kind ON notes(project_id, kind);
CREATE INDEX IF NOT EXISTS idx_notes_status ON notes(status);

CREATE TABLE IF NOT EXISTS note_evidence (
  note_id    INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL,
  ref_id     INTEGER,
  ref_text   TEXT,
  line_start INTEGER,
  line_end   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_nev_note ON note_evidence(note_id);

CREATE TABLE IF NOT EXISTS glossary (
  id            INTEGER PRIMARY KEY,
  project_id    INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  term          TEXT NOT NULL,
  normalized    TEXT,
  definition_md TEXT,
  aliases_json  TEXT,
  scope         TEXT NOT NULL DEFAULT 'project',
  domain_id     INTEGER REFERENCES domains(id) ON DELETE SET NULL,
  -- Survives the domain row being recreated by `build`; see `notes.subject_key`.
  domain_key    TEXT,
  status        TEXT NOT NULL DEFAULT 'fresh',
  model         TEXT,
  prompt_hash   TEXT,
  source_digest TEXT,
  UNIQUE(project_id, term, scope)
);

CREATE TABLE IF NOT EXISTS glossary_links (
  glossary_id INTEGER NOT NULL REFERENCES glossary(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL,
  ref_id      INTEGER,
  ref_text    TEXT
);
CREATE INDEX IF NOT EXISTS idx_glink_g ON glossary_links(glossary_id);

-- ---------------------------------------------------------------- llm
CREATE TABLE IF NOT EXISTS llm_cache (
  prompt_hash   TEXT NOT NULL,
  model         TEXT NOT NULL,
  request_json  TEXT,
  response_json TEXT,
  input_tokens  INTEGER,
  output_tokens INTEGER,
  created_at    TEXT NOT NULL,
  PRIMARY KEY(prompt_hash, model)
);

-- ---------------------------------------------------------------- search
-- The full-text index is a *derived* view (like the Markdown output), so it is
-- safe to drop and rebuild at any time. It is created separately from the rest
-- of the schema — see `FTS_DDL` below — because migrating a v1 database requires
-- dropping the v1 table, and `IF NOT EXISTS` alone cannot change its tokenizer.
"#;

/// The FTS table, created outside `DDL` so the migrate path can drop a v1 table
/// (whose `unicode61` tokenizer cannot be altered in place) before recreating it.
///
/// `trigram` matches substrings in both unsegmented Chinese and English
/// identifiers, which a word-based tokenizer cannot do for CJK. Column order
/// matches `INSERT INTO search_fts(...)` in `src/search.rs`.
pub const FTS_DDL: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS search_fts USING fts5(
  title, body, kind UNINDEXED, subject_kind UNINDEXED,
  subject_id UNINDEXED, project_id UNINDEXED, file_path UNINDEXED,
  tokenize = 'trigram'
);
"#;

/// The `parse_cache` table, created outside `DDL` (like `FTS_DDL`) so that
/// `rebuild` — which recreates it from scratch — cannot race the idempotent
/// `CREATE IF NOT EXISTS` that `migrate` runs against the same statement.
///
/// One row per parsed source file, keyed by content hash: `sync` stores a file's
/// tree-sitter parse here and reuses it instead of re-parsing unchanged files.
pub const PARSE_CACHE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS parse_cache (
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  sha256     TEXT NOT NULL,
  lang       TEXT NOT NULL,
  result_json TEXT NOT NULL,
  PRIMARY KEY(project_id, sha256)
);
"#;
