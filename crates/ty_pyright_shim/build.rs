//! Bakes the commit `--version` reports: the caller's `TY_UNNECESSARY_COMMIT`
//! (sightline's installer passes the checkout's HEAD), else `git rev-parse HEAD`,
//! else `unknown`.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-env-changed=TY_UNNECESSARY_COMMIT");
    let root = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    // A commit on the current branch leaves .git/HEAD untouched, so watch the ref it names.
    if let Ok(head) = std::fs::read_to_string(root.join(".git/HEAD")) {
        println!("cargo::rerun-if-changed={}", root.join(".git/HEAD").display());
        if let Some(git_ref) = head.strip_prefix("ref:").map(str::trim) {
            let path = root.join(".git").join(git_ref);
            let watched = if path.exists() { path } else { root.join(".git/packed-refs") };
            println!("cargo::rerun-if-changed={}", watched.display());
        }
    }
    let commit = std::env::var("TY_UNNECESSARY_COMMIT")
        .ok()
        .or_else(|| {
            let out = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&root)
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8(out.stdout).ok())?
        })
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());
    println!("cargo::rustc-env=SHIM_COMMIT={commit}");
}
