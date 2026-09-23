//! `catlas` CLI.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use codeatlas::{build, llm, render, search, store::Store, sync};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "catlas",
    version,
    about = "为存量老项目生成业务领域知识库",
    long_about = "catlas 从代码、配置与 git 历史中抽取确定性事实，划分业务领域，\n\
                  再由大模型在证据之上生成领域文档，供开发者与 Claude Code 使用。"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 初始化知识库并执行首次构建
    Init {
        /// 项目根目录，默认为当前目录
        path: Option<PathBuf>,
    },
    /// 全量重建 L0 事实层与 L1 结构层
    Build {
        path: Option<PathBuf>,
    },
    /// 增量同步：只重解析内容有变化的文件，其余复用缓存
    Sync {
        path: Option<PathBuf>,
    },
    /// 显示知识库各层统计与新鲜度
    Status {
        path: Option<PathBuf>,
    },
    /// 将知识库渲染为 Markdown 视图
    Render {
        path: Option<PathBuf>,
        /// 输出目录，默认为项目下的 .knowledge/
        #[arg(long)]
        out: Option<PathBuf>,
        /// 同时更新项目 CLAUDE.md 中由 catlas 托管的段落
        #[arg(long)]
        claude_md: bool,
    },
    /// 调用大模型为领域生成解读与术语表
    Deepen {
        path: Option<PathBuf>,
        /// 只处理指定领域，可重复；缺省为全部
        #[arg(long = "domain")]
        domains: Vec<String>,
        /// 生成单入口能力叙述，而非领域解读
        #[arg(long)]
        capability: bool,
        /// 能力叙述只处理指定入口类型，可重复（dubbo/http/job/mq）
        #[arg(long)]
        kind: Vec<String>,
        /// 只估算 token 消耗，不实际调用
        #[arg(long)]
        dry_run: bool,
        /// 最多处理几个领域
        #[arg(long)]
        limit: Option<usize>,
        /// 覆盖模型
        #[arg(long)]
        model: Option<String>,
        /// 强制重算，忽略已有的新鲜结果
        #[arg(long)]
        force: bool,
    },
    /// 列出已划分的业务领域
    Domains {
        path: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// 全文检索知识库（符号、表、入口、领域、术语、提交信息）
    Query {
        /// 检索关键词
        terms: Vec<String>,
        /// 项目根目录，默认为当前目录（会向上查找 .codeatlas/）
        #[arg(long)]
        path: Option<PathBuf>,
        /// 最多返回条数
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// 输出 JSON
        #[arg(long)]
        json: bool,
    },
    /// 以 MCP server 方式运行（stdio），暴露知识库给 Claude Code
    Mcp {
        /// 项目根目录，默认为当前目录（会向上查找 .codeatlas/）
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { path } => init(resolve(path)?),
        Command::Build { path } => cmd_build(existing(path)?),
        Command::Sync { path } => cmd_sync(existing(path)?),
        Command::Status { path } => status(existing(path)?),
        Command::Render { path, out, claude_md } => {
            cmd_render(existing(path)?, out.as_deref(), claude_md)
        }
        Command::Deepen { path, domains, capability, kind, dry_run, limit, model, force } => {
            cmd_deepen(existing(path)?, domains, capability, kind, dry_run, limit, model.as_deref(), force)
        }
        Command::Domains { path, json } => cmd_domains(existing(path)?, json),
        Command::Query { path, terms, limit, json } => {
            cmd_query(existing(path)?, terms, limit, json)
        }
        Command::Mcp { path } => codeatlas::mcp::run(&existing(path)?),
    }
}

/// Canonicalize the target directory so stored paths are stable.
fn resolve(path: Option<PathBuf>) -> Result<PathBuf> {
    let p = path.unwrap_or_else(|| PathBuf::from("."));
    let p = p
        .canonicalize()
        .with_context(|| format!("解析路径 {} 失败", p.display()))?;
    anyhow::ensure!(p.is_dir(), "{} 不是目录", p.display());
    Ok(p)
}

/// Resolve the root for a command that needs a knowledge base already built.
///
/// Unlike `resolve`, this walks up to the directory that owns `.codeatlas/`, so
/// every command works from a subdirectory — and so the root used for scanning,
/// rendering and printing is the same one the store opened.
fn existing(path: Option<PathBuf>) -> Result<PathBuf> {
    Store::find_root(&resolve(path)?)
}

fn project_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string())
}

fn init(root: PathBuf) -> Result<()> {
    let store = Store::open(&root)?;
    let name = project_name(&root);
    store.ensure_project(&name, &root)?;
    write_gitignore(&root)?;
    println!("已初始化 {}/.codeatlas", root.display());
    cmd_build(root)
}

/// Keep the database out of git; it is a derived artifact, like `.codegraph/`.
fn write_gitignore(root: &Path) -> Result<()> {
    let path = root.join(codeatlas::store::CODEATLAS_DIR).join(".gitignore");
    if path.exists() {
        return Ok(());
    }
    std::fs::write(
        &path,
        "# catlas 的派生产物，不纳入版本管理\n*.db\n*.db-shm\n*.db-wal\n",
    )
    .with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(())
}

fn cmd_build(root: PathBuf) -> Result<()> {
    let store = Store::open_existing(&root)?;
    let name = project_name(&root);
    store.ensure_project(&name, &root)?;

    let started = std::time::Instant::now();
    let stats = build::run(&store, &root)?;
    let secs = started.elapsed().as_secs_f64();

    print_stats(&stats, secs);
    Ok(())
}

/// Shared summary printer for `build` and `sync`.
fn print_stats(stats: &build::BuildStats, secs: f64) {
    println!("构建完成，耗时 {secs:.1}s");
    println!("  模块        {}", stats.modules);
    println!("  文件        {}", stats.files);
    println!("  符号        {}", stats.symbols);
    println!(
        "  调用边      {} (已解析 {}，{:.0}%)",
        stats.refs,
        stats.refs_resolved,
        pct(stats.refs_resolved, stats.refs)
    );
    println!("  数据表      {}", stats.tables);
    println!("  字段        {}", stats.columns);
    println!("  表访问      {}", stats.table_access);
    println!("  入口        {}", stats.entrypoints);
    println!("  配置项      {}", stats.config_props);
    println!("  提交        {}", stats.commits);
    println!("  领域        {}", stats.domains);
    println!("  链路        {}", stats.traces);
    if stats.parse_errors > 0 {
        println!("  解析告警    {} 个文件含语法错误（已尽力抽取）", stats.parse_errors);
    }
}

fn cmd_sync(root: PathBuf) -> Result<()> {
    let store = Store::open_existing(&root)?;
    let started = std::time::Instant::now();
    let stats = sync::run(&store, &root)?;
    let secs = started.elapsed().as_secs_f64();
    print_stats(&stats, secs);
    Ok(())
}

fn pct(part: usize, whole: usize) -> f64 {
    if whole == 0 { 0.0 } else { part as f64 * 100.0 / whole as f64 }
}

fn cmd_render(root: PathBuf, out: Option<&Path>, claude_md: bool) -> Result<()> {
    let store = Store::open_existing(&root)?;
    let stats = render::run(&store, &root, out, claude_md)?;
    println!("已生成 {} 个文件到 {}", stats.files_written, stats.out_dir.display());
    if claude_md {
        println!("已更新 {}", root.join("CLAUDE.md").display());
    }
    Ok(())
}

fn cmd_deepen(
    root: PathBuf,
    domains: Vec<String>,
    capability: bool,
    kinds: Vec<String>,
    dry_run: bool,
    limit: Option<usize>,
    model: Option<&str>,
    force: bool,
) -> Result<()> {
    let store = Store::open_existing(&root)?;
    let opts = llm::deepen::DeepenOptions {
        domains,
        capability,
        kinds,
        dry_run,
        limit,
        only_stale: !force,
        ..Default::default()
    };

    // In dry-run mode no credential is needed, so a user can estimate cost
    // before deciding whether to configure one.
    let cfg = if dry_run {
        llm::client::Config::from_env(model).unwrap_or(llm::client::Config {
            base_url: String::new(),
            api_key: String::new(),
            model: model.unwrap_or("claude-sonnet-5").to_string(),
            max_tokens: 16384,
            timeout_secs: 300,
            budget_tokens: 0,
        })
    } else {
        llm::client::Config::from_env(model)?
    };

    let stats = if opts.capability {
        llm::deepen::run_capabilities(&store, &cfg, &opts)?
    } else {
        llm::deepen::run(&store, &cfg, &opts)?
    };

    if dry_run {
        let total: usize = stats.planned.iter().map(|(_, t)| *t).sum();
        let unit = if opts.capability { "个入口" } else { "个领域" };
        println!("将处理 {} {}，预估输入 token：", stats.planned.len(), unit);
        for (key, tokens) in &stats.planned {
            println!("  {key:<24} ~{tokens}");
        }
        println!("\n合计 ~{total} 输入 token（模型 {}）", cfg.model);
        println!("这是保守估算，实际用量以 API 返回为准。去掉 --dry-run 即开始执行。");
        return Ok(());
    }

    let unit = if opts.capability { "个入口" } else { "个领域" };
    println!("完成：{} {}已生成，{} 个跳过（结果仍新鲜）", stats.domains_processed, unit, stats.domains_skipped);
    if opts.capability {
        println!("  能力叙述  {}", stats.notes_written);
    } else {
        println!("  领域解读  {}", stats.notes_written);
        println!("  术语条目  {}", stats.glossary_written);
    }
    println!("  实耗 token 输入 {} / 输出 {}", stats.input_tokens, stats.output_tokens);
    if stats.cache_hits > 0 {
        println!(
            "  缓存命中  {} 次请求，省下输入 {} / 输出 {}",
            stats.cache_hits, stats.saved_input_tokens, stats.saved_output_tokens
        );
    }
    if !stats.failures.is_empty() {
        println!("\n{} 个失败：", stats.failures.len());
        for (key, err) in &stats.failures {
            println!("  {key}: {err}");
        }
    }
    println!("\n运行 `catlas render` 将结果写入 Markdown");
    Ok(())
}

fn cmd_domains(root: PathBuf, json: bool) -> Result<()> {
    let store = Store::open_existing(&root)?;
    let project_id = store.project_id()?;
    let rows: Vec<(String, Option<String>, f64, i64, i64, i64)> = store
        .conn
        .prepare(
            "SELECT d.key, d.label, d.confidence,
                (SELECT COUNT(*) FROM domain_members m WHERE m.domain_id=d.id AND m.kind='file'),
                (SELECT COUNT(*) FROM domain_members m WHERE m.domain_id=d.id AND m.kind='table'),
                (SELECT COUNT(*) FROM domain_members m WHERE m.domain_id=d.id AND m.kind='entrypoint')
             FROM domains d WHERE d.project_id = ?1 ORDER BY 4 DESC",
        )?
        .query_map(rusqlite::params![project_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    if json {
        let v: Vec<_> = rows
            .iter()
            .map(|(k, l, c, f, t, e)| {
                serde_json::json!({
                    "key": k, "label": l, "confidence": c,
                    "files": f, "tables": t, "entrypoints": e
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    println!("{:<24} {:>6} {:>6} {:>6} {:>8}", "领域", "文件", "表", "入口", "置信度");
    for (key, label, conf, files, tables, entries) in rows {
        let name = label.unwrap_or(key);
        println!("{name:<24} {files:>6} {tables:>6} {entries:>6} {conf:>8.2}");
    }
    println!("\n如需调整划分，编辑 .codeatlas/domains.toml 后重新 catlas build");
    Ok(())
}

fn cmd_query(root: PathBuf, terms: Vec<String>, limit: usize, json: bool) -> Result<()> {
    let store = Store::open_existing(&root)?;
    let project_id = store.project_id()?;

    // The index may be absent for a knowledge base built before search existed;
    // fill it on demand rather than forcing a full rebuild.
    let populated: i64 = store.conn.query_row("SELECT COUNT(*) FROM search_fts", [], |r| r.get(0))?;
    if populated == 0 {
        search::rebuild(&store, project_id)?;
    }

    let q = terms.join(" ");
    if q.trim().is_empty() {
        anyhow::bail!("请提供一个检索关键词，例如 `catlas query 特价`");
    }
    let hits = search::query(&store, project_id, &q, limit)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }

    if hits.is_empty() {
        println!("没有匹配「{q}」的结果。");
        return Ok(());
    }
    println!("匹配「{q}」{} 条：\n", hits.len());
    for h in &hits {
        let where_ = h.file.as_deref().map(|f| format!("  @ {f}")).unwrap_or_default();
        println!("[{}] {}{}", search::label(&h.kind, &h.subject_kind), h.title, where_);
        if !h.snippet.is_empty() {
            println!("      {}", h.snippet);
        }
    }
    Ok(())
}

fn status(root: PathBuf) -> Result<()> {
    let store = Store::open_existing(&root)?;
    println!("项目: {}", root.display());
    for (label, table) in [
        ("模块", "modules"),
        ("文件", "files"),
        ("符号", "symbols"),
        ("调用边", "refs"),
        ("数据表", "tables"),
        ("字段", "columns"),
        ("表访问", "table_access"),
        ("入口", "entrypoints"),
        ("配置项", "config_props"),
        ("提交", "git_commits"),
        ("领域", "domains"),
        ("链路", "traces"),
        ("文档", "notes"),
        ("术语", "glossary"),
    ] {
        println!("  {:<8} {}", label, store.count(table)?);
    }
    Ok(())
}
