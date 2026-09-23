//! Git history signals.
//!
//! Commit subjects and branch names are the highest-value domain vocabulary in
//! the projects we target: they are written in the team's own language and
//! describe intent, which code identifiers rarely do. Churn identifies which
//! files actually matter today, independent of how large they are.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub authored_at: String,
    pub subject: String,
    /// Repo-relative paths touched, as reported by git.
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileHistory {
    pub commit_count: u32,
    pub first_commit_at: Option<String>,
    pub last_commit_at: Option<String>,
}

pub fn is_repo(root: &Path) -> bool {
    root.join(".git").exists()
}

/// The current `HEAD` commit, the key the git cache hangs off.
///
/// Git history is a pure function of `HEAD` (ignoring remote branch pointers),
/// so a `HEAD` that has not moved means the two expensive walks — [`log`] and
/// [`file_history`] — can be replayed from cache instead of re-run.
pub fn head(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "HEAD"]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
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
    Ok(parse_commits(&raw))
}

/// Commits strictly newer than `old` and reachable from `new` (`old..new`).
///
/// This is the incremental slice `sync` fetches after a commit lands, instead of
/// re-walking the whole history. Unbounded — a long-un-synced tree may yield many
/// commits, but still far fewer than full history.
pub fn log_range(root: &Path, old: &str, new: &str) -> Result<Vec<Commit>> {
    let raw = git(
        root,
        &[
            "log",
            &format!("{old}..{new}"),
            "--no-merges",
            "--name-only",
            "--date=iso-strict",
            "--pretty=format:%x1e%H%x1f%ad%x1f%s%x1f",
        ],
    )?;
    Ok(parse_commits(&raw))
}

/// True when `old` is an ancestor of (or equal to) `new`.
///
/// The guard for incremental ingestion: folding `old..new` into a snapshot taken
/// at `old` is only sound when `old` is on `new`'s first-parent line. After a
/// rebase or force-push this is false, and the caller falls back to a full walk.
pub fn is_ancestor(root: &Path, old: &str, new: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "--is-ancestor", old, new])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Parse the `%x1e … %x1f …` record stream [`log`] and [`log_range`] share.
///
/// A record separator (rather than newlines) lets us parse subjects that
/// themselves contain newlines.
fn parse_commits(raw: &str) -> Vec<Commit> {
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
    commits
}

/// Fold commits from `old..new` into a cached full-history snapshot.
///
/// `hist`/`commits` are the state up to `old`; `inc` is `old..new` (newest
/// first). The result is byte-for-byte what a full walk at `new` would produce,
/// without walking the old history again.
pub fn apply_incremental(
    hist: &mut BTreeMap<String, FileHistory>,
    commits: &mut Vec<Commit>,
    inc: &[Commit],
    limit: usize,
) {
    // Subject list: every commit in `inc` is newer than everything cached, so it
    // goes in front; the cached tail is already the newest-`limit` of old history,
    // so truncating after the merge keeps the newest-`limit` of the combined tree.
    let mut merged = Vec::with_capacity(inc.len() + commits.len());
    merged.extend(inc.iter().cloned());
    merged.append(commits);
    merged.truncate(limit);
    *commits = merged;

    // Churn: count touches per file in `inc`, tracking the newest and oldest date
    // in the slice. `inc` is newest-first, so the first sighting is the newest and
    // the last is the oldest.
    let mut delta: BTreeMap<String, (String, String, u32)> = BTreeMap::new();
    for c in inc {
        let date = &c.authored_at;
        for f in &c.files {
            let e = delta
                .entry(f.clone())
                .or_insert_with(|| (date.clone(), date.clone(), 0));
            e.1 = date.clone(); // overwritten each pass → oldest wins
            e.2 += 1;
        }
    }
    for (path, (newest, oldest, count)) in delta {
        let h = hist.entry(path).or_default();
        h.commit_count += count;
        // New commits are newer than anything cached, so the newest sighting is
        // the file's new last touch.
        h.last_commit_at = Some(newest);
        // A file with no cached entry first appears in this slice; its oldest
        // sighting is its true first touch.
        if h.first_commit_at.is_none() {
            h.first_commit_at = Some(oldest);
        }
    }
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

    #[test]
    fn head_matches_the_latest_commit() {
        let dir = fixture();
        let h = head(dir.path()).unwrap();
        assert_eq!(h.len(), 40);
        // `head` is the newest commit, so its subject is the last message.
        let commits = log(dir.path(), 50).unwrap();
        assert_eq!(h, commits[0].sha);
    }

    #[test]
    fn head_is_none_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(head(dir.path()).is_none());
    }

    #[test]
    fn log_range_returns_only_new_commits() {
        let dir = fixture();
        let full = log(dir.path(), 50).unwrap();
        assert_eq!(full.len(), 2);
        let old = &full[1].sha; // oldest
        let new = &full[0].sha; // newest
        let range = log_range(dir.path(), old, new).unwrap();
        assert_eq!(range.len(), 1);
        assert_eq!(range[0].subject, "优惠券模板优化");
    }

    #[test]
    fn is_ancestor_detects_rebased_history() {
        let dir = fixture();
        let full = log(dir.path(), 50).unwrap();
        assert!(is_ancestor(dir.path(), &full[1].sha, &full[0].sha));
        // A commit is an ancestor of itself.
        assert!(is_ancestor(dir.path(), &full[0].sha, &full[0].sha));
        // A newer commit is not an ancestor of an older one.
        assert!(!is_ancestor(dir.path(), &full[0].sha, &full[1].sha));
    }

    #[test]
    fn apply_incremental_folds_new_commits_into_snapshot() {
        // Cached snapshot at HEAD=A: one commit touching a.java.
        let mut hist: BTreeMap<String, FileHistory> = BTreeMap::new();
        hist.insert(
            "a.java".into(),
            FileHistory {
                commit_count: 1,
                first_commit_at: Some("2020-01-01".into()),
                last_commit_at: Some("2020-01-01".into()),
            },
        );
        let mut commits: Vec<Commit> = vec![Commit {
            sha: "oldsha".into(),
            authored_at: "2020-01-01".into(),
            subject: "初始".into(),
            files: vec!["a.java".into()],
        }];

        // New commits since A (newest first).
        let inc = vec![
            Commit {
                sha: "new2".into(),
                authored_at: "2020-03-01".into(),
                subject: "改 a".into(),
                files: vec!["a.java".into()],
            },
            Commit {
                sha: "new1".into(),
                authored_at: "2020-02-01".into(),
                subject: "加 b".into(),
                files: vec!["b.java".into()],
            },
        ];

        apply_incremental(&mut hist, &mut commits, &inc, 4000);

        // Subjects: new commits first, then cached.
        assert_eq!(commits.len(), 3);
        assert_eq!(commits[0].subject, "改 a");
        assert_eq!(commits[1].subject, "加 b");
        assert_eq!(commits[2].subject, "初始");

        // Churn: a.java gained one touch; last_commit_at advanced.
        let a = &hist["a.java"];
        assert_eq!(a.commit_count, 2);
        assert_eq!(a.first_commit_at.as_deref(), Some("2020-01-01"));
        assert_eq!(a.last_commit_at.as_deref(), Some("2020-03-01"));
        // b.java is brand new: both bounds come from the slice.
        let b = &hist["b.java"];
        assert_eq!(b.commit_count, 1);
        assert_eq!(b.first_commit_at.as_deref(), Some("2020-02-01"));
        assert_eq!(b.last_commit_at.as_deref(), Some("2020-02-01"));
    }

    #[test]
    fn apply_incremental_truncates_to_limit() {
        let mut commits: Vec<Commit> = vec![Commit {
            sha: "old".into(),
            authored_at: "2020-01-01".into(),
            subject: "旧".into(),
            files: vec![],
        }];
        let inc = (0..5)
            .map(|i| Commit {
                sha: format!("n{i}"),
                authored_at: "2020-01-01".into(),
                subject: format!("新{i}"),
                files: vec![],
            })
            .collect::<Vec<_>>();
        let mut hist = BTreeMap::new();
        apply_incremental(&mut hist, &mut commits, &inc, 3);
        assert_eq!(commits.len(), 3);
        // Newest survive; the cached tail fell off.
        assert_eq!(commits[0].subject, "新0");
        assert_eq!(commits[2].subject, "新2");
        assert!(commits.iter().all(|c| c.subject != "旧"));
    }
}
