//! Java source extraction via tree-sitter.
//!
//! Pulls out type/member declarations with their Javadoc and annotations, plus
//! a best-effort list of callee names per method body. Name resolution happens
//! later in `extract::resolve`, where the whole project's symbol table exists.

use crate::store::model::{Annotation, JavaFile, RawSymbol, SymbolKind};
use crate::util::clean_javadoc;
use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

pub fn parser() -> Result<Parser> {
    let mut p = Parser::new();
    p.set_language(&tree_sitter_java::LANGUAGE.into())
        .context("loading tree-sitter-java grammar")?;
    Ok(p)
}

pub fn extract(src: &str, parser: &mut Parser) -> Result<JavaFile> {
    let tree = parser.parse(src, None).context("parsing java source")?;
    let root = tree.root_node();
    let mut out = JavaFile { had_parse_error: root.has_error(), ..Default::default() };
    let bytes = src.as_bytes();

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "package_declaration" => {
                out.package = child
                    .named_child(0)
                    .and_then(|n| text(n, bytes))
                    .map(|s| s.to_string());
            }
            "import_declaration" => {
                if let Some(t) = text(child, bytes) {
                    let t = t
                        .trim_start_matches("import")
                        .trim()
                        .trim_start_matches("static")
                        .trim()
                        .trim_end_matches(';')
                        .trim();
                    if !t.is_empty() {
                        out.imports.push(t.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    let pkg = out.package.clone();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if is_type_decl(child.kind()) {
            walk_type(child, bytes, pkg.as_deref(), None, &mut out.symbols);
        }
    }
    Ok(out)
}

fn is_type_decl(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    )
}

fn text<'a>(node: Node, bytes: &'a [u8]) -> Option<&'a str> {
    std::str::from_utf8(&bytes[node.byte_range()]).ok()
}

fn line(node: Node) -> u32 {
    node.start_position().row as u32 + 1
}

fn end_line(node: Node) -> u32 {
    node.end_position().row as u32 + 1
}

/// Javadoc or line comments immediately preceding a declaration. Legacy code in
/// scope uses both styles, and `//` comments there often carry the only
/// business description available, so we accept them too.
fn leading_doc(node: Node, bytes: &[u8]) -> Option<String> {
    let mut collected: Vec<String> = Vec::new();
    let mut cur = node.prev_sibling();
    // Annotations sit between the comment and the declaration in the modifiers
    // node, so the comment is usually the immediate previous sibling.
    while let Some(n) = cur {
        match n.kind() {
            "block_comment" | "line_comment" => {
                if let Some(t) = text(n, bytes) {
                    collected.push(t.to_string());
                }
                cur = n.prev_sibling();
            }
            "modifiers" => cur = n.prev_sibling(),
            _ => break,
        }
    }
    if collected.is_empty() {
        return None;
    }
    collected.reverse();
    clean_javadoc(&collected.join("\n"))
}

fn visibility(node: Node, bytes: &[u8]) -> Option<String> {
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        if ch.kind() == "modifiers" {
            let t = text(ch, bytes)?;
            for kw in ["public", "protected", "private"] {
                if t.split_whitespace().any(|w| w == kw) {
                    return Some(kw.to_string());
                }
            }
            return Some("package".to_string());
        }
    }
    None
}

fn is_static_final(node: Node, bytes: &[u8]) -> bool {
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        if ch.kind() == "modifiers" {
            if let Some(t) = text(ch, bytes) {
                let words: Vec<&str> = t.split_whitespace().collect();
                return words.contains(&"static") && words.contains(&"final");
            }
        }
    }
    false
}

/// Names in a type's `extends` and `implements` clauses, generics stripped.
fn supertypes_of(node: Node, bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        match ch.kind() {
            // `extends Base` on a class.
            "superclass" => collect_type_names(ch, bytes, &mut out),
            // `implements A, B` on a class; `extends A, B` on an interface.
            "super_interfaces" | "extends_interfaces" => {
                collect_type_names(ch, bytes, &mut out)
            }
            _ => {}
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Type identifiers under a node, ignoring generic arguments.
fn collect_type_names(node: Node, bytes: &[u8], out: &mut Vec<String>) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "type_identifier" => {
                if let Some(t) = text(n, bytes) {
                    out.push(t.to_string());
                }
                // Do not descend: a generic argument is not a supertype.
                continue;
            }
            "scoped_type_identifier" => {
                if let Some(t) = text(n, bytes) {
                    // Keep only the last segment to match how types are named
                    // elsewhere; resolution maps simple names to FQNs.
                    let last = t.rsplit('.').next().unwrap_or(t);
                    out.push(last.trim().to_string());
                }
                continue;
            }
            "type_arguments" => continue,
            _ => {}
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
}

fn annotations(node: Node, bytes: &[u8]) -> Vec<Annotation> {
    let mut out = Vec::new();
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        if ch.kind() != "modifiers" {
            continue;
        }
        let mut mc = ch.walk();
        for m in ch.children(&mut mc) {
            match m.kind() {
                "marker_annotation" => {
                    if let Some(name) = m.child_by_field_name("name").and_then(|n| text(n, bytes)) {
                        out.push(Annotation { name: name.to_string(), args: Vec::new() });
                    }
                }
                "annotation" => {
                    let Some(name) =
                        m.child_by_field_name("name").and_then(|n| text(n, bytes))
                    else {
                        continue;
                    };
                    let mut args = Vec::new();
                    if let Some(list) = m.child_by_field_name("arguments") {
                        let mut ac = list.walk();
                        for a in list.children(&mut ac) {
                            match a.kind() {
                                "element_value_pair" => {
                                    let k = a
                                        .child_by_field_name("key")
                                        .and_then(|n| text(n, bytes))
                                        .unwrap_or("value");
                                    let v = a
                                        .child_by_field_name("value")
                                        .and_then(|n| text(n, bytes))
                                        .unwrap_or("");
                                    args.push((k.to_string(), unquote(v)));
                                }
                                "(" | ")" | "," => {}
                                other if !other.is_empty() => {
                                    // Positional single argument: @X("v") / @X({"a","b"}).
                                    if let Some(v) = text(a, bytes) {
                                        args.push(("value".to_string(), unquote(v)));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    out.push(Annotation { name: name.to_string(), args });
                }
                _ => {}
            }
        }
    }
    out
}

/// Strip surrounding quotes and braces from an annotation argument's raw text.
fn unquote(raw: &str) -> String {
    let t = raw.trim();
    let t = t.strip_prefix('{').map(|s| s.trim_end_matches('}')).unwrap_or(t);
    let t = t.trim();
    t.trim_matches('"').trim().to_string()
}

fn type_kind(kind: &str) -> SymbolKind {
    match kind {
        "interface_declaration" => SymbolKind::Interface,
        "enum_declaration" => SymbolKind::Enum,
        "record_declaration" => SymbolKind::Record,
        "annotation_type_declaration" => SymbolKind::Annotation,
        _ => SymbolKind::Class,
    }
}

/// Record a type declaration and everything nested inside it.
fn walk_type(
    node: Node,
    bytes: &[u8],
    pkg: Option<&str>,
    parent: Option<usize>,
    out: &mut Vec<RawSymbol>,
) {
    let Some(name) = node.child_by_field_name("name").and_then(|n| text(n, bytes)) else {
        return;
    };
    let kind = type_kind(node.kind());
    let supertypes = supertypes_of(node, bytes);
    // Nested types get an Outer.Inner FQN so they stay distinguishable.
    let fqn = match (pkg, parent.and_then(|p| out.get(p)).and_then(|s| s.fqn.clone())) {
        (_, Some(outer)) => Some(format!("{outer}.{name}")),
        (Some(p), None) => Some(format!("{p}.{name}")),
        (None, None) => Some(name.to_string()),
    };

    out.push(RawSymbol {
        kind,
        name: name.to_string(),
        fqn: fqn.clone(),
        signature: node
            .child_by_field_name("superclass")
            .and_then(|n| text(n, bytes))
            .map(|s| s.trim().to_string()),
        start_line: line(node),
        end_line: end_line(node),
        doc: leading_doc(node, bytes),
        visibility: visibility(node, bytes),
        parent,
        annotations: annotations(node, bytes),
        calls: Vec::new(),
        supertypes,
    });
    let self_idx = out.len() - 1;

    let Some(body) = node
        .child_by_field_name("body")
        .or_else(|| node.child_by_field_name("interface_body"))
    else {
        return;
    };
    let mut c = body.walk();
    for member in body.children(&mut c) {
        match member.kind() {
            k if is_type_decl(k) => walk_type(member, bytes, pkg, Some(self_idx), out),
            "method_declaration" => walk_method(member, bytes, self_idx, out, &fqn),
            "constructor_declaration" => walk_method(member, bytes, self_idx, out, &fqn),
            "field_declaration" => walk_field(member, bytes, self_idx, out, &fqn),
            "enum_body_declarations" => {
                let mut ec = member.walk();
                for m in member.children(&mut ec) {
                    match m.kind() {
                        "method_declaration" | "constructor_declaration" => {
                            walk_method(m, bytes, self_idx, out, &fqn)
                        }
                        "field_declaration" => walk_field(m, bytes, self_idx, out, &fqn),
                        k if is_type_decl(k) => walk_type(m, bytes, pkg, Some(self_idx), out),
                        _ => {}
                    }
                }
            }
            "enum_constant" => {
                if let Some(cn) = member.child_by_field_name("name").and_then(|n| text(n, bytes)) {
                    out.push(RawSymbol {
                        kind: SymbolKind::EnumMember,
                        name: cn.to_string(),
                        fqn: fqn.as_ref().map(|f| format!("{f}.{cn}")),
                        signature: member
                            .child_by_field_name("arguments")
                            .and_then(|n| text(n, bytes))
                            .map(|s| s.to_string()),
                        start_line: line(member),
                        end_line: end_line(member),
                        doc: leading_doc(member, bytes),
                        visibility: None,
                        parent: Some(self_idx),
                        annotations: Vec::new(),
                        calls: Vec::new(),
                        supertypes: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }
}

fn walk_method(
    node: Node,
    bytes: &[u8],
    parent: usize,
    out: &mut Vec<RawSymbol>,
    owner_fqn: &Option<String>,
) {
    let Some(name) = node.child_by_field_name("name").and_then(|n| text(n, bytes)) else {
        return;
    };
    let params = node
        .child_by_field_name("parameters")
        .and_then(|n| text(n, bytes))
        .unwrap_or("()");
    let ret = node.child_by_field_name("type").and_then(|n| text(n, bytes)).unwrap_or("");
    let signature = if ret.is_empty() {
        format!("{name}{params}")
    } else {
        format!("{ret} {name}{params}")
    };
    let mut calls = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(body, bytes, &mut calls);
    }
    calls.sort_unstable();
    calls.dedup();

    out.push(RawSymbol {
        kind: SymbolKind::Method,
        name: name.to_string(),
        fqn: owner_fqn.as_ref().map(|f| format!("{f}#{name}")),
        signature: Some(signature),
        start_line: line(node),
        end_line: end_line(node),
        doc: leading_doc(node, bytes),
        visibility: visibility(node, bytes),
        parent: Some(parent),
        annotations: annotations(node, bytes),
        calls,
        supertypes: Vec::new(),
    });
}

fn walk_field(
    node: Node,
    bytes: &[u8],
    parent: usize,
    out: &mut Vec<RawSymbol>,
    owner_fqn: &Option<String>,
) {
    let ty = node.child_by_field_name("type").and_then(|n| text(n, bytes)).unwrap_or("");
    // `static final` fields are the constant tables these projects use instead
    // of enums, so they get their own kind and are surfaced prominently.
    let kind = if is_static_final(node, bytes) { SymbolKind::Constant } else { SymbolKind::Field };
    let doc = leading_doc(node, bytes);
    let vis = visibility(node, bytes);
    let annots = annotations(node, bytes);

    let mut c = node.walk();
    for decl in node.children(&mut c) {
        if decl.kind() != "variable_declarator" {
            continue;
        }
        let Some(name) = decl.child_by_field_name("name").and_then(|n| text(n, bytes)) else {
            continue;
        };
        let init = decl.child_by_field_name("value").and_then(|n| text(n, bytes));
        out.push(RawSymbol {
            kind,
            name: name.to_string(),
            fqn: owner_fqn.as_ref().map(|f| format!("{f}#{name}")),
            signature: match init {
                Some(v) => Some(format!("{ty} {name} = {v}")),
                None => Some(format!("{ty} {name}")),
            },
            start_line: line(decl),
            end_line: end_line(decl),
            doc: doc.clone(),
            visibility: vis.clone(),
            parent: Some(parent),
            annotations: annots.clone(),
            calls: Vec::new(),
            supertypes: Vec::new(),
        });
    }
}

/// Collect callee names from a method body. We keep the receiver when it is a
/// simple identifier (`couponService.query` → `couponService.query`) so the
/// resolver can use field types; bare calls keep just the method name.
fn collect_calls(node: Node, bytes: &[u8], out: &mut Vec<String>) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "method_invocation" {
            let name = n.child_by_field_name("name").and_then(|x| text(x, bytes));
            let obj = n.child_by_field_name("object").and_then(|x| text(x, bytes));
            if let Some(name) = name {
                match obj {
                    Some(o) if o.len() <= 64 && !o.contains('\n') && !o.contains('(') => {
                        out.push(format!("{o}.{name}"))
                    }
                    _ => out.push(name.to_string()),
                }
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> JavaFile {
        extract(src, &mut parser().unwrap()).unwrap()
    }

    #[test]
    fn enum_members_keep_their_comments() {
        let f = parse(
            r#"package com.yaoex.promotion.core.model;
/**
 * 活动类型枚举类
 */
public enum PromotionTypeEnum {
    /** 特价 */
    BARGAIN,
    /** 满减 */
    FULL_SUB;
}"#,
        );
        assert!(!f.had_parse_error);
        assert_eq!(f.package.as_deref(), Some("com.yaoex.promotion.core.model"));
        let e = f.symbols.iter().find(|s| s.kind == SymbolKind::Enum).unwrap();
        assert_eq!(e.name, "PromotionTypeEnum");
        assert_eq!(e.doc.as_deref(), Some("活动类型枚举类"));
        let members: Vec<_> =
            f.symbols.iter().filter(|s| s.kind == SymbolKind::EnumMember).collect();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].doc.as_deref(), Some("特价"));
        assert_eq!(
            members[0].fqn.as_deref(),
            Some("com.yaoex.promotion.core.model.PromotionTypeEnum.BARGAIN")
        );
    }

    #[test]
    fn static_final_fields_become_constants_with_docs() {
        let f = parse(
            r#"package p;
public class ConstantPromotion {
    /**
     * 促销活动商态0：“生效中”
     */
    public static final Integer PROMOTION_STATE_EFFECTIVE = 0;
    private String name;
}"#,
        );
        let c = f.symbols.iter().find(|s| s.kind == SymbolKind::Constant).unwrap();
        assert_eq!(c.name, "PROMOTION_STATE_EFFECTIVE");
        assert_eq!(c.doc.as_deref(), Some("促销活动商态0：“生效中”"));
        assert!(c.signature.as_deref().unwrap().contains("= 0"));
        assert!(f.symbols.iter().any(|s| s.kind == SymbolKind::Field && s.name == "name"));
    }

    #[test]
    fn annotations_capture_table_and_route() {
        let f = parse(
            r#"package p;
@TableName(value = "t_combined_price")
public class A {}
"#,
        );
        let a = f.symbols.iter().find(|s| s.name == "A").unwrap();
        let t = a.annotations.iter().find(|x| x.name == "TableName").unwrap();
        assert_eq!(t.value(), Some("t_combined_price"));

        let f = parse(
            r#"package p;
@TableName("t_group_buying")
class B {}
"#,
        );
        let b = f.symbols.iter().find(|s| s.name == "B").unwrap();
        assert_eq!(b.annotations[0].value(), Some("t_group_buying"));
    }

    #[test]
    fn methods_get_signature_docs_and_calls() {
        let f = parse(
            r#"package p;
public class S {
    /**
     * 获取特价活动信息
     * @param id 活动ID
     */
    public ResultModel<List<P>> getProductPromotion(Long id, boolean flag) {
        promotionDao.selectById(id);
        return build(id);
    }
}"#,
        );
        let m = f.symbols.iter().find(|s| s.kind == SymbolKind::Method).unwrap();
        assert_eq!(m.name, "getProductPromotion");
        assert_eq!(m.fqn.as_deref(), Some("p.S#getProductPromotion"));
        assert!(m.signature.as_deref().unwrap().starts_with("ResultModel<List<P>> getProductPromotion("));
        assert!(m.doc.as_deref().unwrap().contains("获取特价活动信息"));
        assert!(m.calls.contains(&"promotionDao.selectById".to_string()));
        assert!(m.calls.contains(&"build".to_string()));
    }

    #[test]
    fn interface_methods_and_overloads_are_separate_symbols() {
        let f = parse(
            r#"package p;
/** 促销服务化接口 */
public interface PromotionDubboService {
    /** 取活动 A */
    ResultModel get(List<X> a, String b);
    /** 取活动 B */
    ResultModel get(List<X> a, String b, boolean c);
}"#,
        );
        assert!(f.symbols.iter().any(|s| s.kind == SymbolKind::Interface));
        let ms: Vec<_> = f.symbols.iter().filter(|s| s.kind == SymbolKind::Method).collect();
        assert_eq!(ms.len(), 2);
        assert_ne!(ms[0].signature, ms[1].signature);
        assert_eq!(ms[1].doc.as_deref(), Some("取活动 B"));
    }

    #[test]
    fn nested_types_get_qualified_fqn() {
        let f = parse("package p;\nclass Outer { static class Inner { void go() {} } }");
        assert!(f.symbols.iter().any(|s| s.fqn.as_deref() == Some("p.Outer.Inner")));
        assert!(f.symbols.iter().any(|s| s.fqn.as_deref() == Some("p.Outer.Inner#go")));
    }

    #[test]
    fn imports_normalize_static_and_wildcard() {
        let f = parse(
            "package p;\nimport java.util.*;\nimport static a.b.C.MENU;\nclass A {}",
        );
        assert!(f.imports.contains(&"java.util.*".to_string()));
        assert!(f.imports.contains(&"a.b.C.MENU".to_string()));
    }

    #[test]
    fn line_comments_count_as_docs() {
        let f = parse("package p;\nclass A {\n  // 活动状态\n  private int st;\n}");
        let s = f.symbols.iter().find(|s| s.name == "st").unwrap();
        assert_eq!(s.doc.as_deref(), Some("活动状态"));
    }
}
