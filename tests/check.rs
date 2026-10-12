//! End-to-end coverage for the `tiny-dfr check` subcommand: the same
//! binary users and CI run, proving exit codes and diagnostics without
//! opening the Touch Bar.

use std::process::Command;

fn check(args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_tiny-dfr"))
        .arg("check")
        .args(args)
        .output()
        .expect("the tiny-dfr binary runs");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn the_shipped_example_checks_clean_over_the_repo_share_directory() {
    let (code, stdout, stderr) = check(&[
        "--base",
        "share/tiny-dfr/config.toml",
        "--icon-root",
        "share/tiny-dfr",
        "share/tiny-dfr/multibar.toml",
    ]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("OK"), "{stdout}");
    assert!(stdout.contains("multibar.toml"), "{stdout}");
}

#[test]
fn an_unknown_field_exits_nonzero_with_the_line_and_a_suggestion() {
    let path =
        std::env::temp_dir().join(format!("tiny-dfr-check-test-{}.toml", std::process::id()));
    std::fs::write(&path, "ShowButtonOutline = false\n").unwrap();
    let (code, stdout, stderr) = check(&[
        "--base",
        "share/tiny-dfr/config.toml",
        "--icon-root",
        "share/tiny-dfr",
        &path.to_string_lossy(),
    ]);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(code, Some(1), "stdout: {stdout} stderr: {stderr}");
    assert!(
        stderr.contains("unknown field `ShowButtonOutline`"),
        "{stderr}"
    );
    assert!(
        stderr.contains("did you mean `ShowButtonOutlines`"),
        "{stderr}"
    );
    assert!(
        stderr.contains("line 1"),
        "the TOML line rides along: {stderr}"
    );
}

#[test]
fn bad_usage_exits_two_and_help_exits_zero() {
    let (code, _, stderr) = check(&["--nonsense"]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("usage"), "{stderr}");
    let (code, stdout, _) = check(&["--help"]);
    assert_eq!(code, Some(0));
    assert!(stdout.contains("usage"), "{stdout}");
}
