//! Java `.properties` parsing and `${placeholder}` resolution.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

static RE_PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([^}:]+)(?::([^}]*))?\}").unwrap());

/// Parse `key=value` / `key:value` lines, honoring comments and line continuations.
pub fn parse(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut pending = String::new();
    for raw in text.lines() {
        let line = raw.trim_start();
        if pending.is_empty() && (line.is_empty() || line.starts_with('#') || line.starts_with('!'))
        {
            continue;
        }
        let joined = if pending.is_empty() {
            line.to_string()
        } else {
            format!("{pending}{}", line)
        };
        // A trailing backslash continues onto the next line.
        if let Some(stripped) = joined.strip_suffix('\\') {
            pending = stripped.to_string();
            continue;
        }
        pending.clear();
        let Some(idx) = joined.find(['=', ':']) else { continue };
        let key = joined[..idx].trim();
        let value = joined[idx + 1..].trim();
        if !key.is_empty() {
            out.insert(key.to_string(), value.to_string());
        }
    }
    out
}

/// Substitute `${key}` references using `props`, honoring `${key:default}`.
/// Unresolvable placeholders are left verbatim so the raw config stays visible.
pub fn resolve(expr: &str, props: &BTreeMap<String, String>) -> String {
    let mut cur = expr.to_string();
    // Bounded to stop placeholder cycles in hand-written legacy configs.
    for _ in 0..8 {
        if !cur.contains("${") {
            break;
        }
        let next = RE_PLACEHOLDER
            .replace_all(&cur, |c: &regex::Captures| {
                let key = c[1].trim();
                match props.get(key) {
                    Some(v) => v.clone(),
                    None => match c.get(2) {
                        Some(d) => d.as_str().to_string(),
                        None => c[0].to_string(),
                    },
                }
            })
            .to_string();
        if next == cur {
            break;
        }
        cur = next;
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_and_both_separators() {
        let p = parse("# comment\n! also\nxxl.job.admin.addresses=http://a\nfoo : bar\n\n");
        assert_eq!(p.get("xxl.job.admin.addresses").unwrap(), "http://a");
        assert_eq!(p.get("foo").unwrap(), "bar");
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn honors_line_continuation() {
        // Per the `.properties` format, leading whitespace on a continuation
        // line is part of the indentation, not of the value.
        let p = parse("topics=a,\\\n b,\\\n c");
        assert_eq!(p.get("topics").unwrap(), "a,b,c");
    }

    #[test]
    fn value_may_contain_separators() {
        let p = parse("url=jdbc:mysql://h:3306/db?x=1");
        assert_eq!(p.get("url").unwrap(), "jdbc:mysql://h:3306/db?x=1");
    }

    #[test]
    fn resolves_placeholders_with_defaults() {
        let mut props = BTreeMap::new();
        props.insert("couponAudit.mq.topic".to_string(), "TOPIC_COUPON_AUDIT".to_string());
        assert_eq!(resolve("${couponAudit.mq.topic}", &props), "TOPIC_COUPON_AUDIT");
        assert_eq!(resolve("${missing:FALLBACK}", &props), "FALLBACK");
        // Unknown and without a default: keep it visible rather than blanking it.
        assert_eq!(resolve("${unknown.topic}", &props), "${unknown.topic}");
    }

    #[test]
    fn resolves_nested_references() {
        let mut props = BTreeMap::new();
        props.insert("a".to_string(), "${b}".to_string());
        props.insert("b".to_string(), "final".to_string());
        assert_eq!(resolve("${a}", &props), "final");
    }

    #[test]
    fn placeholder_cycle_terminates() {
        let mut props = BTreeMap::new();
        props.insert("a".to_string(), "${b}".to_string());
        props.insert("b".to_string(), "${a}".to_string());
        // Must not hang; the exact residue does not matter.
        let _ = resolve("${a}", &props);
    }
}
