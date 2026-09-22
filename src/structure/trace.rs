//! Entrypoint traces.
//!
//! For each entrypoint, walk the call graph until reaching code that touches a
//! table. The resulting path plus table set is what makes a capability legible:
//! "this Dubbo method ends up writing `t_coupon_enterprise`" is the single most
//! useful fact about it, and nothing in the source states it directly.

use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// How far to follow calls from an entrypoint. Legacy service methods nest
/// deeply, but past this depth paths are dominated by shared utilities and stop
/// describing the capability.
pub const MAX_DEPTH: usize = 6;

/// Cap on paths recorded per entrypoint, so one fan-out-heavy controller cannot
/// dominate the database.
const MAX_PATHS_PER_ENTRY: usize = 24;

/// One path from an entrypoint to table-touching code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracePath {
    /// Symbol ids from the entrypoint to the terminal symbol.
    pub symbols: Vec<i64>,
    /// Human-readable FQNs for the same hops.
    pub labels: Vec<String>,
    /// Lowest confidence along the path: how much to trust the whole chain.
    pub min_confidence: f64,
}

struct Graph {
    /// src symbol → (dst symbol, confidence).
    edges: BTreeMap<i64, Vec<(i64, f64)>>,
    /// Symbol → tables it accesses, with the operation.
    tables: BTreeMap<i64, Vec<(i64, String)>>,
    /// Symbol → FQN or name, for labels.
    names: BTreeMap<i64, String>,
    /// Class symbol → its method symbols, to enter a type's behaviour.
    members: BTreeMap<i64, Vec<i64>>,
}

fn load_graph(store: &Store, project_id: i64) -> Result<Graph> {
    let mut g = Graph {
        edges: BTreeMap::new(),
        tables: BTreeMap::new(),
        names: BTreeMap::new(),
        members: BTreeMap::new(),
    };

    // Only trust edges the resolver was reasonably sure about: a 0.3 edge is a
    // guess from a unique method name and would invent plausible-looking paths.
    let mut stmt = store.conn.prepare(
        "SELECT src_symbol_id, dst_symbol_id, confidence FROM refs
         WHERE project_id = ?1 AND resolved = 1 AND confidence >= 0.6",
    )?;
    for row in stmt.query_map(params![project_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?))
    })? {
        let (s, d, c) = row?;
        g.edges.entry(s).or_default().push((d, c));
    }

    let mut stmt = store.conn.prepare(
        "SELECT ta.symbol_id, ta.table_id, ta.op FROM table_access ta
         WHERE ta.project_id = ?1 AND ta.symbol_id IS NOT NULL",
    )?;
    for row in stmt.query_map(params![project_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
    })? {
        let (sym, table, op) = row?;
        g.tables.entry(sym).or_default().push((table, op));
    }

    let mut stmt = store.conn.prepare(
        "SELECT id, COALESCE(fqn, name), kind, parent_id FROM symbols WHERE project_id = ?1",
    )?;
    for row in stmt.query_map(params![project_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?,
        ))
    })? {
        let (id, label, kind, parent) = row?;
        g.names.insert(id, label);
        if kind == "method" {
            if let Some(p) = parent {
                g.members.entry(p).or_default().push(id);
            }
        }
    }
    Ok(g)
}

/// Compute and store traces for every entrypoint.
pub fn build(store: &Store, project_id: i64) -> Result<usize> {
    let g = load_graph(store, project_id)?;

    let entries: Vec<(i64, Option<i64>)> = store
        .conn
        .prepare("SELECT id, symbol_id FROM entrypoints WHERE project_id = ?1")?
        .query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let tx = store.conn.unchecked_transaction()?;
    let mut count = 0usize;
    {
        let mut ins = tx.prepare(
            "INSERT INTO traces(project_id, entrypoint_id, path_json, depth, tables_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (entry_id, symbol_id) in entries {
            let Some(sym) = symbol_id else { continue };
            // A Dubbo entrypoint points at an interface, not a method; expand it
            // to the methods it declares so the walk has somewhere to start.
            let roots: Vec<i64> = match g.members.get(&sym) {
                Some(methods) if !methods.is_empty() => methods.clone(),
                _ => vec![sym],
            };
            for root in roots {
                for path in walk(&g, root) {
                    let tables = tables_on(&g, &path.symbols);
                    ins.execute(params![
                        project_id,
                        entry_id,
                        serde_json::to_string(&path)?,
                        path.symbols.len() as i64,
                        serde_json::to_string(&tables)?,
                    ])?;
                    count += 1;
                }
            }
        }
    }
    tx.commit()?;
    Ok(count)
}

/// Table ids and operations reachable along a path.
fn tables_on(g: &Graph, symbols: &[i64]) -> BTreeMap<i64, BTreeSet<String>> {
    let mut out: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for s in symbols {
        for (table, op) in g.tables.get(s).into_iter().flatten() {
            out.entry(*table).or_default().insert(op.clone());
        }
    }
    out
}

/// Breadth-first walk from `root`, recording paths that reach table access.
///
/// Breadth-first rather than depth-first so that the shortest route to each
/// table is the one reported: that is the path a reader should follow.
fn walk(g: &Graph, root: i64) -> Vec<TracePath> {
    let mut out = Vec::new();
    // Tables already explained by a shorter path from this root.
    let mut covered: BTreeSet<i64> = BTreeSet::new();
    let mut seen: BTreeSet<i64> = BTreeSet::from([root]);
    let mut queue: VecDeque<(Vec<i64>, f64)> = VecDeque::from([(vec![root], 1.0)]);

    while let Some((path, conf)) = queue.pop_front() {
        if out.len() >= MAX_PATHS_PER_ENTRY {
            break;
        }
        let tip = *path.last().expect("path is never empty");

        // Reaching table access terminates this branch: deeper hops would be
        // inside the persistence layer, not part of the business flow.
        let hits: Vec<i64> = g
            .tables
            .get(&tip)
            .into_iter()
            .flatten()
            .map(|(t, _)| *t)
            .filter(|t| !covered.contains(t))
            .collect();
        if !hits.is_empty() {
            covered.extend(&hits);
            out.push(TracePath {
                labels: path.iter().map(|s| g.names.get(s).cloned().unwrap_or_default()).collect(),
                symbols: path,
                min_confidence: conf,
            });
            continue;
        }

        if path.len() >= MAX_DEPTH {
            continue;
        }
        for (dst, c) in g.edges.get(&tip).into_iter().flatten() {
            // `seen` is per-root, so a shared helper is expanded once per walk.
            if !seen.insert(*dst) {
                continue;
            }
            let mut next = path.clone();
            next.push(*dst);
            queue.push_back((next, conf.min(*c)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a → b → c, where c writes table 7; plus an unrelated leaf d.
    fn graph() -> Graph {
        Graph {
            edges: BTreeMap::from([(1, vec![(2, 1.0), (4, 0.6)]), (2, vec![(3, 1.0)])]),
            tables: BTreeMap::from([(3, vec![(7, "update".to_string())])]),
            names: BTreeMap::from([
                (1, "A#go".into()),
                (2, "B#mid".into()),
                (3, "C#save".into()),
                (4, "D#leaf".into()),
            ]),
            members: BTreeMap::new(),
        }
    }

    #[test]
    fn reaches_table_access_and_records_path() {
        let paths = walk(&graph(), 1);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].symbols, vec![1, 2, 3]);
        assert_eq!(paths[0].labels, vec!["A#go", "B#mid", "C#save"]);
        assert_eq!(paths[0].min_confidence, 1.0);
    }

    #[test]
    fn min_confidence_is_the_weakest_hop() {
        let mut g = graph();
        g.edges.insert(1, vec![(2, 0.6)]);
        let paths = walk(&g, 1);
        assert_eq!(paths[0].min_confidence, 0.6);
    }

    #[test]
    fn collects_tables_with_operations() {
        let g = graph();
        let t = tables_on(&g, &[1, 2, 3]);
        assert_eq!(t[&7], BTreeSet::from(["update".to_string()]));
    }

    #[test]
    fn stops_at_depth_limit() {
        // A chain longer than MAX_DEPTH with the table only at the very end.
        let n = MAX_DEPTH as i64 + 3;
        let mut edges = BTreeMap::new();
        for i in 1..n {
            edges.insert(i, vec![(i + 1, 1.0)]);
        }
        let g = Graph {
            edges,
            tables: BTreeMap::from([(n, vec![(9, "select".to_string())])]),
            names: (1..=n).map(|i| (i, format!("S{i}"))).collect(),
            members: BTreeMap::new(),
        };
        assert!(walk(&g, 1).is_empty());
    }

    #[test]
    fn cycles_terminate() {
        let g = Graph {
            edges: BTreeMap::from([(1, vec![(2, 1.0)]), (2, vec![(1, 1.0)])]),
            tables: BTreeMap::new(),
            names: BTreeMap::from([(1, "A".into()), (2, "B".into())]),
            members: BTreeMap::new(),
        };
        assert!(walk(&g, 1).is_empty());
    }

    #[test]
    fn each_table_reported_once_by_shortest_path() {
        // Two routes to the same table; only the shorter is kept.
        let g = Graph {
            edges: BTreeMap::from([(1, vec![(2, 1.0), (3, 1.0)]), (3, vec![(2, 1.0)])]),
            tables: BTreeMap::from([(2, vec![(7, "select".to_string())])]),
            names: (1..=3).map(|i| (i, format!("S{i}"))).collect(),
            members: BTreeMap::new(),
        };
        let paths = walk(&g, 1);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].symbols, vec![1, 2]);
    }
}
