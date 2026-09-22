//! Java pass: parse every source file in parallel, then persist symbols,
//! annotations, HTTP routes and call edges.
//!
//! Parsing is CPU-bound and independent per file, so it fans out over rayon.
//! Persistence is serial: SQLite wants one writer, and reference resolution
//! needs the whole project's symbol table anyway.

use crate::build::BuildStats;
use crate::extract::{entrypoint, java, walk};
use crate::store::Store;
use crate::store::model::{EntrypointKind, JavaFile, SymbolKind};
use anyhow::Result;
use rayon::prelude::*;
use rusqlite::params;
use std::collections::BTreeMap;
use std::path::Path;
/// What later passes need to know about the Java layer.
pub struct JavaIndex {
    /// FQN → symbol id, for every type.
    pub types: BTreeMap<String, i64>,
    /// Simple class name → FQN. Ambiguous names are dropped rather than guessed.
    pub simple_names: BTreeMap<String, String>,
    /// Type FQN → its file's relative path.
    pub type_files: BTreeMap<String, String>,
    /// `Type#method` → symbol id.
    pub methods: BTreeMap<String, i64>,
    /// Type FQN → method names it declares, with their symbol ids.
    pub methods_by_type: BTreeMap<String, Vec<(String, i64)>>,
    /// Type FQN → Javadoc, used when an entrypoint has no doc of its own.
    pub type_docs: BTreeMap<String, String>,
    /// Interface or superclass FQN → types that declare it as a supertype.
    pub implementors: BTreeMap<String, Vec<String>>,
}

pub fn run(
    store: &Store,
    project_id: i64,
    root: &Path,
    files: &[walk::Found],
    file_ids: &BTreeMap<String, i64>,
    stats: &mut BuildStats,
) -> Result<JavaIndex> {
    let java_files: Vec<&walk::Found> =
        files.iter().filter(|f| f.lang == walk::Lang::Java).collect();

    // Parse in parallel; one parser per rayon thread via thread-local reuse is
    // unnecessary because grammar loading is cheap relative to a 200-line file.
    //
    // A parse whose content hash is already in `parse_cache` is reused instead
    // of re-parsed — this is the one step `sync` actually skips. `build` empties
    // the cache first, so it always parses every file and repopulates it.
    //
    // The `Connection` is not `Sync`, so the closure may not touch the store:
    // the cache is pre-loaded into a plain map for reads, and new parses are
    // buffered in a `Mutex` and persisted single-threaded afterwards.
    let cache: BTreeMap<String, String> = {
        let mut stmt = store.conn.prepare("SELECT sha256, result_json FROM parse_cache WHERE project_id = ?1")?;
        let mut out = BTreeMap::new();
        for row in stmt.query_map(params![project_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (sha, json) = row?;
            out.insert(sha, json);
        }
        out
    };
    let new_parses = std::sync::Mutex::new(Vec::new());
    let parsed: Vec<(String, JavaFile)> = java_files
        .par_iter()
        .filter_map(|f| {
            let src = std::fs::read_to_string(&f.path).ok()?;
            let sha = crate::util::sha256_hex(src.as_bytes());
            if let Some(hit) = cache.get(&sha) {
                return serde_json::from_str::<JavaFile>(hit).ok().map(|jf| (f.rel.clone(), jf));
            }
            let mut p = java::parser().ok()?;
            let jf = java::extract(&src, &mut p).ok()?;
            // Cache only a clean parse; a file with a syntax error is re-parsed
            // next time rather than pinning a partial result.
            if !jf.had_parse_error {
                if let Ok(json) = serde_json::to_string(&jf) {
                    new_parses.lock().unwrap().push((sha, json));
                }
            }
            Some((f.rel.clone(), jf))
        })
        .collect();
    for (sha, json) in new_parses.into_inner().unwrap() {
        store.cache_parse(project_id, &sha, "java", &json)?;
    }

    stats.parse_errors += parsed.iter().filter(|(_, jf)| jf.had_parse_error).count();

    let mut idx = JavaIndex {
        types: BTreeMap::new(),
        simple_names: BTreeMap::new(),
        type_files: BTreeMap::new(),
        methods: BTreeMap::new(),
        methods_by_type: BTreeMap::new(),
        type_docs: BTreeMap::new(),
        implementors: BTreeMap::new(),
    };
    // Simple names seen more than once cannot be resolved unambiguously.
    let mut ambiguous: Vec<String> = Vec::new();
    // Supertype simple names per type, resolved to FQNs after the first pass.
    let mut raw_supertypes: Vec<(String, Vec<String>)> = Vec::new();
    // Per-file symbol ids, kept for the later reference-resolution pass.
    let mut per_file: Vec<(String, JavaFile, Vec<i64>)> = Vec::new();

    let tx = store.conn.unchecked_transaction()?;
    {
        let mut ins_sym = tx.prepare(
            "INSERT INTO symbols(project_id, file_id, kind, name, fqn, signature,
                                 start_line, end_line, doc, visibility, parent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?;
        let mut ins_ann = tx.prepare(
            "INSERT INTO symbol_annotations(symbol_id, name, args_json) VALUES (?1, ?2, ?3)",
        )?;

        for (rel, jf) in &parsed {
            let Some(&file_id) = file_ids.get(rel) else { continue };
            let mut ids: Vec<i64> = Vec::with_capacity(jf.symbols.len());
            for sym in &jf.symbols {
                let parent_id = sym.parent.and_then(|p| ids.get(p).copied());
                ins_sym.execute(params![
                    project_id,
                    file_id,
                    sym.kind.as_str(),
                    sym.name,
                    sym.fqn,
                    sym.signature,
                    sym.start_line,
                    sym.end_line,
                    sym.doc,
                    sym.visibility,
                    parent_id,
                ])?;
                let id = tx.last_insert_rowid();
                ids.push(id);

                for a in &sym.annotations {
                    let args = serde_json::to_string(
                        &a.args.iter().cloned().collect::<BTreeMap<_, _>>(),
                    )?;
                    ins_ann.execute(params![id, a.name, args])?;
                }

                if let Some(fqn) = &sym.fqn {
                    match sym.kind {
                        SymbolKind::Class
                        | SymbolKind::Interface
                        | SymbolKind::Enum
                        | SymbolKind::Record
                        | SymbolKind::Annotation => {
                            idx.types.insert(fqn.clone(), id);
                            idx.type_files.insert(fqn.clone(), rel.clone());
                            if let Some(doc) = &sym.doc {
                                idx.type_docs.insert(fqn.clone(), doc.clone());
                            }
                            if idx.simple_names.insert(sym.name.clone(), fqn.clone()).is_some() {
                                ambiguous.push(sym.name.clone());
                            }
                            if !sym.supertypes.is_empty() {
                                raw_supertypes.push((fqn.clone(), sym.supertypes.clone()));
                            }
                        }
                        SymbolKind::Method => {
                            idx.methods.insert(fqn.clone(), id);
                            if let Some((owner, m)) = fqn.rsplit_once('#') {
                                idx.methods_by_type
                                    .entry(owner.to_string())
                                    .or_default()
                                    .push((m.to_string(), id));
                            }
                        }
                        _ => {}
                    }
                }
            }
            stats.symbols += jf.symbols.len();
            per_file.push((rel.clone(), jf.clone(), ids));
        }
    }
    tx.commit()?;

    for name in ambiguous {
        idx.simple_names.remove(&name);
    }

    // Resolve supertype simple names now that every type is known.
    for (impl_fqn, supers) in raw_supertypes {
        for s in supers {
            let target = idx
                .simple_names
                .get(&s)
                .cloned()
                // A fully qualified supertype needs no lookup.
                .or_else(|| idx.types.contains_key(&s).then(|| s.clone()));
            if let Some(target) = target {
                idx.implementors.entry(target).or_default().push(impl_fqn.clone());
            }
        }
    }

    index_http_routes(store, project_id, file_ids, &per_file, stats)?;
    resolve_refs(store, project_id, &per_file, &idx, stats)?;
    let _ = root;
    Ok(idx)
}

fn index_http_routes(
    store: &Store,
    project_id: i64,
    file_ids: &BTreeMap<String, i64>,
    per_file: &[(String, JavaFile, Vec<i64>)],
    stats: &mut BuildStats,
) -> Result<()> {
    let tx = store.conn.unchecked_transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO entrypoints(project_id, kind, name, addr, symbol_id, file_id,
                                     config_json, doc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for (rel, jf, ids) in per_file {
            let Some(&file_id) = file_ids.get(rel) else { continue };
            for r in entrypoint::http_routes(jf) {
                let sym = &jf.symbols[r.symbol];
                let cfg = serde_json::json!({ "guards": r.guards, "method": r.method });
                ins.execute(params![
                    project_id,
                    EntrypointKind::Http.as_str(),
                    sym.fqn.clone().unwrap_or_else(|| sym.name.clone()),
                    format!("{} {}", r.method, r.path),
                    ids.get(r.symbol),
                    file_id,
                    cfg.to_string(),
                    sym.doc,
                ])?;
                stats.entrypoints += 1;
            }
            for (sidx, name) in entrypoint::xxl_job_handlers(jf) {
                let sym = &jf.symbols[sidx];
                ins.execute(params![
                    project_id,
                    EntrypointKind::Job.as_str(),
                    sym.fqn.clone().unwrap_or_else(|| sym.name.clone()),
                    name,
                    ids.get(sidx),
                    file_id,
                    serde_json::json!({ "trigger": "xxl-job-annotation" }).to_string(),
                    sym.doc,
                ])?;
                stats.entrypoints += 1;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Resolve callee names to symbol ids.
///
/// Full type inference is out of scope, so each edge records how it was derived:
///
/// * `1.0` — the receiver is a field whose declared type we know, and that type
///   declares the method. Also covers an unqualified call to a method of the
///   enclosing class.
/// * `0.6` — the receiver's type is known, the method is not declared on it, but
///   exactly one project type declares that name. Typical of calls through a
///   Dubbo interface where the impl lives elsewhere.
/// * `0.3` — only the method name is known and exactly one project type declares
///   it.
///
/// Anything less certain is stored unresolved with `dst_fqn_raw` preserved, so
/// the raw evidence survives even where the graph cannot.
fn resolve_refs(
    store: &Store,
    project_id: i64,
    per_file: &[(String, JavaFile, Vec<i64>)],
    idx: &JavaIndex,
    stats: &mut BuildStats,
) -> Result<()> {
    // Method name → the single type declaring it, when unique project-wide.
    let mut unique_method: BTreeMap<&str, i64> = BTreeMap::new();
    let mut seen_twice: Vec<&str> = Vec::new();
    for (_, methods) in &idx.methods_by_type {
        for (name, id) in methods {
            if unique_method.insert(name.as_str(), *id).is_some() {
                seen_twice.push(name.as_str());
            }
        }
    }
    for n in seen_twice {
        unique_method.remove(n);
    }

    let tx = store.conn.unchecked_transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO refs(project_id, src_symbol_id, dst_symbol_id, dst_fqn_raw,
                              kind, resolved, confidence)
             VALUES (?1, ?2, ?3, ?4, 'call', ?5, ?6)",
        )?;
        for (_, jf, ids) in per_file {
            // Field and parameter types visible to a method, by identifier.
            let field_types = field_type_map(jf, idx);

            for (si, sym) in jf.symbols.iter().enumerate() {
                if sym.calls.is_empty() {
                    continue;
                }
                let Some(&src_id) = ids.get(si) else { continue };
                let owner = sym.fqn.as_deref().and_then(|f| f.split_once('#')).map(|(o, _)| o);

                for call in &sym.calls {
                    let (recv, method) = match call.split_once('.') {
                        Some((r, m)) => (Some(r), m),
                        None => (None, call.as_str()),
                    };

                    let mut dst: Option<i64> = None;
                    let mut confidence = 0.0f64;

                    if let Some(r) = recv {
                        if let Some(ty) = field_types.get(r) {
                            if let Some(id) = method_on(idx, ty, method) {
                                dst = Some(id);
                                confidence = 1.0;
                            } else if let Some(&id) = unique_method.get(method) {
                                // Known receiver type, method declared elsewhere:
                                // typically an interface call landing on an impl.
                                dst = Some(id);
                                confidence = 0.6;
                            }
                        }
                    } else if let Some(o) = owner {
                        if let Some(id) = method_on(idx, o, method) {
                            dst = Some(id);
                            confidence = 1.0;
                        }
                    }

                    if dst.is_none() {
                        if let Some(&id) = unique_method.get(method) {
                            dst = Some(id);
                            confidence = 0.3;
                        }
                    }

                    // Never record a self-loop: it adds no traversal information.
                    if dst == Some(src_id) {
                        continue;
                    }
                    ins.execute(params![
                        project_id,
                        src_id,
                        dst,
                        call,
                        dst.is_some() as i64,
                        confidence,
                    ])?;
                    stats.refs += 1;
                    if dst.is_some() {
                        stats.refs_resolved += 1;
                    }
                }
            }
        }
    }
    tx.commit()?;
    link_overrides(store, project_id, idx, stats)?;
    Ok(())
}

/// Link every interface method to the implementations that override it.
///
/// This is the dynamic-dispatch hop. These projects always call services through
/// their interface, so a purely syntactic call graph stops at
/// `DefectiveService#findById` and never reaches the `...Impl` that actually
/// touches the database. Recording the override as its own edge kind keeps the
/// distinction visible while letting traversal continue.
fn link_overrides(
    store: &Store,
    project_id: i64,
    idx: &JavaIndex,
    stats: &mut BuildStats,
) -> Result<()> {
    let tx = store.conn.unchecked_transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO refs(project_id, src_symbol_id, dst_symbol_id, dst_fqn_raw,
                              kind, resolved, confidence)
             VALUES (?1, ?2, ?3, ?4, 'override', 1, ?5)",
        )?;
        for (supertype, impls) in &idx.implementors {
            let Some(super_methods) = idx.methods_by_type.get(supertype) else { continue };
            for impl_fqn in impls {
                let Some(impl_methods) = idx.methods_by_type.get(impl_fqn) else { continue };
                for (name, super_id) in super_methods {
                    // Overload sets collapse to the same name here; a single
                    // interface method may therefore link to several impl
                    // methods, which is the safe direction for traversal.
                    let matches: Vec<i64> = impl_methods
                        .iter()
                        .filter(|(n, _)| n == name)
                        .map(|(_, id)| *id)
                        .collect();
                    // One implementor with one matching method is unambiguous;
                    // several candidates mean the true target is a guess.
                    let confidence = if impls.len() == 1 && matches.len() == 1 { 1.0 } else { 0.7 };
                    for impl_id in matches {
                        if impl_id == *super_id {
                            continue;
                        }
                        ins.execute(params![
                            project_id,
                            super_id,
                            impl_id,
                            format!("{impl_fqn}#{name}"),
                            confidence,
                        ])?;
                        stats.refs += 1;
                        stats.refs_resolved += 1;
                    }
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// The symbol id of `method` declared directly on `type_fqn`.
fn method_on(idx: &JavaIndex, type_fqn: &str, method: &str) -> Option<i64> {
    idx.methods_by_type
        .get(type_fqn)?
        .iter()
        .find(|(n, _)| n == method)
        .map(|(_, id)| *id)
}

/// Identifier → declared type FQN, for a file's fields.
///
/// These projects inject dependencies as annotated fields, so a field's declared
/// type is what makes `couponService.query(...)` resolvable at all.
fn field_type_map(jf: &JavaFile, idx: &JavaIndex) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for sym in &jf.symbols {
        if !matches!(sym.kind, SymbolKind::Field | SymbolKind::Constant) {
            continue;
        }
        let Some(sig) = sym.signature.as_deref() else { continue };
        // Signature is `Type name` or `Type name = init`.
        let Some(raw_ty) = sig.split_whitespace().next() else { continue };
        // Drop generics: `List<Coupon>` resolves as `List`, which we then ignore
        // unless it names a project type.
        let base = raw_ty.split('<').next().unwrap_or(raw_ty).trim();
        let simple = base.rsplit('.').next().unwrap_or(base);
        if let Some(fqn) = idx.simple_names.get(simple) {
            out.insert(sym.name.clone(), fqn.clone());
        } else if idx.types.contains_key(base) {
            out.insert(sym.name.clone(), base.to_string());
        }
    }
    out
}
