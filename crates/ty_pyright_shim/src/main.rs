//! `ty-unnecessary`: a pyright-CLI-compatible shim over `ty check`.
//!
//! Speaks just enough of basedpyright's CLI (`--outputjson --threads N --project
//! <pyrightconfig.json> <root>`) and output JSON (`generalDiagnostics` with pyright rule
//! names) for sightline's `provers/oracle.py` to run against it unchanged. Emits only the
//! diagnostics the oracle consumes: the `reportUnnecessary*` family (from the ported
//! `unnecessary-*` lints plus `redundant-cast`), `reportMissingImports` (from
//! `unresolved-import`), and `reveal_type` answers reformatted to pyright's
//! `Type of "..." is "..."` message shape.

mod batch;

use std::io::Write;

use anyhow::{Context, Result};
use ruff_db::diagnostic::{Diagnostic, DiagnosticId, Severity, UnifiedFile};
use ruff_db::source::{line_index, source_text};
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ruff_ranged_value::RangedValue;
use ty_project::metadata::options::{EnvironmentOptions, Options, SrcOptions};
use ty_project::metadata::value::{RelativeGlobPattern, RelativePathBuf};
use ty_project::{ProjectDatabase, ProjectMetadata};
use ty_python_semantic::lint::Level;

struct Args {
    threads: usize,
    project_config: Option<String>,
    python_path: Option<String>,
    batch: Option<String>,
    serve: bool,
    root: String,
}

fn parse_args() -> Result<Args> {
    let mut threads = 4usize;
    let mut project_config = None;
    let mut python_path = None;
    let mut batch = None;
    let mut serve = false;
    let mut root = None;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--outputjson" => {}
            "--threads" => {
                threads = iter
                    .next()
                    .context("--threads requires a value")?
                    .parse()
                    .context("--threads value must be an integer")?;
            }
            "--project" | "-p" => {
                project_config = Some(iter.next().context("--project requires a value")?);
            }
            "--pythonpath" => {
                python_path = Some(iter.next().context("--pythonpath requires a value")?);
            }
            "--batch" => {
                batch = Some(iter.next().context("--batch requires a value")?);
            }
            "--serve" => serve = true,
            other if other.starts_with("--") => {
                // Boolean pyright flags are ignored for drop-in compatibility. An unknown
                // *value-taking* flag would misparse (its value lands in `root`); the
                // oracle's invocation is fixed, so none reach us today.
            }
            other => root = Some(other.to_string()),
        }
    }
    Ok(Args {
        threads,
        project_config,
        python_path,
        batch,
        serve,
        root: root.context("missing root path argument")?,
    })
}

/// The subset of pyrightconfig.json the oracle writes.
struct PyrightConfig {
    extra_paths: Vec<String>,
    exclude: Vec<String>,
    python_path: Option<String>,
}

fn read_pyright_config(path: Option<&str>) -> Result<PyrightConfig> {
    let mut config = PyrightConfig {
        extra_paths: Vec::new(),
        exclude: Vec::new(),
        python_path: None,
    };
    let Some(path) = path else {
        return Ok(config);
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read project config `{path}`"))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).context("project config is not valid JSON")?;
    let string_list = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    config.extra_paths = string_list("extraPaths");
    config.exclude = string_list("exclude");
    config.python_path = value
        .get("pythonPath")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(config)
}

/// pyright rule name for a ty diagnostic; `None` drops the diagnostic.
fn pyright_rule(id: DiagnosticId) -> Option<&'static str> {
    if id == DiagnosticId::RevealedType {
        return Some("reveal");
    }
    let DiagnosticId::Lint(name) = id else {
        return None;
    };
    match name.as_str() {
        "unnecessary-isinstance" => Some("reportUnnecessaryIsInstance"),
        "unnecessary-comparison" => Some("reportUnnecessaryComparison"),
        "unnecessary-contains" => Some("reportUnnecessaryContains"),
        "redundant-cast" => Some("reportUnnecessaryCast"),
        "unresolved-import" => Some("reportMissingImports"),
        _ => None,
    }
}

fn main() -> Result<()> {
    // Before any root parsing: an installer asks a downloaded binary which commit it is.
    if std::env::args().skip(1).any(|arg| arg == "--version") {
        println!("ty-unnecessary {}", env!("SHIM_COMMIT"));
        return Ok(());
    }
    let args = parse_args()?;
    let mut config = read_pyright_config(args.project_config.as_deref())?;
    // The CLI flag wins over the config key, matching basedpyright 1.39.10 (which
    // no longer recognizes "pythonPath" in the config at all).
    if args.python_path.is_some() {
        config.python_path.clone_from(&args.python_path);
    }

    rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .stack_size(16 * 1024 * 1024)
        .build_global()
        .ok();

    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let cwd = SystemPathBuf::from_path_buf(cwd)
        .map_err(|_| anyhow::anyhow!("current directory is not valid UTF-8"))?;
    let root = SystemPath::absolute(&args.root, &cwd);
    // The system anchors CLI-relative values (RelativeGlobPattern::cli /
    // RelativePathBuf::cli) at its cwd. The oracle invokes this binary from an
    // arbitrary directory, so anchor at the analyzed root — otherwise the
    // config's exclude globs silently match nothing on a real tree.
    let system = OsSystem::new(&root);

    let mut metadata = ProjectMetadata::discover(&root, &system)?;
    metadata.apply_configuration_files(&system)?;

    let rules = [
        "unnecessary-isinstance",
        "unnecessary-comparison",
        "unnecessary-contains",
        "redundant-cast",
        "unresolved-import",
    ]
    .into_iter()
    .map(|rule| {
        (
            RangedValue::cli(rule.to_string()),
            RangedValue::cli(Level::Warn),
        )
    })
    .collect();

    let options = Options {
        environment: Some(EnvironmentOptions {
            python: config.python_path.map(RelativePathBuf::cli),
            extra_paths: if config.extra_paths.is_empty() {
                None
            } else {
                Some(
                    config
                        .extra_paths
                        .iter()
                        .map(RelativePathBuf::cli)
                        .collect(),
                )
            },
            ..EnvironmentOptions::default()
        }),
        src: Some(SrcOptions {
            // pyright does not respect gitignore files; exclusion is config-driven only.
            respect_ignore_files: Some(false),
            exclude: if config.exclude.is_empty() {
                None
            } else {
                Some(RangedValue::cli(
                    config
                        .exclude
                        .iter()
                        .map(RelativeGlobPattern::cli)
                        .collect(),
                ))
            },
            ..SrcOptions::default()
        }),
        rules: Some(rules),
        ..Options::default()
    };
    metadata.apply_override_options(options);

    let mut db = ProjectDatabase::fallible(metadata, system)?;
    ruff_db::disable_lru(&mut db);

    if args.serve {
        // one warm db serves every request (sightline v5.2): the batch mode's
        // three redundant cold checks per audit go
        return batch::serve(&mut db, &root);
    }
    if let Some(request_path) = &args.batch {
        // batch mode mutates source-text overrides (expr appends, world
        // overlays), which a frozen db panics on — never freeze here
        return batch::run(&mut db, &root, request_path);
    }

    db.freeze_open_files();
    db.freeze();

    let diagnostics = db.check();

    let mut general = Vec::new();
    for diagnostic in &diagnostics {
        if let Some(entry) = convert(&db, diagnostic) {
            general.push(entry);
        }
    }

    let output = serde_json::json!({
        "version": "1.1.412-ty-unnecessary",
        "generalDiagnostics": general,
        "summary": {
            "errorCount": 0,
            "warningCount": general.len(),
            "informationCount": 0,
        },
    });
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &output)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

pub(crate) fn convert(db: &ProjectDatabase, diagnostic: &Diagnostic) -> Option<serde_json::Value> {
    // Unmapped diagnostics pass through at error severity only: sightline's
    // counterfactual arbiter (#5/#10) vetoes a proposal on any *new error* in
    // its caller files after the annotation splice, so type errors such as
    // `invalid-argument-type` must reach the JSON or vetoes can never fire.
    let rule = match pyright_rule(diagnostic.id()) {
        Some(rule) => rule,
        None if diagnostic.severity() >= Severity::Error => "",
        None => return None,
    };
    let annotation = diagnostic.primary_annotation()?;
    let span = annotation.get_span();
    let &UnifiedFile::Ty(file) = span.file() else {
        return None;
    };
    let range = span.range()?;
    let path = file.path(db).as_system_path()?.to_string();

    let source = source_text(db, file);
    let index = line_index(db, file);
    let start = index.line_column(range.start(), &source);
    let end = index.line_column(range.end(), &source);
    let position = |lc: ruff_source_file::LineColumn| {
        serde_json::json!({
            "line": lc.line.to_zero_indexed(),
            "character": lc.column.to_zero_indexed(),
        })
    };

    let passthrough_rule;
    let (rule_field, severity, message) = if rule.is_empty() {
        passthrough_rule = diagnostic.id().to_string();
        (
            Some(passthrough_rule.as_str()),
            "error",
            strip_markup(diagnostic.headline_message()),
        )
    } else if rule == "reveal" {
        // pyright: `Type of "<expr>" is "<type>"`. The oracle's regex wildcards the
        // expression, so a placeholder is fine; the type comes from ty's annotation
        // message (a backtick-quoted type), normalized to pyright's display forms.
        let type_text = normalize_type_display(annotation.get_message()?.trim_matches('`'));
        let expr_text = source
            .as_str()
            .get(std::ops::Range::<usize>::from(range))
            .unwrap_or("_");
        (
            None,
            "information",
            format!("Type of \"{expr_text}\" is \"{type_text}\""),
        )
    } else {
        (
            Some(rule),
            "warning",
            strip_markup(diagnostic.headline_message()),
        )
    };

    let mut entry = serde_json::json!({
        "file": path,
        "severity": severity,
        "message": message,
        "range": {
            "start": position(start),
            "end": position(end),
        },
    });
    if let Some(rule) = rule_field {
        entry["rule"] = serde_json::Value::String(rule.to_string());
    }
    Some(entry)
}

/// Rewrite ty type displays that sightline's type-string algebra would misread into
/// pyright's forms: exact-form markers (`float*` -> `float`), module literals
/// (`<module 'x'>` -> `Module("x")`), and named function signatures
/// (`def f(x) -> R` -> `(x) -> R`).
pub(crate) fn normalize_type_display(text: &str) -> String {
    // Every rewrite must leave quoted string-literal contents (`Literal["a*b"]`) alone:
    // this function feeds the parity pipeline, so silent corruption there would show up
    // as spurious answer mismatches.
    //
    // `<module 'x'>` -> `Module("x")`. This runs on the whole string because the module
    // form carries its own single quotes, which the quote-aware pass below would treat as
    // a string literal; the full `<module '` prefix is specific enough not to occur
    // inside real literals. Its `Module("x")` output then reads as a quoted span below,
    // protecting module names from the star/def surgery.
    let mut with_modules = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("<module '") {
        with_modules.push_str(&rest[..idx]);
        let tail = &rest[idx + "<module '".len()..];
        if let Some(end) = tail.find("'>") {
            with_modules.push_str("Module(\"");
            with_modules.push_str(&tail[..end]);
            with_modules.push_str("\")");
            rest = &tail[end + 2..];
        } else {
            with_modules.push_str("<module '");
            rest = tail;
        }
    }
    with_modules.push_str(rest);

    rewrite_outside_quotes(&with_modules, |span, out| {
        // Exact-form markers (`float*`) dropped; `def name(...)` -> `(...)`.
        let bytes = span.as_bytes();
        let mut skip_until = 0usize;
        for (i, ch) in span.char_indices() {
            if i < skip_until {
                continue;
            }
            if ch == '*' && i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
                continue;
            }
            if span[i..].starts_with("def ") && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
                if let Some(paren) = span[i..].find('(') {
                    let name = &span[i + 4..i + paren];
                    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        skip_until = i + paren;
                        continue;
                    }
                }
            }
            out.push(ch);
        }
    })
}

/// Apply `rewrite` to the spans of `text` outside `"…"`/`'…'` string literals (backslash
/// escapes respected); quoted spans are copied through verbatim.
fn rewrite_outside_quotes(text: &str, rewrite: impl Fn(&str, &mut String)) -> String {
    let mut out = String::with_capacity(text.len());
    let mut span_start = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, ch) in text.char_indices() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == q {
                    quote = None;
                    out.push_str(&text[span_start..i + ch.len_utf8()]);
                    span_start = i + ch.len_utf8();
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    rewrite(&text[span_start..i], &mut out);
                    span_start = i;
                    quote = Some(ch);
                }
            }
        }
    }
    match quote {
        // Unterminated quote: treat the tail as literal.
        Some(_) => out.push_str(&text[span_start..]),
        None => rewrite(&text[span_start..], &mut out),
    }
    out
}

/// ty renders inline code in diagnostic messages with backticks; pyright's messages quote
/// types with double quotes, which our ported lints already emit. Leave messages untouched
/// except for stray backticks from non-ported diagnostics (e.g. `unresolved-import`).
fn strip_markup(message: &str) -> String {
    message.replace('`', "\"")
}

#[cfg(test)]
mod tests {
    use super::normalize_type_display;

    #[test]
    fn star_markers_stripped_outside_literals() {
        assert_eq!(normalize_type_display("float*"), "float");
        assert_eq!(
            normalize_type_display("int | float* | complex*"),
            "int | complex".replace("int | complex", "int | float | complex")
        );
    }

    #[test]
    fn literal_contents_untouched() {
        assert_eq!(
            normalize_type_display("Literal[\"a*b\"]"),
            "Literal[\"a*b\"]"
        );
        assert_eq!(
            normalize_type_display("Literal['def f(']"),
            "Literal['def f(']"
        );
        assert_eq!(
            normalize_type_display("Literal[\"x\"] | float*"),
            "Literal[\"x\"] | float"
        );
    }

    #[test]
    fn module_display_rewritten() {
        assert_eq!(
            normalize_type_display("<module 'torch'>"),
            "Module(\"torch\")"
        );
        // the rewritten module name is quote-protected from later passes
        assert_eq!(normalize_type_display("<module 'a*b'>"), "Module(\"a*b\")");
    }

    #[test]
    fn def_signatures_lose_their_name() {
        assert_eq!(normalize_type_display("def call() -> int"), "() -> int");
        assert_eq!(
            normalize_type_display("def f(x, y) -> None"),
            "(x, y) -> None"
        );
        // not a definition display: untouched
        assert_eq!(
            normalize_type_display("undef (x) -> int"),
            "undef (x) -> int"
        );
    }
}
