//! `--batch <request.json>`: sightline's native protocol, replacing the
//! reveal-injection transport and per-pass shadow re-checks.
//!
//! Request (files root-relative posix; cols are UTF-8 byte offsets within the
//! line, the Python `ast` convention):
//!
//! ```json
//! {
//!   "queries": [
//!     {"id": "a0", "file": "src/m.py", "line": 12, "col_start": 4, "col_end": 9},
//!     {"id": "r0", "file": "src/m.py", "expr": "C.m"}
//!   ],
//!   "worlds": [
//!     {"id": "p0", "overlays": [{"file": "src/m.py", "content": "..."}]}
//!   ]
//! }
//! ```
//!
//! Response: `{"version", "diagnostics": [...], "answers": {id: type},
//! "worlds": [{"id", "added_diagnostics": [...]}]}` — diagnostic entries in the
//! same shape as the pyright-CLI mode's `generalDiagnostics` (error-severity
//! passthrough included: sightline's counterfactual veto depends on it).
//!
//! Span queries resolve natively (covering node at the exact range, real
//! inference — no source mutation). Expr queries append `reveal_type(<expr>)`
//! lines in memory via source-text overrides, exercising the very
//! `report_revealed_type` path the old transport used; the resulting
//! `RevealedType` artifacts never reach `diagnostics`. Worlds apply full-file
//! overlays sequentially on the same salsa db — an incremental re-check — and
//! report the diagnostics added relative to the base check, by
//! `(file, line, rule)` identity (sightline's counterfactual baseline key).

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, anyhow};
use ruff_db::diagnostic::{Diagnostic, DiagnosticId};
use ruff_db::files::{File, system_path_to_file};
use ruff_db::parsed::parsed_module;
use ruff_db::source::{line_index, source_text};
use ruff_db::system::{SystemPath, SystemPathBuf};
use ruff_diagnostics::SourceMap;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::find_node::covering_node;
use ruff_text_size::{Ranged, TextRange, TextSize};
use salsa::Setter as _;
use serde::Deserialize;
use ty_project::ProjectDatabase;
use ty_python_semantic::Db as _;
use ty_python_semantic::types::revealed_display;
use ty_python_semantic::{HasType, SemanticModel};

use crate::convert;

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    queries: Vec<Query>,
    #[serde(default)]
    worlds: Vec<World>,
}

#[derive(Deserialize)]
struct Query {
    id: String,
    file: String,
    #[serde(default)]
    line: Option<usize>, // 1-based
    #[serde(default)]
    col_start: Option<usize>,
    #[serde(default)]
    col_end: Option<usize>,
    #[serde(default)]
    expr: Option<String>,
}

#[derive(Deserialize)]
struct World {
    id: String,
    overlays: Vec<Overlay>,
}

#[derive(Deserialize)]
struct Overlay {
    file: String,
    content: String,
}

pub(crate) fn run(
    db: &mut ProjectDatabase,
    root: &SystemPath,
    request_path: &str,
) -> Result<()> {
    let text = std::fs::read_to_string(request_path)
        .with_context(|| format!("failed to read batch request `{request_path}`"))?;
    let request: Request =
        serde_json::from_str(&text).context("batch request is not valid JSON")?;

    fn resolve(db: &ProjectDatabase, root: &SystemPath, rel: &str) -> Result<File> {
        let path = SystemPathBuf::from(root.as_str()).join(rel);
        system_path_to_file(db, &path)
            .map_err(|_| anyhow!("request names a file the project cannot resolve: `{rel}`"))
    }

    // -- expr queries: append reveal_type lines in memory, before the base check
    let mut appends_by_file: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for q in &request.queries {
        if let Some(expr) = &q.expr {
            appends_by_file
                .entry(q.file.clone())
                .or_default()
                .push((q.id.clone(), expr.clone()));
        }
    }
    let mut reveal_ids: HashMap<(File, usize), String> = HashMap::new();
    for (rel, entries) in &appends_by_file {
        let file = resolve(db, root, rel)?;
        let base = source_text(db, file);
        let mut content = base.as_str().to_string();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        let mut line = content.matches('\n').count();
        for (id, expr) in entries {
            content.push_str(&format!("reveal_type({expr})\n"));
            line += 1;
            reveal_ids.insert((file, line), id.clone());
        }
        let overridden = base.with_text(content, &SourceMap::default());
        file.set_source_text_override(db).to(Some(overridden));
    }

    // -- base check: diagnostics + reveal answers
    let mut answers: BTreeMap<String, String> = BTreeMap::new();
    let mut diagnostics = Vec::new();
    let mut base_keys: HashSet<(String, u64, String)> = HashSet::new();
    for diagnostic in &db.check() {
        if let Some((file, line, type_text)) = reveal_answer(db, diagnostic) {
            if let Some(id) = reveal_ids.get(&(file, line)) {
                answers.insert(id.clone(), type_text);
            }
            continue; // transport artifact (ours or user-authored reveal_type)
        }
        if let Some(entry) = convert(db, diagnostic) {
            if let Some(key) = entry_key(&entry) {
                base_keys.insert(key);
            }
            diagnostics.push(entry);
        }
    }

    // -- span queries: native resolution against the same base state
    for q in &request.queries {
        if q.expr.is_some() {
            continue;
        }
        let (Some(line), Some(col_start), Some(col_end)) = (q.line, q.col_start, q.col_end)
        else {
            return Err(anyhow!(
                "query `{}` has neither expr nor a complete line/col span",
                q.id
            ));
        };
        let file = resolve(db, root, &q.file)?;
        if let Some(type_text) = span_type(db, file, line, col_start, col_end) {
            answers.insert(q.id.clone(), type_text);
        }
    }

    // -- worlds: sequential incremental re-checks, added-diagnostics vs base
    let mut world_results = Vec::new();
    for world in &request.worlds {
        let mut restore: Vec<(File, Option<ruff_db::source::SourceText>)> = Vec::new();
        for overlay in &world.overlays {
            let file = resolve(db, root, &overlay.file)?;
            restore.push((file, file.source_text_override(db).clone()));
            let overridden =
                source_text(db, file).with_text(overlay.content.clone(), &SourceMap::default());
            file.set_source_text_override(db).to(Some(overridden));
        }
        let mut added = Vec::new();
        for diagnostic in &db.check() {
            if diagnostic.id() == DiagnosticId::RevealedType {
                continue;
            }
            if let Some(entry) = convert(db, diagnostic) {
                match entry_key(&entry) {
                    Some(key) if base_keys.contains(&key) => {}
                    _ => added.push(entry),
                }
            }
        }
        for (file, prior) in restore {
            file.set_source_text_override(db).to(prior);
        }
        world_results.push(serde_json::json!({
            "id": world.id,
            "added_diagnostics": added,
        }));
    }

    let output = serde_json::json!({
        "version": "1.1.412-ty-unnecessary-batch",
        "diagnostics": diagnostics,
        "answers": answers,
        "worlds": world_results,
    });
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &output)?;
    use std::io::Write;
    stdout.write_all(b"\n")?;
    Ok(())
}

/// `(file, one-based line, normalized type text)` when `diagnostic` is a
/// `RevealedType` artifact.
fn reveal_answer(
    db: &ProjectDatabase,
    diagnostic: &Diagnostic,
) -> Option<(File, usize, String)> {
    if diagnostic.id() != DiagnosticId::RevealedType {
        return None;
    }
    let annotation = diagnostic.primary_annotation()?;
    let span = annotation.get_span();
    let ruff_db::diagnostic::UnifiedFile::Ty(file) = span.file() else {
        return None;
    };
    let range = span.range()?;
    let source = source_text(db, *file);
    let index = line_index(db, *file);
    let line = index.line_column(range.start(), &source).line.get();
    let type_text =
        crate::normalize_type_display(annotation.get_message()?.trim_matches('`'));
    Some((*file, line, type_text))
}

/// Native `(file, span) -> type`: the expression whose range is exactly the
/// requested span, through the same inferred-return patch and display
/// normalization as the reveal transport. `None` is an honest miss (no node at
/// exactly that range, or no inferred type) — never a nearest-node guess.
fn span_type(
    db: &ProjectDatabase,
    file: File,
    line: usize,
    col_start: usize,
    col_end: usize,
) -> Option<String> {
    let program_file = db.program_file(file);
    let parsed = parsed_module(db, program_file.python_file(db)).load(db);
    let source = source_text(db, file);
    let index = line_index(db, file);
    let line_start = index.line_start(
        ruff_source_file::OneIndexed::new(line)?,
        &source,
    );
    let start = line_start + TextSize::try_from(col_start).ok()?;
    let end = line_start + TextSize::try_from(col_end).ok()?;
    if end > TextSize::of(source.as_str()) || start > end {
        return None;
    }
    let range = TextRange::new(start, end);
    // the innermost *expression* at exactly the requested range (the leaf may
    // be a sub-expression token node, e.g. StringLiteral under ExprStringLiteral)
    let covering = covering_node(parsed.syntax().into(), range)
        .find_first(AnyNodeRef::is_expression)
        .ok()?;
    let node = covering.node();
    if node.range() != range {
        return None;
    }
    let expr_ref = node.as_expr_ref()?;
    let model = SemanticModel::new(db, program_file);
    let ty = expr_ref.inferred_type(&model)?;
    let display = revealed_display(db, ty, &model.program_environment());
    Some(crate::normalize_type_display(&display))
}

/// The added-diff identity `(file, zero-based line, rule)` of a converted
/// diagnostic entry — sightline's counterfactual baseline key.
fn entry_key(entry: &serde_json::Value) -> Option<(String, u64, String)> {
    Some((
        entry.get("file")?.as_str()?.to_string(),
        entry.get("range")?.get("start")?.get("line")?.as_u64()?,
        entry
            .get("rule")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string(),
    ))
}
