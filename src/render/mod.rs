//! Project the database into Markdown for humans and for Claude Code.
//!
//! Markdown is a view, never the source of truth: `render` is safe to run
//! repeatedly and the output directory can be deleted without losing anything.

mod claude_md;
mod domains;
mod glossary;
mod reference;

use crate::store::Store;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Default output directory, relative to the project root.
pub const OUT_DIR: &str = ".knowledge";

pub struct RenderStats {
    pub files_written: usize,
    pub out_dir: PathBuf,
}

pub fn run(store: &Store, root: &Path, out: Option<&Path>, write_claude_md: bool) -> Result<RenderStats> {
    let project_id = store.project_id()?;
    let out_dir = match out {
        Some(p) if p.is_absolute() => p.to_path_buf(),
        Some(p) => root.join(p),
        None => root.join(OUT_DIR),
    };
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("创建 {} 失败", out_dir.display()))?;

    let mut written = 0usize;
    written += write(&out_dir.join("README.md"), &overview(store, project_id, root)?)?;
    written += reference::write_all(store, project_id, &out_dir)?;
    written += domains::write_all(store, project_id, &out_dir)?;
    written += glossary::write_all(store, project_id, &out_dir)?;

    if write_claude_md {
        written += claude_md::write(store, project_id, root, &out_dir)?;
    }
    Ok(RenderStats { files_written: written, out_dir })
}

/// Write a file, returning 1 so callers can total it.
fn write(path: &Path, body: &str) -> Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body).with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(1)
}

/// Escape a cell so a pipe in a Javadoc cannot break the table.
pub(crate) fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ").trim().to_string()
}

/// First sentence or line of a doc comment, for table cells.
pub(crate) fn summarize(doc: Option<&str>, max: usize) -> String {
    let Some(doc) = doc else { return String::new() };
    let first = doc
        .lines()
        // Javadoc tag lines describe parameters, not the member itself.
        .find(|l| !l.trim_start().starts_with('@') && !l.trim().is_empty())
        .unwrap_or("");
    let first = first.split(['。', '；']).next().unwrap_or(first);
    cell(&crate::util::truncate_chars(first.trim(), max))
}

fn overview(store: &Store, project_id: i64, root: &Path) -> Result<String> {
    use std::fmt::Write as _;
    let mut s = String::new();
    let name: String = store.conn.query_row(
        "SELECT name FROM projects WHERE id = ?1",
        rusqlite::params![project_id],
        |r| r.get(0),
    )?;

    writeln!(s, "# {name} 业务领域知识库\n")?;
    writeln!(
        s,
        "本目录由 [`catlas`](https://github.com/) 自动生成，**不要手工编辑**：\
         内容会在下次 `catlas render` 时被覆盖。\n"
    )?;
    writeln!(s, "- 项目根目录：`{}`", root.display())?;
    writeln!(s, "- 生成时间：{}\n", crate::util::now_iso())?;

    writeln!(s, "## 规模\n")?;
    writeln!(s, "| 项 | 数量 |")?;
    writeln!(s, "|---|---|")?;
    for (label, table) in [
        ("模块", "modules"),
        ("文件", "files"),
        ("符号", "symbols"),
        ("数据表", "tables"),
        ("入口", "entrypoints"),
        ("业务领域", "domains"),
    ] {
        writeln!(s, "| {label} | {} |", store.count(table)?)?;
    }
    writeln!(s)?;

    writeln!(s, "## 怎么用\n")?;
    writeln!(s, "| 想知道什么 | 看哪里 |")?;
    writeln!(s, "|---|---|")?;
    writeln!(s, "| 某个业务领域怎么运作 | `domains/<领域>.md` |")?;
    writeln!(s, "| 有哪些表、字段什么含义 | `reference/tables.md` |")?;
    writeln!(s, "| 系统从哪里被调用 | `reference/entrypoints.md` |")?;
    writeln!(s, "| 定时任务和消息消费 | `reference/jobs.md`、`reference/mq.md` |")?;
    writeln!(s, "| 某个中文术语对应哪段代码 | `glossary.md` |")?;
    writeln!(s, "| 精确检索 | `catlas query \"<关键词>\"` |")?;
    writeln!(s)?;

    s.push_str(&domains::index_section(store, project_id)?);
    Ok(s)
}
