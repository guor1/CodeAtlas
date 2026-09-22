//! Spring XML bean extraction, used to find MQ consumers.
//!
//! These projects wire message listeners in XML with the topic held in a
//! `${placeholder}`, so the listener class alone tells you nothing about which
//! topic it consumes. We pair each listener bean with the topic property of the
//! consumer bean that references it, then resolve placeholders against the
//! properties files.

use crate::extract::xmlutil::{attrs, comment_doc, qname, reader};
use anyhow::Result;
use quick_xml::events::Event;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct SpringBean {
    pub id: Option<String>,
    pub class: Option<String>,
    /// Literal `<property name= value=>` pairs.
    pub props: BTreeMap<String, String>,
    /// `<property name= ref=>` pairs.
    pub refs: BTreeMap<String, String>,
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SpringXml {
    pub beans: Vec<SpringBean>,
    /// `<context:component-scan base-package="...">` entries.
    pub scan_packages: Vec<String>,
}

impl SpringXml {
    /// Topic bindings discovered by joining consumer beans to listener beans.
    ///
    /// Returns `(listener_class, topic_expr)`. `topic_expr` may still be a
    /// `${...}` placeholder for the caller to resolve.
    pub fn mq_bindings(&self) -> Vec<(String, String)> {
        let by_id: BTreeMap<&str, &SpringBean> = self
            .beans
            .iter()
            .filter_map(|b| b.id.as_deref().map(|i| (i, b)))
            .collect();
        let mut out = Vec::new();
        for bean in &self.beans {
            let Some(topic) = bean.props.get("topic") else { continue };
            // The consumer bean points at its listener through a ref property.
            for (_, target) in &bean.refs {
                if let Some(listener) = by_id.get(target.as_str()) {
                    if let Some(class) = listener.class.as_deref() {
                        out.push((class.to_string(), topic.clone()));
                    }
                }
            }
            // Some consumers embed the listener class directly.
            if bean.refs.is_empty() {
                if let Some(class) = bean.class.as_deref() {
                    if class.contains("Listener") {
                        out.push((class.to_string(), topic.clone()));
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

pub fn parse(xml: &str) -> Result<SpringXml> {
    let mut out = SpringXml::default();
    let mut rd = reader(xml);

    let mut cur: Option<SpringBean> = None;
    let mut pending_doc: Option<String> = None;

    loop {
        match rd.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Comment(c)) => {
                pending_doc = comment_doc(&c);
            }
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = qname(&e);
                let attrs = attrs(&e);
                match name.as_str() {
                    "bean" => {
                        // Nested beans replace the outer one; we only need the
                        // flat property view, not the nesting structure.
                        if let Some(b) = cur.take() {
                            out.beans.push(b);
                        }
                        cur = Some(SpringBean {
                            id: attrs.get("id").or_else(|| attrs.get("name")).cloned(),
                            class: attrs.get("class").cloned(),
                            props: BTreeMap::new(),
                            refs: BTreeMap::new(),
                            doc: pending_doc.take(),
                        });
                    }
                    "property" => {
                        if let (Some(b), Some(name)) = (cur.as_mut(), attrs.get("name")) {
                            if let Some(v) = attrs.get("value") {
                                b.props.insert(name.clone(), v.clone());
                            } else if let Some(r) = attrs.get("ref") {
                                b.refs.insert(name.clone(), r.clone());
                            }
                        }
                    }
                    "context:component-scan" => {
                        if let Some(p) = attrs.get("base-package") {
                            out.scan_packages.push(p.clone());
                        }
                        pending_doc = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == "bean" {
                    if let Some(b) = cur.take() {
                        out.beans.push(b);
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    if let Some(b) = cur.take() {
        out.beans.push(b);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_listener_class_with_consumer_topic() {
        let xml = r#"<beans>
  <bean id="couponAuditListener" class="com.yaoex.promotion.service.listener.CouponAuditOnsMessageListener">
    <property name="type" value="ons"/>
  </bean>
  <bean id="couponAuditConsumer" class="com.yaoex.mq.Consumer">
    <property name="topic" value="${couponAuditConsumer.mq.topic}"/>
    <property name="messageListener" ref="couponAuditListener"/>
  </bean>
</beans>"#;
        let s = parse(xml).unwrap();
        assert_eq!(s.beans.len(), 2);
        let b = s.mq_bindings();
        assert_eq!(
            b,
            vec![(
                "com.yaoex.promotion.service.listener.CouponAuditOnsMessageListener".to_string(),
                "${couponAuditConsumer.mq.topic}".to_string()
            )]
        );
    }

    #[test]
    fn reads_component_scan_packages() {
        let xml = r#"<beans xmlns:context="http://www.springframework.org/schema/context">
  <!-- 配置01、JobHandler 扫描路径 -->
  <context:component-scan base-package="com.yaoex.promotion.job"/>
</beans>"#;
        let s = parse(xml).unwrap();
        assert_eq!(s.scan_packages, vec!["com.yaoex.promotion.job"]);
    }

    #[test]
    fn captures_bean_comment_as_doc() {
        let xml = r#"<beans>
  <!--天降红包数量重置-->
  <bean id="redPacketJobService" class="com.x.RedPacketJobServiceImpl"/>
</beans>"#;
        let s = parse(xml).unwrap();
        assert_eq!(s.beans[0].doc.as_deref(), Some("天降红包数量重置"));
    }
}
