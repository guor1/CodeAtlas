//! Maintain a managed block in the project's `CLAUDE.md`.
//!
//! The block is delimited by markers so that hand-written guidance in the same
//! file is never touched. This matters: `CLAUDE.md` is a file the team edits, and
//! a generator that overwrites it wholesale would be abandoned after one
//! surprise.

use crate::store::Store;
use anyhow::{Context, Result};
use rusqlite::params;
use std::fmt::Write as _;
use std::path::Path;

const BEGIN: &str = "<!-- catlas:begin -->";
const END: &str = "<!-- catlas:end -->";

pub fn write(store: &Store, project_id: i64, root: &Path, out_dir: &Path) -> Result<usize> {
    let path = root.join("CLAUDE.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let block = block(store, project_id, root, out_dir)?;
    let merged = splice(&existing, &block);
    std::fs::write(&path, merged).with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(1)
}

/// Replace the managed block, or append it when absent.
fn splice(existing: &str, block: &str) -> String {
    match (existing.find(BEGIN), existing.find(END)) {
        (Some(start), Some(end)) if end > start => {
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..start]);
            out.push_str(block);
            out.push_str(&existing[end + END.len()..]);
            out
        }
        // Malformed or absent markers: append rather than risk clobbering prose.
        _ => {
            let mut out = existing.trim_end().to_string();
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(block);
            out.push('\n');
            out
        }
    }
}

fn block(store: &Store, project_id: i64, root: &Path, out_dir: &Path) -> Result<String> {
    let rel = out_dir
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| out_dir.to_string_lossy().to_string());

    let mut s = String::new();
    writeln!(s, "{BEGIN}")?;
    writeln!(s, "## 业务领域知识库\n")?;
    writeln!(
        s,
        "本项目代码量大、业务分散、技术栈老旧。动手改代码前，**先查知识库再读代码**，\
         能省掉大量在 23 万行里翻找的时间。以下内容由 `catlas` 生成，勿手工编辑。\n"
    )?;

    writeln!(s, "### 查什么看哪里\n")?;
    writeln!(s, "| 问题 | 位置 |")?;
    writeln!(s, "|---|---|")?;
    writeln!(s, "| 项目全貌与领域地图 | `{rel}/README.md` |")?;
    writeln!(s, "| 某领域的职责、状态机、业务规则 | `{rel}/domains/<领域>.md` |")?;
    writeln!(s, "| 表结构与字段含义（项目无 DDL，这是唯一来源） | `{rel}/reference/tables.md` |")?;
    writeln!(s, "| Dubbo 接口与 HTTP 路由 | `{rel}/reference/entrypoints.md` |")?;
    writeln!(s, "| 定时任务、消息消费 | `{rel}/reference/jobs.md`、`{rel}/reference/mq.md` |")?;
    writeln!(s, "| 中文术语对应的代码标识符 | `{rel}/glossary.md` |")?;
    writeln!(s, "| 精确检索（全文） | `catlas query \"<关键词>\"` |")?;
    writeln!(s)?;

    // Listing the domains inline saves an agent one file read on every task.
    let domains: Vec<(String, Option<String>, i64)> = store
        .conn
        .prepare(
            "SELECT d.key, d.label,
                    (SELECT COUNT(*) FROM domain_members m
                     WHERE m.domain_id = d.id AND m.kind = 'file')
             FROM domains d WHERE d.project_id = ?1 ORDER BY 3 DESC LIMIT 20",
        )?
        .query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if !domains.is_empty() {
        writeln!(s, "### 主要业务领域\n")?;
        let list: Vec<String> = domains
            .iter()
            .map(|(key, label, n)| match label {
                Some(l) => format!("`{key}`（{l}，{n} 文件）"),
                None => format!("`{key}`（{n} 文件）"),
            })
            .collect();
        writeln!(s, "{}\n", list.join("、"))?;
    }

    writeln!(s, "### 这个项目的已知坑\n")?;
    writeln!(
        s,
        "- **没有 DDL 文件**。表结构只能从 mapper XML 的 `resultMap` 和 `@TableName` 反推，\
         知识库已经做了这件事，不要再去翻 SQL 找表结构。"
    )?;
    writeln!(
        s,
        "- **服务一律经接口调用**。顺着调用链读代码会在 `XxxService` 接口处断掉，\
         真正的逻辑在 `XxxServiceImpl`。领域文档里的「关键链路」已经把这一跳接上了。"
    )?;
    writeln!(
        s,
        "- **部分 MQ topic 是外部配置**。`reference/mq.md` 里显示为 `${{占位符}}` 的，\
         仓库里查不到实际值，需要去配置中心。"
    )?;
    writeln!(
        s,
        "- **枚举和常量类里的中文注释是业务语义的权威出处**，比方法名可靠得多。"
    )?;
    writeln!(s)?;

    // The MCP tool description alone is too weak a signal: a session asked a
    // question gets an answer and moves on, and the finding evaporates. The
    // CLAUDE.md block is the only place we can tell every future session,
    // up front, that writing conclusions down is part of the job.
    writeln!(s, "### 得出结论后沉淀回来\n")?;
    writeln!(
        s,
        "分析代码得出**已核实**的业务结论时，调 MCP 工具 `propose_insight` 沉淀为候选洞察，\
         需附代码位置作证据。三种类型："
    )?;
    writeln!(s, "- `business_rule` 业务规则 —— 代码实际怎么运作（如：取消订单不释放优惠券）")?;
    writeln!(s, "- `landmine` 坑 —— 容易踩的陷阱（如：核销入口不校验门店归属，漏传即跨门店核销）")?;
    writeln!(s, "- `term` 术语 —— 中文黑话与代码的对应（如：特价活动 = `TEJIA` 枚举）")?;
    writeln!(
        s,
        "确认这件事的依据在代码里、能给出 file/行号 的，才提交；推断、猜测、待办不要提交——\
         这不是笔记工具。提交后成为候选，人工 `catlas review --accept` 确认才进入检索。"
    )?;
    writeln!(s, "\n知识库过期时重新生成：`catlas build && catlas render --claude-md`")?;
    write!(s, "{END}")?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_when_no_markers_present() {
        let out = splice("# 我的项目\n\n手写的说明。\n", "<!-- catlas:begin -->\nB\n<!-- catlas:end -->");
        assert!(out.starts_with("# 我的项目\n\n手写的说明。"));
        assert!(out.contains("catlas:begin"));
    }

    #[test]
    fn replaces_only_the_managed_block() {
        let existing = "前言保留\n\n<!-- catlas:begin -->\n旧内容\n<!-- catlas:end -->\n\n后记保留\n";
        let out = splice(existing, "<!-- catlas:begin -->\n新内容\n<!-- catlas:end -->");
        assert!(out.contains("前言保留"));
        assert!(out.contains("后记保留"));
        assert!(out.contains("新内容"));
        assert!(!out.contains("旧内容"));
    }

    #[test]
    fn repeated_renders_are_idempotent() {
        let block = "<!-- catlas:begin -->\nX\n<!-- catlas:end -->";
        let once = splice("手写\n", block);
        let twice = splice(&once, block);
        assert_eq!(once.trim(), twice.trim());
        assert_eq!(twice.matches(BEGIN).count(), 1);
    }

    #[test]
    fn empty_file_gets_just_the_block() {
        let out = splice("", "<!-- catlas:begin -->\nX\n<!-- catlas:end -->");
        assert_eq!(out.trim(), "<!-- catlas:begin -->\nX\n<!-- catlas:end -->");
    }

    #[test]
    fn malformed_markers_do_not_destroy_content() {
        // END before BEGIN: appending is safer than splicing a bogus range.
        let existing = "重要内容\n<!-- catlas:end -->\n<!-- catlas:begin -->\n";
        let out = splice(existing, "<!-- catlas:begin -->\nX\n<!-- catlas:end -->");
        assert!(out.contains("重要内容"));
    }

    #[test]
    fn block_directs_sessions_to_propose_insights() {
        // A session gets the write-path guidance from the block itself — without
        // this section, findings evaporate with the session.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let pid = store.ensure_project("p", dir.path()).unwrap();
        let out_dir = dir.path().join(".knowledge");
        let b = block(&store, pid, dir.path(), &out_dir).unwrap();
        assert!(b.contains("propose_insight"));
        assert!(b.contains("business_rule"));
        assert!(b.contains("landmine"));
        assert!(b.contains("term"));
    }
}
