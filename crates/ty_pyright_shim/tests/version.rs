//! `--version` answers the installer: `ty-unnecessary <commit>`, exit 0, no root
//! argument (root parsing must not run first).

use std::process::Command;

#[test]
fn version_prints_the_baked_commit() {
    let output = Command::new(env!("CARGO_BIN_EXE_ty-unnecessary"))
        .arg("--version")
        .output()
        .expect("failed to run ty-unnecessary");
    assert!(output.status.success(), "--version must exit 0");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let commit = stdout
        .trim()
        .strip_prefix("ty-unnecessary ")
        .expect("stdout is `ty-unnecessary <commit>`");
    assert!(
        commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit()),
        "commit is a full sha, got {commit:?}"
    );
}
