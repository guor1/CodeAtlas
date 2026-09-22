//! Entrypoint discovery from Java annotations.
//!
//! Dubbo and MQ entrypoints come from XML (see [`super::dubbo_xml`] and
//! [`super::spring`]); HTTP routes and job handlers are annotation-driven and
//! derived here from the symbol table a file produced.

use crate::store::model::{Annotation, JavaFile, RawSymbol, SymbolKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRoute {
    /// `GET`, `POST`, … or `ANY` when the mapping does not constrain the method.
    pub method: String,
    /// Full path, class-level prefix included.
    pub path: String,
    /// Index of the handler method in the file's symbol vector.
    pub symbol: usize,
    /// Permission annotations guarding the route, e.g. `RequireListPermission`.
    pub guards: Vec<String>,
}

const MAPPING_ANNOTATIONS: &[(&str, &str)] = &[
    ("GetMapping", "GET"),
    ("PostMapping", "POST"),
    ("PutMapping", "PUT"),
    ("DeleteMapping", "DELETE"),
    ("PatchMapping", "PATCH"),
    ("RequestMapping", "ANY"),
];

const GUARD_HINTS: &[&str] = &["Permission", "RequiresRoles", "PreAuthorize", "Auth"];

/// The HTTP method and path fragment a mapping annotation declares, if it is one.
fn mapping_of(a: &Annotation) -> Option<(String, String)> {
    let simple = a.name.rsplit('.').next().unwrap_or(&a.name);
    let (_, verb) = MAPPING_ANNOTATIONS.iter().find(|(n, _)| *n == simple)?;
    // `value` and `path` are interchangeable; `method` overrides a bare
    // @RequestMapping's verb.
    let raw = a.arg("value").or_else(|| a.arg("path")).unwrap_or("");
    let method = match a.arg("method") {
        Some(m) => m
            .rsplit('.')
            .next()
            .unwrap_or(m)
            .trim()
            .trim_end_matches('}')
            .to_string(),
        None => verb.to_string(),
    };
    // Multi-path mappings take their first path; the rest are aliases.
    let first = raw.split(',').next().unwrap_or("").trim().trim_matches('"');
    Some((method, first.to_string()))
}

fn join_path(prefix: &str, suffix: &str) -> String {
    let p = prefix.trim().trim_matches('/');
    let s = suffix.trim().trim_matches('/');
    match (p.is_empty(), s.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{s}"),
        (false, true) => format!("/{p}"),
        (false, false) => format!("/{p}/{s}"),
    }
}

fn guards_of(sym: &RawSymbol) -> Vec<String> {
    sym.annotations
        .iter()
        .map(|a| a.name.rsplit('.').next().unwrap_or(&a.name).to_string())
        .filter(|n| GUARD_HINTS.iter().any(|h| n.contains(h)))
        .collect()
}

/// True when a type is a Spring MVC controller.
fn is_controller(sym: &RawSymbol) -> bool {
    sym.annotations.iter().any(|a| {
        let n = a.name.rsplit('.').next().unwrap_or(&a.name);
        n == "Controller" || n == "RestController"
    })
}

/// Every HTTP route declared in a parsed file.
pub fn http_routes(file: &JavaFile) -> Vec<HttpRoute> {
    let mut out = Vec::new();
    for (idx, sym) in file.symbols.iter().enumerate() {
        if !matches!(sym.kind, SymbolKind::Class) || !is_controller(sym) {
            continue;
        }
        let class_prefix = sym
            .annotations
            .iter()
            .find_map(mapping_of)
            .map(|(_, p)| p)
            .unwrap_or_default();
        let class_guards = guards_of(sym);

        for (m_idx, m) in file.symbols.iter().enumerate() {
            if m.kind != SymbolKind::Method || m.parent != Some(idx) {
                continue;
            }
            let Some((method, path)) = m.annotations.iter().find_map(mapping_of) else {
                continue;
            };
            let mut guards = class_guards.clone();
            guards.extend(guards_of(m));
            guards.sort();
            guards.dedup();
            out.push(HttpRoute {
                method,
                path: join_path(&class_prefix, &path),
                symbol: m_idx,
                guards,
            });
        }
    }
    out
}

/// Job handler declared by an XXL-Job annotation, if this file has one.
///
/// Older handlers in these projects carry no annotation at all: they are plain
/// classes in a job package, exported over Dubbo and triggered by the scheduler.
/// Those are recognised by package placement at build time, not here.
pub fn xxl_job_handlers(file: &JavaFile) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (idx, sym) in file.symbols.iter().enumerate() {
        for a in &sym.annotations {
            let n = a.name.rsplit('.').next().unwrap_or(&a.name);
            if n == "XxlJob" || n == "JobHandler" {
                let name = a.value().unwrap_or(&sym.name).to_string();
                out.push((idx, name));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::java;

    fn parse(src: &str) -> JavaFile {
        java::extract(src, &mut java::parser().unwrap()).unwrap()
    }

    #[test]
    fn combines_class_prefix_with_method_paths() {
        let f = parse(
            r#"package p;
@Controller
@RequestMapping("/defective")
@RequireListPermission({MENU_NAME_PROMOTION})
public class DefectiveController {
    @GetMapping("/queryInfo/{promotionId}")
    public Ret queryInfo(Long id) { return null; }

    @RequestMapping(value = "/list", method = RequestMethod.POST)
    public Ret list() { return null; }

    public Ret notARoute() { return null; }
}"#,
        );
        let routes = http_routes(&f);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].method, "GET");
        assert_eq!(routes[0].path, "/defective/queryInfo/{promotionId}");
        assert_eq!(routes[1].method, "POST");
        assert_eq!(routes[1].path, "/defective/list");
        // The class-level permission annotation guards every route inside it.
        assert_eq!(routes[0].guards, vec!["RequireListPermission"]);
    }

    #[test]
    fn bare_request_mapping_is_any_method() {
        let f = parse(
            r#"package p;
@Controller
@RequestMapping("/x")
class C {
    @RequestMapping("/go")
    public String go() { return null; }
}"#,
        );
        let r = http_routes(&f);
        assert_eq!(r[0].method, "ANY");
        assert_eq!(r[0].path, "/x/go");
    }

    #[test]
    fn non_controller_classes_yield_nothing() {
        let f = parse("package p;\n@Service\nclass S { @GetMapping(\"/a\") void a() {} }");
        assert!(http_routes(&f).is_empty());
    }

    #[test]
    fn class_without_prefix_still_routes() {
        let f = parse(
            "package p;\n@RestController\nclass C { @PostMapping(\"/save\") void s() {} }",
        );
        let r = http_routes(&f);
        assert_eq!(r[0].path, "/save");
        assert_eq!(r[0].method, "POST");
    }

    #[test]
    fn multi_path_mapping_takes_first() {
        let f = parse(
            r#"package p;
@Controller
class C { @RequestMapping({"/a", "/b"}) void go() {} }"#,
        );
        assert_eq!(http_routes(&f)[0].path, "/a");
    }

    #[test]
    fn route_symbol_index_points_at_handler() {
        let f = parse(
            "package p;\n@Controller\nclass C { @GetMapping(\"/a\") public void handler() {} }",
        );
        let r = http_routes(&f);
        assert_eq!(f.symbols[r[0].symbol].name, "handler");
    }

    #[test]
    fn finds_annotated_job_handlers() {
        let f = parse(
            r#"package p;
public class J {
    @XxlJob("couponExpireJobHandler")
    public void run() {}
}"#,
        );
        let h = xxl_job_handlers(&f);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].1, "couponExpireJobHandler");
        assert_eq!(f.symbols[h[0].0].name, "run");
    }
}
