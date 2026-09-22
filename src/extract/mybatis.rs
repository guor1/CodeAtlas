//! MyBatis mapper XML extraction.
//!
//! In the legacy projects we target there is no DDL anywhere, so mapper XML is
//! the only description of the schema. Two things come out of each file:
//!   * `<resultMap>` gives column ↔ Java property pairs — the closest thing to a
//!     column dictionary that exists.
//!   * statement bodies give (table, operation) pairs attributed to the DAO
//!     method named by the statement id.

use crate::extract::sql;
use crate::extract::xmlutil::{attrs as attrs_of, is_text, local, reader, text_of};
use crate::store::model::TableOp;
use anyhow::Result;
use quick_xml::events::Event;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct ResultMapping {
    pub column: String,
    pub property: String,
    pub is_pk: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ResultMapDef {
    pub id: String,
    /// The PO/entity class this result map targets.
    pub type_fqn: Option<String>,
    pub mappings: Vec<ResultMapping>,
}

#[derive(Debug, Clone)]
pub struct Statement {
    pub id: String,
    /// select / insert / update / delete as written in the XML tag.
    pub tag: String,
    pub result_map: Option<String>,
    pub result_type: Option<String>,
    pub tables: Vec<(String, TableOp)>,
}

#[derive(Debug, Clone, Default)]
pub struct MapperFile {
    /// The DAO interface FQN this mapper implements.
    pub namespace: Option<String>,
    pub result_maps: Vec<ResultMapDef>,
    pub statements: Vec<Statement>,
    /// Reusable `<sql id="...">` fragments, already inlined where referenced.
    pub fragments: BTreeMap<String, String>,
}

impl MapperFile {
    /// Every distinct table this mapper touches, with the union of operations.
    pub fn tables(&self) -> Vec<(String, TableOp)> {
        let mut out: Vec<(String, TableOp)> = Vec::new();
        for s in &self.statements {
            for t in &s.tables {
                if !out.contains(t) {
                    out.push(t.clone());
                }
            }
        }
        out
    }

    /// The table a result map most plausibly describes: the one its statements
    /// read through that map. Used to attach columns to a table.
    pub fn table_for_result_map(&self, map_id: &str) -> Option<String> {
        // Prefer a statement that names this map and reads exactly one table.
        let mut fallback = None;
        for s in &self.statements {
            if s.result_map.as_deref() != Some(map_id) {
                continue;
            }
            let reads: Vec<&String> = s
                .tables
                .iter()
                .filter(|(_, op)| *op == TableOp::Select)
                .map(|(t, _)| t)
                .collect();
            if reads.len() == 1 {
                return Some(reads[0].clone());
            }
            if fallback.is_none() {
                fallback = reads.first().map(|t| (*t).clone());
            }
        }
        fallback
    }
}

const STATEMENT_TAGS: &[&str] = &["select", "insert", "update", "delete"];

pub fn parse(xml: &str) -> Result<MapperFile> {
    let mut out = MapperFile::default();
    let mut rd = reader(xml);

    // State for the element currently being accumulated.
    let mut cur_stmt: Option<Statement> = None;
    let mut cur_body = String::new();
    let mut cur_map: Option<ResultMapDef> = None;
    let mut cur_fragment: Option<String> = None;
    let mut depth_in_stmt = 0usize;

    loop {
        match rd.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Empty(e)) => {
                // Self-closing elements: only `<id>`/`<result>`/`<include>` carry
                // information, and none of them open a scope.
                let name = local(&e);
                let attrs = attrs_of(&e);
                match name.as_str() {
                    "id" | "result" => {
                        if let Some(m) = cur_map.as_mut() {
                            if let (Some(col), Some(prop)) =
                                (attrs.get("column"), attrs.get("property"))
                            {
                                m.mappings.push(ResultMapping {
                                    column: col.to_ascii_lowercase(),
                                    property: prop.clone(),
                                    is_pk: name == "id",
                                });
                            }
                        }
                    }
                    "include" => {
                        if let Some(rid) = attrs.get("refid") {
                            if let Some(frag) = out.fragments.get(rid) {
                                cur_body.push(' ');
                                cur_body.push_str(frag);
                                cur_body.push(' ');
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Start(e)) => {
                let name = local(&e);
                let attrs = attrs_of(&e);
                match name.as_str() {
                    "mapper" => out.namespace = attrs.get("namespace").cloned(),
                    "resultMap" => {
                        cur_map = Some(ResultMapDef {
                            id: attrs.get("id").cloned().unwrap_or_default(),
                            type_fqn: attrs.get("type").cloned(),
                            mappings: Vec::new(),
                        });
                    }
                    "id" | "result" => {
                        if let Some(m) = cur_map.as_mut() {
                            if let (Some(col), Some(prop)) =
                                (attrs.get("column"), attrs.get("property"))
                            {
                                m.mappings.push(ResultMapping {
                                    column: col.to_ascii_lowercase(),
                                    property: prop.clone(),
                                    is_pk: name == "id",
                                });
                            }
                        }
                    }
                    "sql" => {
                        cur_fragment = attrs.get("id").cloned();
                        cur_body.clear();
                    }
                    t if STATEMENT_TAGS.contains(&t) => {
                        if cur_stmt.is_some() {
                            // A nested <select> inside dynamic SQL: keep it as body text.
                            depth_in_stmt += 1;
                        } else {
                            cur_stmt = Some(Statement {
                                id: attrs.get("id").cloned().unwrap_or_default(),
                                tag: t.to_string(),
                                result_map: attrs.get("resultMap").cloned(),
                                result_type: attrs.get("resultType").cloned(),
                                tables: Vec::new(),
                            });
                            cur_body.clear();
                        }
                    }
                    "include" => {
                        // Inline the referenced fragment so its tables are seen.
                        if let Some(rid) = attrs.get("refid") {
                            if let Some(frag) = out.fragments.get(rid) {
                                cur_body.push(' ');
                                cur_body.push_str(frag);
                                cur_body.push(' ');
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let raw = e.name().as_ref().to_string();
                let name = raw.rsplit(':').next().unwrap_or(&raw).to_string();
                match name.as_str() {
                    "resultMap" => {
                        if let Some(m) = cur_map.take() {
                            out.result_maps.push(m);
                        }
                    }
                    "sql" => {
                        if let Some(id) = cur_fragment.take() {
                            out.fragments.insert(id, std::mem::take(&mut cur_body));
                        }
                    }
                    t if STATEMENT_TAGS.contains(&t) => {
                        if depth_in_stmt > 0 {
                            depth_in_stmt -= 1;
                        } else if let Some(mut s) = cur_stmt.take() {
                            s.tables = sql::tables_with_ops(&cur_body);
                            // A statement's own tag is authoritative for writes:
                            // `<insert>` bodies sometimes read tables too.
                            out.statements.push(s);
                            cur_body.clear();
                        }
                    }
                    _ => {}
                }
            }
            Ok(ev) if is_text(&ev) => {
                if cur_stmt.is_some() || cur_fragment.is_some() {
                    if let Some(s) = text_of(&ev) {
                        cur_body.push_str(&s);
                    }
                }
            }
            Ok(_) => {}
            // Legacy mappers contain malformed islands; keep whatever parsed.
            Err(_) => break,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE mapper PUBLIC "-//mybatis.org//DTD Mapper 3.0//EN" "http://mybatis.org/dtd/mybatis-3-mapper.dtd">
<mapper namespace="com.yaoex.promotion.persistence.interfaces.defective.DefectiveDao">
    <resultMap type="com.yaoex.promotion.model.defective.DefectivePO" id="DefectiveMap">
        <id property="id" column="id"/>
        <result property="enterpriseId" column="enterprise_id"/>
        <result property="promotionName" column="promotion_name"/>
    </resultMap>
    <sql id="Base_Column_List">id, enterprise_id, promotion_name</sql>
    <select id="queryPromotionPage" resultMap="DefectiveMap">
        SELECT <include refid="Base_Column_List"/>
        FROM t_promotion_defective t1
        <if test="groupCode != null">
            left join t_promotion_defective_group t2 on t1.id=t2.promotion_id
        </if>
        <where>
            <if test="startTime == null and endTime !=null">
                <![CDATA[and t1.begin_time<=#{endTime} ]]>
            </if>
        </where>
    </select>
    <insert id="save">insert into t_promotion_defective (enterprise_id) values (#{enterpriseId})</insert>
    <update id="cancel">update t_promotion_defective set status = 1 where id = #{id}</update>
</mapper>"#;

    #[test]
    fn reads_namespace_result_map_and_pk() {
        let m = parse(SAMPLE).unwrap();
        assert_eq!(
            m.namespace.as_deref(),
            Some("com.yaoex.promotion.persistence.interfaces.defective.DefectiveDao")
        );
        assert_eq!(m.result_maps.len(), 1);
        let rm = &m.result_maps[0];
        assert_eq!(rm.id, "DefectiveMap");
        assert_eq!(rm.type_fqn.as_deref(), Some("com.yaoex.promotion.model.defective.DefectivePO"));
        assert_eq!(rm.mappings.len(), 3);
        assert!(rm.mappings[0].is_pk);
        assert_eq!(rm.mappings[1].column, "enterprise_id");
        assert_eq!(rm.mappings[1].property, "enterpriseId");
    }

    #[test]
    fn statements_carry_tables_and_ops() {
        let m = parse(SAMPLE).unwrap();
        let ids: Vec<&str> = m.statements.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["queryPromotionPage", "save", "cancel"]);

        let q = &m.statements[0];
        assert_eq!(q.tag, "select");
        assert_eq!(q.result_map.as_deref(), Some("DefectiveMap"));
        let mut names: Vec<&str> = q.tables.iter().map(|(t, _)| t.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["t_promotion_defective", "t_promotion_defective_group"]);

        assert_eq!(
            m.statements[1].tables,
            vec![("t_promotion_defective".to_string(), TableOp::Insert)]
        );
        assert_eq!(
            m.statements[2].tables,
            vec![("t_promotion_defective".to_string(), TableOp::Update)]
        );
    }

    #[test]
    fn result_map_maps_to_its_single_read_table() {
        let m = parse(SAMPLE).unwrap();
        // queryPromotionPage reads two tables, so the single-table rule does not
        // apply and we fall back to the first read table.
        assert_eq!(
            m.table_for_result_map("DefectiveMap").as_deref(),
            Some("t_promotion_defective")
        );
    }

    #[test]
    fn single_table_select_binds_result_map_exactly() {
        let xml = r#"<mapper namespace="a.B">
          <resultMap type="a.P" id="M"><result property="x" column="x"/></resultMap>
          <select id="one" resultMap="M">select x from t_only where id=#{id}</select>
        </mapper>"#;
        let m = parse(xml).unwrap();
        assert_eq!(m.table_for_result_map("M").as_deref(), Some("t_only"));
    }

    #[test]
    fn fragments_are_inlined_so_their_tables_count() {
        let xml = r#"<mapper namespace="a.B">
          <sql id="joins">left join t_extra e on e.id = t.id</sql>
          <select id="q">select * from t_main t <include refid="joins"/></select>
        </mapper>"#;
        let m = parse(xml).unwrap();
        let mut names: Vec<&str> = m.statements[0].tables.iter().map(|(t, _)| t.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["t_extra", "t_main"]);
    }

    #[test]
    fn aggregate_tables_dedupe_across_statements() {
        let m = parse(SAMPLE).unwrap();
        let tables = m.tables();
        let defective: Vec<_> = tables
            .iter()
            .filter(|(t, _)| t == "t_promotion_defective")
            .map(|(_, op)| *op)
            .collect();
        assert!(defective.contains(&TableOp::Select));
        assert!(defective.contains(&TableOp::Insert));
        assert!(defective.contains(&TableOp::Update));
    }
}
