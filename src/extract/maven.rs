//! Maven POM extraction: module tree and dependency coordinates.

use crate::extract::xmlutil::{local, reader, text_of};
use anyhow::Result;
use quick_xml::events::Event;

#[derive(Debug, Clone, Default)]
pub struct Pom {
    pub artifact_id: Option<String>,
    pub group_id: Option<String>,
    pub version: Option<String>,
    pub packaging: Option<String>,
    /// Relative paths declared in `<modules>`.
    pub modules: Vec<String>,
    /// `group:artifact` of declared dependencies.
    pub dependencies: Vec<String>,
}

pub fn parse(xml: &str) -> Result<Pom> {
    let mut out = Pom::default();
    let mut rd = reader(xml);

    // Element path, so we can tell `/project/artifactId` from a dependency's.
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    // Coordinates of the dependency currently being read.
    let mut dep_group: Option<String> = None;
    let mut dep_artifact: Option<String> = None;

    loop {
        match rd.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                path.push(local(&e));
                text.clear();
            }
            Ok(ev @ Event::Text(_)) => {
                if let Some(s) = text_of(&ev) {
                    text.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let raw = e.name().as_ref().to_string();
                let name = raw.rsplit(':').next().unwrap_or(&raw).to_string();
                let val = text.trim().to_string();
                // The immediate parent element disambiguates identically named
                // tags: `<project>`'s own coordinates vs a `<dependency>`'s vs an
                // `<exclusion>`'s wildcards.
                let parent = path.get(path.len().saturating_sub(2)).map(String::as_str);
                let in_dependency = parent == Some("dependency");
                let at_project_root = parent == Some("project");
                match name.as_str() {
                    "artifactId" if at_project_root => out.artifact_id = Some(val.clone()),
                    "groupId" if at_project_root => out.group_id = Some(val.clone()),
                    "version" if at_project_root => out.version = Some(val.clone()),
                    "packaging" if at_project_root => out.packaging = Some(val.clone()),
                    "module" => {
                        if !val.is_empty() {
                            out.modules.push(val.clone());
                        }
                    }
                    "groupId" if in_dependency => dep_group = Some(val.clone()),
                    "artifactId" if in_dependency => dep_artifact = Some(val.clone()),
                    "dependency" => {
                        if let (Some(g), Some(a)) = (dep_group.take(), dep_artifact.take()) {
                            out.dependencies.push(format!("{g}:{a}"));
                        }
                    }
                    _ => {}
                }
                text.clear();
                path.pop();
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out.dependencies.sort();
    out.dependencies.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_aggregator_modules_and_own_coordinates() {
        let xml = r#"<project>
  <groupId>com.yaoex</groupId>
  <artifactId>promotion</artifactId>
  <version>1.2-SNAPSHOT</version>
  <packaging>pom</packaging>
  <modules>
    <module>promotion-common</module>
    <module>promotion-model</module>
  </modules>
</project>"#;
        let p = parse(xml).unwrap();
        assert_eq!(p.artifact_id.as_deref(), Some("promotion"));
        assert_eq!(p.group_id.as_deref(), Some("com.yaoex"));
        assert_eq!(p.packaging.as_deref(), Some("pom"));
        assert_eq!(p.modules, vec!["promotion-common", "promotion-model"]);
    }

    #[test]
    fn dependency_coordinates_do_not_shadow_project_ones() {
        let xml = r#"<project>
  <artifactId>promotion-service</artifactId>
  <dependencies>
    <dependency>
      <groupId>org.apache.dubbo</groupId>
      <artifactId>dubbo</artifactId>
      <version>2.7.8</version>
    </dependency>
    <dependency>
      <groupId>com.baomidou</groupId>
      <artifactId>mybatis-plus</artifactId>
      <version>2.3.3</version>
    </dependency>
  </dependencies>
</project>"#;
        let p = parse(xml).unwrap();
        assert_eq!(p.artifact_id.as_deref(), Some("promotion-service"));
        assert_eq!(
            p.dependencies,
            vec!["com.baomidou:mybatis-plus", "org.apache.dubbo:dubbo"]
        );
    }

    #[test]
    fn ignores_exclusion_coordinates() {
        let xml = r#"<project>
  <artifactId>a</artifactId>
  <dependencies>
    <dependency>
      <groupId>g</groupId><artifactId>a1</artifactId>
      <exclusions><exclusion><groupId>*</groupId><artifactId>*</artifactId></exclusion></exclusions>
    </dependency>
  </dependencies>
</project>"#;
        let p = parse(xml).unwrap();
        // The exclusion's `*:*` must not become a dependency of its own; the
        // parent dependency is still recorded.
        assert!(p.dependencies.contains(&"g:a1".to_string()));
        assert!(!p.dependencies.iter().any(|d| d.contains('*')));
    }
}
