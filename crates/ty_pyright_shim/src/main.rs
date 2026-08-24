//! `ty-unnecessary`: a pyright-CLI-compatible shim over `ty check`.
//!
//! Speaks just enough of basedpyright's CLI (`--outputjson --threads N --project
//! <pyrightconfig.json> <root>`) and output JSON (`generalDiagnostics` with pyright rule
//! names) for sightline's `provers/oracle.py` to run against it unchanged. Emits only the
//! diagnostics the oracle consumes: the `reportUnnecessary*` family (from the ported
//! `unnecessary-*` lints plus `redundant-cast`), `reportMissingImports` (from
//! `unresolved-import`), and `reveal_type` answers reformatted to pyright's
//! `Type of "..." is "..."` message shape.

use std::io::Write;

use anyhow::{Context, Result};
use ruff_db::diagnostic::{Diagnostic, DiagnosticId, UnifiedFile};
use ruff_db::source::{line_index, source_text};
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ty_project::metadata::options::{EnvironmentOptions, Options, SrcOptions};
use ruff_ranged_value::RangedValue;
use ty_project::metadata::value::{RelativeGlobPattern, RelativePathBuf};
use ty_project::{ProjectDatabase, ProjectMetadata};
use ty_python_semantic::lint::Level;

struct Args {
    threads: usize,
    project_config: Option<String>,
    python_path: Option<String>,
    root: String,
}

fn parse_args() -> Result<Args> {
    let mut threads = 4usize;
    let mut project_config = None;
    let mut python_path = None;
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
            other if other.starts_with("--") => {
                // Ignore unknown pyright flags for drop-in compatibility.
            }
            other => root = Some(other.to_string()),
        }
    }
    Ok(Args {
        threads,
        project_config,
        python_path,
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
    let args = parse_args()?;
    let mut config = read_pyright_config(args.project_config.as_deref())?;
    // The CLI flag wins over the config key, matching basedpyright 1.39.10 (which
    // no longer recognizes "pythonPath" in the config at all).
    if args.python_path.is_some() {
        config.python_path = args.python_path.clone();
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
    let system = OsSystem::new(&cwd);

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
                    config.exclude.iter().map(RelativeGlobPattern::cli).collect(),
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

fn convert(db: &ProjectDatabase, diagnostic: &Diagnostic) -> Option<serde_json::Value> {
    let rule = pyright_rule(diagnostic.id())?;
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

    let (rule_field, severity, message) = if rule == "reveal" {
        // pyright: `Type of "<expr>" is "<type>"`. The oracle's regex wildcards the
        // expression, so a placeholder is fine; the type comes from ty's annotation
        // message (a backtick-quoted type), normalized to pyright's display forms.
        let type_text =
            normalize_type_display(annotation.get_message()?.trim_matches('`'));
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
fn normalize_type_display(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("<module '") {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx + "<module '".len()..];
        if let Some(end) = tail.find("'>") {
            out.push_str("Module(\"");
            out.push_str(&tail[..end]);
            out.push_str("\")");
            rest = &tail[end + 2..];
        } else {
            out.push_str("<module '");
            rest = tail;
        }
    }
    out.push_str(rest);

    // `def name(...)` -> `(...)`; exact-form star markers dropped.
    let mut result = String::with_capacity(out.len());
    let bytes = out.as_bytes();
    let mut skip_until = 0usize;
    for (i, ch) in out.char_indices() {
        if i < skip_until {
            continue;
        }
        if ch == '*' && i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
            continue; // float* / complex* exact markers
        }
        if out[i..].starts_with("def ")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        {
            if let Some(paren) = out[i..].find('(') {
                let name = &out[i + 4..i + paren];
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    skip_until = i + paren;
                    continue;
                }
            }
        }
        result.push(ch);
    }
    result
}

/// ty renders inline code in diagnostic messages with backticks; pyright's messages quote
/// types with double quotes, which our ported lints already emit. Leave messages untouched
/// except for stray backticks from non-ported diagnostics (e.g. `unresolved-import`).
fn strip_markup(message: &str) -> String {
    message.replace('`', "\"")
}
