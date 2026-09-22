//! Dubbo XML provider/consumer extraction.
//!
//! `<dubbo:service>` elements are the authoritative list of RPC entrypoints,
//! including the ones whose Java interface looks unused. Commented-out services
//! are intentionally skipped: quick-xml reports them as comments, so retired
//! entrypoints never reach the knowledge base.

use crate::extract::xmlutil::{attrs, comment_doc, qname, reader};
use anyhow::Result;
use quick_xml::events::Event;

#[derive(Debug, Clone)]
pub struct DubboService {
    /// The exported interface FQN.
    pub interface: String,
    /// Spring bean id implementing it.
    pub reference: Option<String>,
    pub timeout: Option<String>,
    pub retries: Option<String>,
    pub version: Option<String>,
    pub group: Option<String>,
    /// Comment immediately above the element — in these projects it is often the
    /// only Chinese description of what the service is for.
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DubboReference {
    pub interface: String,
    pub bean_id: Option<String>,
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DubboXml {
    pub services: Vec<DubboService>,
    pub references: Vec<DubboReference>,
}

pub fn parse(xml: &str) -> Result<DubboXml> {
    let mut out = DubboXml::default();
    let mut rd = reader(xml);

    let mut pending_doc: Option<String> = None;
    loop {
        match rd.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Comment(c)) => {
                // A commented-out bean is not documentation.
                pending_doc = comment_doc(&c);
            }
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = qname(&e);
                let attrs = attrs(&e);
                match name.as_str() {
                    "dubbo:service" => {
                        if let Some(iface) = attrs.get("interface") {
                            out.services.push(DubboService {
                                interface: iface.clone(),
                                reference: attrs.get("ref").cloned(),
                                timeout: attrs.get("timeout").cloned(),
                                retries: attrs.get("retries").cloned(),
                                version: attrs.get("version").cloned(),
                                group: attrs.get("group").cloned(),
                                doc: pending_doc.take(),
                            });
                        }
                    }
                    "dubbo:reference" => {
                        if let Some(iface) = attrs.get("interface") {
                            out.references.push(DubboReference {
                                interface: iface.clone(),
                                bean_id: attrs.get("id").cloned(),
                                doc: pending_doc.take(),
                            });
                        }
                    }
                    // Any other element consumes the pending comment so it does
                    // not drift onto a later, unrelated service.
                    _ => {
                        pending_doc = None;
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_services_with_config_and_comment() {
        let xml = r#"<beans xmlns:dubbo="http://dubbo.apache.org/schema/dubbo">
  <dubbo:provider filter="traceFilter" retries="0"/>
  <dubbo:service interface="com.yaoex.promotion.dubbo.service.interfaces.PromotionDubboService" ref="iPromotionDubboService" timeout="3000" retries="0"/>
  <!-- 限购活动dubbo接口 -->
  <dubbo:service interface="com.yaoex.a.IProductLimitBuyDubboService" ref="iProductLimitBuyDubboService" timeout="3000" retries="0"/>
</beans>"#;
        let d = parse(xml).unwrap();
        assert_eq!(d.services.len(), 2);
        assert_eq!(d.services[0].reference.as_deref(), Some("iPromotionDubboService"));
        assert_eq!(d.services[0].timeout.as_deref(), Some("3000"));
        assert!(d.services[0].doc.is_none());
        assert_eq!(d.services[1].doc.as_deref(), Some("限购活动dubbo接口"));
    }

    #[test]
    fn skips_commented_out_services() {
        let xml = r#"<beans xmlns:dubbo="http://dubbo.apache.org/schema/dubbo">
  <!--<dubbo:service interface="com.yaoex.a.IPinTuanDubboManageService" ref="x" timeout="50000"/>-->
  <dubbo:service interface="com.yaoex.a.Live" ref="y"/>
</beans>"#;
        let d = parse(xml).unwrap();
        assert_eq!(d.services.len(), 1);
        assert_eq!(d.services[0].interface, "com.yaoex.a.Live");
        // The commented-out XML must not be mistaken for the live service's doc.
        assert!(d.services[0].doc.is_none());
    }

    #[test]
    fn extracts_consumer_references() {
        let xml = r#"<beans xmlns:dubbo="http://dubbo.apache.org/schema/dubbo">
  <!--用户中心-->
  <dubbo:reference id="userService" interface="com.yaoex.user.UserService"/>
</beans>"#;
        let d = parse(xml).unwrap();
        assert_eq!(d.references.len(), 1);
        assert_eq!(d.references[0].bean_id.as_deref(), Some("userService"));
        assert_eq!(d.references[0].doc.as_deref(), Some("用户中心"));
    }
}
