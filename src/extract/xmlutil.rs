//! Shared helpers over `quick_xml`'s reader.
//!
//! In quick-xml 0.42 the event API is `&str`-based (`QName(pub &'a str)`), while
//! `Reader::from_str` still yields a `Reader<&[u8]>`. These helpers hide that
//! seam and the entity unescaping that mapper bodies need.

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::BTreeMap;

/// A reader configured for the hand-maintained XML found in legacy projects:
/// mismatched end tags tolerated, since a strict parse would abandon files that
/// still hold useful facts.
pub fn reader(xml: &str) -> Reader<&[u8]> {
    let mut r = Reader::from_str(xml);
    let cfg = r.config_mut();
    cfg.check_end_names = false;
    r
}

/// Qualified element name, e.g. `dubbo:service`.
pub fn qname(e: &BytesStart) -> String {
    e.name().as_ref().to_string()
}

/// Element name without its namespace prefix.
pub fn local(e: &BytesStart) -> String {
    let q = qname(e);
    q.rsplit(':').next().unwrap_or(&q).to_string()
}

/// Attributes as a map, with entity references resolved and prefixes stripped.
pub fn attrs(e: &BytesStart) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for a in e.attributes().flatten() {
        let raw = a.key.as_ref();
        let key = raw.rsplit(':').next().unwrap_or(raw).to_string();
        let val = a
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map(|v| v.to_string())
            .unwrap_or_default();
        m.insert(key, val);
    }
    m
}

/// True for the events [`text_of`] can turn into character data.
pub fn is_text(ev: &Event) -> bool {
    matches!(ev, Event::Text(_) | Event::CData(_) | Event::GeneralRef(_))
}

/// Character data of a text, CDATA or entity-reference event.
///
/// quick-xml reports `&lt;` as its own [`Event::GeneralRef`] rather than as part
/// of the surrounding text, so a caller that only handles `Text` would silently
/// drop every `<=` in a mapper's SQL — splitting statements mid-expression and
/// losing table references. Resolving the reference back into the stream keeps
/// bodies intact.
pub fn text_of(ev: &Event) -> Option<String> {
    match ev {
        Event::Text(t) => {
            let raw: &str = t;
            Some(
                quick_xml::escape::unescape(raw)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|_| raw.to_string()),
            )
        }
        // CDATA is literal by definition; unescaping it would be wrong.
        Event::CData(c) => Some(c.as_ref().to_string()),
        Event::GeneralRef(r) => {
            if let Ok(Some(ch)) = r.resolve_char_ref() {
                return Some(ch.to_string());
            }
            let name: &str = r;
            Some(
                quick_xml::escape::resolve_predefined_entity(name)
                    .map(|s| s.to_string())
                    // An unknown entity keeps its source form rather than vanishing.
                    .unwrap_or_else(|| format!("&{name};")),
            )
        }
        _ => None,
    }
}

/// A comment's prose, or `None` when it is commented-out markup rather than
/// documentation. Legacy configs are full of disabled beans, and treating those
/// as a description of the next live element produces confidently wrong docs.
pub fn comment_doc(raw: &str) -> Option<String> {
    if raw.contains('<') {
        return None;
    }
    crate::util::clean_javadoc(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenate every character-data event, the way the body accumulators do.
    fn collect(xml: &str) -> String {
        let mut r = reader(xml);
        let mut out = String::new();
        loop {
            match r.read_event() {
                Ok(Event::Eof) => break,
                Ok(ev) if is_text(&ev) => {
                    if let Some(t) = text_of(&ev) {
                        out.push_str(&t);
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        out
    }

    #[test]
    fn entity_refs_rejoin_the_text_stream() {
        // `&lt;` arrives as its own GeneralRef event; dropping it would corrupt
        // the SQL comparison operators that mapper bodies are full of.
        assert_eq!(collect("<a>and t.begin_time &lt;= 1</a>"), "and t.begin_time <= 1");
        assert_eq!(collect("<a>a &amp;&amp; b</a>"), "a && b");
    }

    #[test]
    fn cdata_is_literal() {
        assert_eq!(collect("<b><![CDATA[y <= 2]]></b>"), "y <= 2");
    }

    #[test]
    fn char_refs_resolve() {
        assert_eq!(collect("<a>&#60;&#x3E;</a>"), "<>");
    }

    #[test]
    fn unknown_entity_keeps_source_form() {
        assert_eq!(collect("<a>&nbsp;</a>"), "&nbsp;");
    }

    #[test]
    fn comment_rejects_markup() {
        assert!(comment_doc("<dubbo:service interface=\"x\"/>").is_none());
        assert_eq!(comment_doc(" 限购活动dubbo接口 ").as_deref(), Some("限购活动dubbo接口"));
    }
}
