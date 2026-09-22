//! File discovery.

use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Directories that never contain source worth indexing.
const SKIP_DIRS: &[&str] = &[
    ".git", ".codeatlas", ".codegraph", ".idea", ".vscode", "target", "build", "out",
    "node_modules", "dist", ".mvn", ".settings", "logs",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Java,
    Xml,
    Properties,
    Yaml,
    Sql,
}

impl Lang {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Xml => "xml",
            Self::Properties => "properties",
            Self::Yaml => "yaml",
            Self::Sql => "sql",
        }
    }

    fn of_path(p: &Path) -> Option<Self> {
        match p.extension()?.to_str()? {
            "java" => Some(Self::Java),
            "xml" => Some(Self::Xml),
            "properties" => Some(Self::Properties),
            "yml" | "yaml" => Some(Self::Yaml),
            "sql" => Some(Self::Sql),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    /// Path relative to the project root, with `/` separators.
    pub rel: String,
    pub lang: Lang,
}

/// Every indexable file under `root`, in stable order.
pub fn scan(root: &Path) -> Vec<Found> {
    let mut out: Vec<Found> = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            // Skip build output and VCS metadata, but never the root itself.
            !(e.depth() > 0 && e.file_type().is_dir() && SKIP_DIRS.contains(&name.as_ref()))
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let lang = Lang::of_path(e.path())?;
            let rel = e
                .path()
                .strip_prefix(root)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            Some(Found { path: e.path().to_path_buf(), rel, lang })
        })
        .collect();
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_sources_and_skips_build_dirs() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("src/main/java/a")).unwrap();
        std::fs::create_dir_all(r.join("target/classes")).unwrap();
        std::fs::create_dir_all(r.join(".git")).unwrap();
        std::fs::write(r.join("src/main/java/a/A.java"), "class A{}").unwrap();
        std::fs::write(r.join("pom.xml"), "<project/>").unwrap();
        std::fs::write(r.join("src/main/java/a/x.properties"), "k=v").unwrap();
        std::fs::write(r.join("target/classes/B.java"), "class B{}").unwrap();
        std::fs::write(r.join(".git/C.java"), "class C{}").unwrap();
        std::fs::write(r.join("logo.png"), "x").unwrap();

        let found = scan(r);
        let rels: Vec<&str> = found.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(
            rels,
            vec!["pom.xml", "src/main/java/a/A.java", "src/main/java/a/x.properties"]
        );
        assert_eq!(found[1].lang, Lang::Java);
    }

    #[test]
    fn scans_a_directory_named_like_a_skip_dir_at_root() {
        // A project whose own root directory is called `build` must still index.
        let d = tempfile::tempdir().unwrap();
        let r = d.path().join("build");
        std::fs::create_dir_all(&r).unwrap();
        std::fs::write(r.join("A.java"), "class A{}").unwrap();
        assert_eq!(scan(&r).len(), 1);
    }
}
