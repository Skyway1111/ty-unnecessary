//! Batch-protocol contract test: span query, expr query, and two worlds — one
//! whose overlay breaks a caller (added error-severity diagnostics, the veto
//! signal) and one clean — verifying world isolation (override reset between
//! worlds) and determinism (two runs byte-identical).

use std::path::Path;
use std::process::Command;

const MAIN_PY: &str = "\
def helper(q: str):
    return q.count(\"x\")

def caller(n: int) -> int:
    return helper(\"abc\")
";

/// helper's param retyped to int: caller's `helper(\"abc\")` becomes an
/// invalid-argument-type error — the counterfactual veto signal.
const BREAKING_OVERLAY: &str = "\
def helper(q: int):
    return q

def caller(n: int) -> int:
    return caller(helper(\"abc\"))
";

/// The same file with an annotation the tree already satisfies.
const CLEAN_OVERLAY: &str = "\
def helper(q: str) -> int:
    return q.count(\"x\")

def caller(n: int) -> int:
    return helper(\"abc\")
";

fn run_batch(root: &Path, request: &serde_json::Value) -> (String, bool) {
    let request_path = root.join("request.json");
    std::fs::write(&request_path, serde_json::to_string(request).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ty-unnecessary"))
        .arg("--batch")
        .arg(&request_path)
        .arg(root)
        .output()
        .expect("failed to run ty-unnecessary");
    (
        String::from_utf8(output.stdout).unwrap(),
        output.status.success(),
    )
}

#[test]
fn batch_contract() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("m.py"), MAIN_PY).unwrap();

    let request = serde_json::json!({
        "queries": [
            // the argument `"abc"` on line 5 (1-based), byte cols 18..23
            {"id": "span0", "file": "m.py", "line": 5, "col_start": 18, "col_end": 23},
            // off-node span: an honest miss, never a nearest-node guess
            {"id": "miss0", "file": "m.py", "line": 5, "col_start": 11, "col_end": 20},
            {"id": "expr0", "file": "m.py", "expr": "helper"},
        ],
        "worlds": [
            {"id": "breaking", "overlays": [{"file": "m.py", "content": BREAKING_OVERLAY}]},
            {"id": "clean", "overlays": [{"file": "m.py", "content": CLEAN_OVERLAY}]},
        ],
    });

    let (stdout, ok) = run_batch(root, &request);
    assert!(ok, "batch run failed: {stdout}");
    let response: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    // clean tree, reveal artifacts filtered: no base diagnostics
    assert_eq!(response["diagnostics"].as_array().unwrap().len(), 0);

    let answers = response["answers"].as_object().unwrap();
    assert_eq!(answers["span0"], "Literal[\"abc\"]");
    assert_eq!(answers["expr0"], "(q: str) -> int");
    assert!(!answers.contains_key("miss0"));

    let worlds = response["worlds"].as_array().unwrap();
    assert_eq!(worlds[0]["id"], "breaking");
    let added = worlds[0]["added_diagnostics"].as_array().unwrap();
    assert!(
        added.iter().any(|d| d["severity"] == "error"),
        "breaking world must surface an error-severity diagnostic (veto signal): {added:?}"
    );
    assert_eq!(
        worlds[1]["added_diagnostics"].as_array().unwrap().len(),
        0,
        "clean world after a breaking world must start from restored state"
    );

    // determinism: byte-identical re-run
    let (second, ok) = run_batch(root, &request);
    assert!(ok);
    assert_eq!(stdout, second);
}

#[test]
fn malformed_request_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("m.py"), MAIN_PY).unwrap();
    let request_path = root.join("request.json");
    std::fs::write(&request_path, "{not json").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ty-unnecessary"))
        .arg("--batch")
        .arg(&request_path)
        .arg(root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "no half-response on a bad request");
}

#[test]
fn unresolvable_query_file_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("m.py"), MAIN_PY).unwrap();
    let request = serde_json::json!({
        "queries": [{"id": "q", "file": "missing.py", "expr": "x"}],
    });
    let request_path = root.join("request.json");
    std::fs::write(&request_path, serde_json::to_string(&request).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ty-unnecessary"))
        .arg("--batch")
        .arg(&request_path)
        .arg(root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}
