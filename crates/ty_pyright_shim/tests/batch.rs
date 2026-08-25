//! Batch-protocol contract test: span query, expr query, and two worlds — one
//! whose overlay breaks a caller (added error-severity diagnostics, the veto
//! signal) and one clean — verifying world isolation (override reset between
//! worlds) and determinism (two runs byte-identical).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

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
fn config_excludes_hold_on_a_real_tree() {
    // exclude globs anchor at the analyzed root, not the invoking cwd:
    // an excluded vendored dir must contribute no diagnostics
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("m.py"), MAIN_PY).unwrap();
    std::fs::create_dir_all(root.join("_vendored/toolchain")).unwrap();
    std::fs::write(
        root.join("_vendored/toolchain/bad.py"),
        "import missing_module_xyz\n",
    )
    .unwrap();
    let cfg = root.join("pyrightconfig.json");
    std::fs::write(
        &cfg,
        r#"{"reportMissingImports": "warning", "exclude": ["**/_vendored"]}"#,
    )
    .unwrap();
    let request_path = root.join("request.json");
    std::fs::write(&request_path, "{}").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ty-unnecessary"))
        .arg("--batch")
        .arg(&request_path)
        .arg("--project")
        .arg(&cfg)
        .arg(root)
        .output()
        .unwrap();
    assert!(output.status.success());
    let response: serde_json::Value =
        serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    assert_eq!(
        response["diagnostics"].as_array().unwrap().len(),
        0,
        "excluded dir leaked diagnostics: {response}"
    );
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
    assert!(
        output.stdout.is_empty(),
        "no half-response on a bad request"
    );
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

/// Two classes share a method name (CHA cannot pick); the receiver's type
/// does. A constructor call targets the class; a `Callable`-typed parameter
/// call has no definition to report.
const EDGES_PY: &str = "\
class C:
    def m(self, x: int) -> int:
        return x

class D:
    def m(self, x: int) -> int:
        return x

def helper():
    return 1

def use(c: C, cb):
    c.m(1)
    C()
    cb(2)
    helper()
";

#[test]
fn call_edges_report_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("m.py"), EDGES_PY).unwrap();
    let (stdout, ok) = run_batch(root, &serde_json::json!({"call_edges": true}));
    assert!(ok, "batch run failed: {stdout}");
    let response: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let edges: Vec<(u64, u64, u64, u64, Vec<u64>)> = response["call_edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["line"].as_u64().unwrap(),
                e["col"].as_u64().unwrap(),
                e["end_line"].as_u64().unwrap(),
                e["end_col"].as_u64().unwrap(),
                e["targets"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t["line"].as_u64().unwrap())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        edges,
        vec![
            (13, 4, 13, 10, vec![2]), // c.m(1) -> C.m, not D.m
            (14, 4, 14, 7, vec![1]),  // C() -> class C
            (16, 4, 16, 12, vec![9]), // helper() -> def helper
        ],
        "{response}"
    );
    for edge in response["call_edges"].as_array().unwrap() {
        assert!(edge["file"].as_str().unwrap().ends_with("m.py"));
        assert!(
            edge["targets"][0]["file"]
                .as_str()
                .unwrap()
                .ends_with("m.py")
        );
    }

    // not requested: absent from the work, empty in the response
    let (stdout, ok) = run_batch(root, &serde_json::json!({}));
    assert!(ok);
    let response: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(response["call_edges"].as_array().unwrap().len(), 0);
}

/// `--serve` answers every request on one warm db exactly like a fresh
/// `--batch` process would: expr appends and world overlays are undone
/// between requests (sightline v5.2: the four passes share one process).
#[test]
fn serve_answers_each_request_like_a_fresh_batch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("m.py"), MAIN_PY).unwrap();
    let full = serde_json::json!({
        "queries": [
            {"id": "span0", "file": "m.py", "line": 5, "col_start": 18, "col_end": 23},
            {"id": "expr0", "file": "m.py", "expr": "helper"},
        ],
        "worlds": [
            {"id": "breaking", "overlays": [{"file": "m.py", "content": BREAKING_OVERLAY}]},
        ],
        "call_edges": true,
    });
    let diagnostics_only = serde_json::json!({"call_edges": true});
    let requests = [&full, &diagnostics_only, &full, &diagnostics_only];
    let expected: Vec<String> = requests
        .iter()
        .map(|request| {
            let (stdout, ok) = run_batch(root, request);
            assert!(ok, "batch run failed: {stdout}");
            stdout
        })
        .collect();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ty-unnecessary"))
        .arg("--serve")
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to start ty-unnecessary --serve");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    for (request, want) in requests.iter().zip(&expected) {
        writeln!(stdin, "{}", serde_json::to_string(request).unwrap()).unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(&line, want, "a served response must equal the fresh-batch one");
    }
    drop(stdin);
    assert!(child.wait().unwrap().success(), "EOF must end the server cleanly");
}
