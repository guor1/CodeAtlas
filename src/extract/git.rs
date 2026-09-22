//! Git history signals.
//!
//! Commit subjects and branch names are the highest-value domain vocabulary in
//! the projects we target: they are written in the team's own language and
//! describe intent, which code identifiers rarely do. Churn identifies which
//! files actually matter today, independent of how large they are.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub authored_at: String,
    pub subject: String,
    /// Repo-relative paths touched, as reported by git.
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FileHistory {
    pub commit_count: u32,
    pub first_commit_at: Option<String>,
    pub last_commit_at: Option<String>,
}

pub fn is_repo(root: &Path) -> bool {
    root.join(".git").exists()
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    anyhow::ensure!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    // Commit messages are frequently non-UTF-8 in old repositories; keep what
    // decodes rather than failing the whole build.
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Recent commits with the files they touched.
///
/// `limit` bounds the walk: full history on a decade-old repository costs far
/// more than it adds, and the vocabulary signal saturates quickly.
pub fn log(root: &Path, limit: usize) -> Result<Vec<Commit>> {
    // A record separator lets us parse subjects that themselves contain newlines.
    let raw = git(
        root,
        &[
            "log",
            &format!("-{limit}"),
            "--no-merges",
            "--name-only",
            "--date=iso-strict",
            "--pretty=format:%x1e%H%x1f%ad%x1f%s%x1f",
        ],
    )?;

    let mut commits = Vec::new();
    for rec in raw.split('\u{1e}') {
        let rec = rec.trim_start_matches('\n');
        if rec.trim().is_empty() {
            continue;
        }
        let mut parts = rec.split('\u{1f}');
        let (Some(sha), Some(date), Some(subject)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let files = parts
            .next()
            .unwrap_or("")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        commits.push(Commit {
            sha: sha.trim().to_string(),
            authored_at: date.trim().to_string(),
            subject: subject.trim().to_string(),
            files,
        });
    }
    Ok(commits)
}

/// Per-file churn and first/last touch, over the whole history.
///
/// This is a separate, cheaper walk than [`log`] because it needs no subjects
/// and can therefore cover all commits without holding them in memory.
pub fn file_history(root: &Path) -> Result<BTreeMap<String, FileHistory>> {
    let raw = git(
        root,
        &["log", "--name-only", "--date=iso-strict", "--pretty=format:%x1f%ad"],
    )?;
    let mut out: BTreeMap<String, FileHistory> = BTreeMap::new();
    let mut current_date: Option<String> = None;
    for line in raw.lines() {
        if let Some(date) = line.strip_prefix('\u{1f}') {
            current_date = Some(date.trim().to_string());
            continue;
        }
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        let Some(date) = current_date.as_deref() else { continue };
        let e = out.entry(path.to_string()).or_default();
        e.commit_count += 1;
        // git log walks newest first, so the first date seen is the latest.
        if e.last_commit_at.is_none() {
            e.last_commit_at = Some(date.to_string());
        }
        e.first_commit_at = Some(date.to_string());
    }
    Ok(out)
}

/// Local and remote branch names, which in these repositories encode the feature
/// vocabulary of every release ever shipped.
pub fn branches(root: &Path) -> Result<Vec<String>> {
    let raw = git(root, &["branch", "-a", "--format=%(refname:short)"])?;
    let mut out: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains("HEAD"))
        .map(|l| l.trim_start_matches("remotes/").to_string())
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn remote_url(root: &Path) -> Option<String> {
    git(root, &["remote", "get-url", "origin"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway repository so the assertions do not depend on any
    /// particular checkout being present.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let run = |args: &[&str]| {
            let st = Command::new("git").arg("-C").arg(p).args(args).status().unwrap();
            assert!(st.success(), "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        std::fs::write(p.join("a.java"), "class A {}").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "添加特价活动"]);
        std::fs::write(p.join("a.java"), "class A { int x; }").unwrap();
        std::fs::write(p.join("b.java"), "class B {}").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "优惠券模板优化"]);
        dir
    }

    #[test]
    fn reads_subjects_and_touched_files() {
        let dir = fixture();
        let commits = log(dir.path(), 50).unwrap();
        assert_eq!(commits.len(), 2);
        // Newest first.
        assert_eq!(commits[0].subject, "优惠券模板优化");
        assert!(commits[0].files.contains(&"b.java".to_string()));
        assert_eq!(commits[1].subject, "添加特价活动");
        assert!(commits[1].authored_at.starts_with("20"));
    }

    #[test]
    fn churn_counts_and_bounds_dates() {
        let dir = fixture();
        let h = file_history(dir.path()).unwrap();
        assert_eq!(h["a.java"].commit_count, 2);
        assert_eq!(h["b.java"].commit_count, 1);
        let a = &h["a.java"];
        assert!(a.first_commit_at.as_deref().unwrap() <= a.last_commit_at.as_deref().unwrap());
    }

    #[test]
    fn lists_branches_without_head_alias() {
        let dir = fixture();
        let b = branches(dir.path()).unwrap();
        assert!(!b.is_empty());
        assert!(b.iter().all(|n| !n.contains("HEAD")));
    }

    #[test]
    fn non_repo_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_repo(dir.path()));
    }
}
