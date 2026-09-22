//! Table-name and operation extraction from raw SQL text.
//!
//! MyBatis XML bodies are SQL with `<if>`/`<foreach>` fragments interleaved, so
//! a real SQL parser is the wrong tool: it would reject most statements. We scan
//! for the keywords that introduce table names instead, which is robust against
//! the dynamic-SQL noise and against the vendor dialects legacy projects use.

use crate::store::model::TableOp;
use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

static RE_FROM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\bfrom\s+([`\w.]+)").unwrap()
});
static RE_JOIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\bjoin\s+([`\w.]+)").unwrap()
});
static RE_INSERT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\binsert\s+(?:ignore\s+)?into\s+([`\w.]+)").unwrap()
});
// `ON DUPLICATE KEY UPDATE col = ...` is an upsert clause, not a statement: the
// identifier after `UPDATE` is a column. MySQL upserts are everywhere in these
// mappers, so without this guard every upserted column becomes a phantom table.
static RE_UPDATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)(?:\bon\s+duplicate\s+key\s+)?\bupdate\s+([`\w.]+)").unwrap()
});
/// Matches only the upsert form, so its range can be claimed and skipped.
static RE_DUP_UPDATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\bon\s+duplicate\s+key\s+update\s+([`\w.]+)").unwrap()
});
static RE_DELETE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\bdelete\s+from\s+([`\w.]+)").unwrap()
});
static RE_REPLACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\breplace\s+into\s+([`\w.]+)").unwrap()
});

/// Words that appear where a table name would but are not tables.
const NOT_TABLES: &[&str] = &[
    "select", "dual", "information_schema", "values", "set", "where", "table",
];

/// Schemas whose contents are engine metadata rather than business data.
const META_SCHEMAS: &[&str] = &["information_schema", "performance_schema", "mysql", "sys"];

/// Normalize a captured token into a bare table name, or reject it.
fn normalize(raw: &str) -> Option<String> {
    let t = raw.trim().trim_matches('`').trim();
    // Reject engine catalogs outright: `information_schema.tables` is a probe of
    // the database, not a table this project owns.
    if let Some((schema, _)) = t.split_once('.') {
        if META_SCHEMAS.contains(&schema.trim_matches('`').to_ascii_lowercase().as_str()) {
            return None;
        }
    }
    // Strip a schema qualifier: `db.t_coupon` → `t_coupon`.
    let t = t.rsplit('.').next().unwrap_or(t);
    let t = t.trim_matches('`');
    if t.is_empty() || t.len() < 2 {
        return None;
    }
    // MyBatis placeholders and fragment references are never table names.
    if t.starts_with('#') || t.starts_with('$') || t.contains('{') {
        return None;
    }
    if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    if t.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if NOT_TABLES.contains(&lower.as_str()) {
        return None;
    }
    Some(lower)
}

/// Every (table, operation) pair referenced by a SQL body.
///
/// Write keywords are matched first and claim their byte range, so the `from` in
/// `DELETE FROM t` cannot also register `t` as a read. A table genuinely both
/// read and written by one statement — `UPDATE a SET x=(SELECT .. FROM a)` —
/// still gets both operations, because the second match sits elsewhere.
pub fn tables_with_ops(sql: &str) -> Vec<(String, TableOp)> {
    let mut out: BTreeSet<(String, TableOp)> = BTreeSet::new();
    // Byte ranges already consumed by a higher-precedence keyword.
    let mut claimed: Vec<(usize, usize)> = Vec::new();

    // Claim upsert clauses first so the plain-UPDATE pattern cannot read their
    // column name as a table.
    for c in RE_DUP_UPDATE.captures_iter(sql) {
        let whole = c.get(0).expect("group 0 always present");
        claimed.push((whole.start(), whole.end()));
    }

    for (re, op) in [
        (&*RE_INSERT, TableOp::Insert),
        (&*RE_REPLACE, TableOp::Insert),
        (&*RE_DELETE, TableOp::Delete),
        (&*RE_UPDATE, TableOp::Update),
        (&*RE_FROM, TableOp::Select),
        (&*RE_JOIN, TableOp::Select),
    ] {
        for c in re.captures_iter(sql) {
            let whole = c.get(0).expect("group 0 always present");
            if claimed.iter().any(|&(s, e)| whole.start() < e && s < whole.end()) {
                continue;
            }
            if let Some(t) = normalize(&c[1]) {
                claimed.push((whole.start(), whole.end()));
                out.insert((t, op));
            }
        }
    }
    out.into_iter().collect()
}

impl PartialOrd for TableOp {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TableOp {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(sql: &str) -> Vec<String> {
        let mut v: Vec<String> = tables_with_ops(sql).into_iter().map(|(t, _)| t).collect();
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn handles_dynamic_sql_with_joins() {
        let sql = r#"
        SELECT t1.* FROM t_promotion_defective t1
        left join t_promotion_defective_group t2 on t1.id=t2.promotion_id
        <where><if test="enterpriseId != null">and t1.enterprise_id=#{enterpriseId}</if></where>"#;
        assert_eq!(names(sql), vec!["t_promotion_defective", "t_promotion_defective_group"]);
    }

    #[test]
    fn classifies_operations() {
        let ops = tables_with_ops("insert into t_coupon (a) values (1)");
        assert_eq!(ops, vec![("t_coupon".into(), TableOp::Insert)]);
        let ops = tables_with_ops("update t_coupon set a = 1");
        assert_eq!(ops, vec![("t_coupon".into(), TableOp::Update)]);
        let ops = tables_with_ops("delete from t_coupon where id = 1");
        assert_eq!(ops, vec![("t_coupon".into(), TableOp::Delete)]);
    }

    #[test]
    fn strips_backticks_and_schema() {
        assert_eq!(names("select * from `promotion`.`t_coupon`"), vec!["t_coupon"]);
    }

    #[test]
    fn rejects_placeholders_and_noise() {
        assert!(names("select * from ${tableName}").is_empty());
        assert!(names("select * from #{t}").is_empty());
        assert!(names("select 1 from dual").is_empty());
        assert!(names("select * from information_schema.tables")
            .iter()
            .all(|t| t != "information_schema"));
    }

    #[test]
    fn upsert_clause_columns_are_not_tables() {
        // The identifier after `ON DUPLICATE KEY UPDATE` is a column.
        let sql = "insert into t_real (a, b) values (1, 2)
                   ON DUPLICATE KEY UPDATE
                   gross_profit = VALUES(gross_profit),
                   update_time = NOW()";
        assert_eq!(names(sql), vec!["t_real"]);

        // Same on one line, and with the lowercase spelling the mappers use.
        assert_eq!(
            names("insert into t_x (a) values (1) on duplicate key update pic_url = 1"),
            vec!["t_x"]
        );
    }

    #[test]
    fn real_update_statements_still_register() {
        assert_eq!(names("update t_coupon set status = 1"), vec!["t_coupon"]);
        // An upsert must not suppress a genuine UPDATE elsewhere in the body.
        let mut n = names(
            "update t_a set x = 1; insert into t_b (c) values (2) on duplicate key update c = 3",
        );
        n.sort();
        assert_eq!(n, vec!["t_a", "t_b"]);
    }

    #[test]
    fn engine_catalogs_are_rejected() {
        assert!(names("select count(*) from information_schema.tables").is_empty());
        assert!(names("select * from mysql.user").is_empty());
        // A business table that merely lives in a named schema is still kept.
        assert_eq!(names("select * from promotion.t_coupon"), vec!["t_coupon"]);
    }

    #[test]
    fn insert_ignore_and_replace_are_writes() {
        assert_eq!(
            tables_with_ops("insert ignore into t_x (a) values (1)"),
            vec![("t_x".into(), TableOp::Insert)]
        );
        assert_eq!(
            tables_with_ops("replace into t_y (a) values (1)"),
            vec![("t_y".into(), TableOp::Insert)]
        );
    }
}
