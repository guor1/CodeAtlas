//! `catlas` — business-domain knowledge base generator for legacy codebases.
//!
//! Three layers, kept strictly separate so narrative can always be traced back
//! to evidence:
//!   * **L0 evidence** ([`extract`]) — deterministic facts parsed from source,
//!     build files, config and git history. No LLM involved.
//!   * **L1 structure** ([`structure`]) — domain partitioning and entrypoint
//!     traces, derived from L0 by pure computation.
//!   * **L2 narrative** ([`llm`]) — glossary and domain dossiers written by a
//!     model, where every record carries evidence references and a digest of
//!     its sources so it can be invalidated when the code moves.
//!
//! [`store`] is the truth layer (SQLite); [`render`] projects it into Markdown
//! for humans and for Claude Code.

pub mod build;
pub mod extract;
pub mod llm;
pub mod render;
pub mod search;
pub mod store;
pub mod structure;
pub mod sync;
pub mod util;

/// Minor version as an integer (`0.1` → `1`).
///
/// Cached parses are only reused when this matches the version that produced
/// them, so a tree-sitter or schema change invalidates the cache without any
/// manual step. `sync` compares this against the stored `tool_version`.
pub static VERSION_MINOR: std::sync::LazyLock<i64> = std::sync::LazyLock::new(|| {
    env!("CARGO_PKG_VERSION")
        .split('.')
        .nth(1)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
});
