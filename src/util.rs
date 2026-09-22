//! Small shared helpers.

use sha2::{Digest, Sha256};

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

/// Stable digest over an ordered set of strings, used for staleness detection.
/// The separator keeps `["ab","c"]` from colliding with `["a","bc"]`.
pub fn digest_parts<'a, I: IntoIterator<Item = &'a str>>(parts: I) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    hex(&h.finalize())
}

/// Conservative token estimate. CJK-heavy text costs far more tokens per byte
/// than ASCII, so we deliberately under-divide rather than risk blowing a budget.
pub fn estimate_tokens(text: &str) -> usize {
    (text.len() as f64 / 2.5).ceil() as usize
}

/// Normalize a domain-ish identifier: camelCase and separators to snake_case.
pub fn to_snake(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 4);
    for (i, &ch) in chars.iter().enumerate() {
        if matches!(ch, '-' | '_' | ' ' | '.') {
            if !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            continue;
        }
        if ch.is_ascii_uppercase() {
            let prev = i.checked_sub(1).and_then(|j| chars.get(j)).copied();
            let next = chars.get(i + 1).copied();
            let after_lower = prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit());
            // Break an acronym before its last letter: the `S` in `DTOService`
            // starts a new word, but the `T` and `O` in `DTO` do not.
            let acronym_end = prev.is_some_and(|p| p.is_ascii_uppercase())
                && next.is_some_and(|n| n.is_ascii_lowercase());
            if (after_lower || acronym_end) && !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out.trim_matches('_').to_string()
}

/// Strip Javadoc decoration, keeping the prose and `@tag` lines.
pub fn clean_javadoc(raw: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        let t = t.strip_prefix("/**").unwrap_or(t);
        let t = t.strip_prefix("/*").unwrap_or(t);
        let t = t.strip_suffix("*/").unwrap_or(t);
        let t = t.trim_start();
        let t = t.strip_prefix("* ").or_else(|| t.strip_prefix('*')).unwrap_or(t);
        let t = t.strip_prefix("//").unwrap_or(t);
        let t = t.trim();
        if !t.is_empty() {
            lines.push(t.to_string());
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// True if the text contains CJK characters — a good proxy for "this comment
/// carries real business meaning" in the legacy projects we target.
pub fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// Truncate to a character budget on a line boundary where possible.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    match cut.rfind('\n') {
        Some(i) if i > max / 2 => format!("{}\n…（已截断）", &cut[..i]),
        _ => format!("{cut}…（已截断）"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_handles_camel_and_separators() {
        assert_eq!(to_snake("groupBuying"), "group_buying");
        assert_eq!(to_snake("buyTogether"), "buy_together");
        assert_eq!(to_snake("t_coupon_template"), "t_coupon_template");
        assert_eq!(to_snake("ICouponDubboService"), "i_coupon_dubbo_service");
    }

    #[test]
    fn javadoc_keeps_prose_and_tags() {
        let raw = "/**\n * 促销服务化接口\n * @param id 活动ID\n */";
        assert_eq!(
            clean_javadoc(raw).unwrap(),
            "促销服务化接口\n@param id 活动ID"
        );
    }

    #[test]
    fn javadoc_empty_is_none() {
        assert!(clean_javadoc("/**\n *\n */").is_none());
    }

    #[test]
    fn cjk_detection() {
        assert!(has_cjk("特价活动"));
        assert!(!has_cjk("bargain promotion"));
    }
}
