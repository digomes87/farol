//! End-to-end tests driving the real binary, the way a user does.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../corpus")
}

fn farol(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_farol"))
        .args(args)
        .output()
        .expect("the binary is built by cargo test")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Builds an index inside a temporary directory and returns its path.
fn indexed(dir: &Path) -> String {
    let index = dir.join("test.idx").display().to_string();
    let output = farol(&["index", &corpus().display().to_string(), "-i", &index]);
    assert!(output.status.success(), "{}", stdout(&output));
    index
}

#[test]
fn index_then_search_then_stats() {
    let dir = tempfile::tempdir().unwrap();
    let index = indexed(dir.path());

    let search = farol(&["search", "stemming", "-i", &index, "-n", "3"]);
    assert!(search.status.success());
    let text = stdout(&search);
    assert!(text.contains("text-analysis.md"), "{text}");
    // Piped output must be plain: no ANSI escapes.
    assert!(!text.contains('\x1b'), "piped output was colored");

    let stats = farol(&["stats", "-i", &index]);
    assert!(stdout(&stats).contains("documents"));
}

#[test]
fn json_output_is_machine_readable() {
    let dir = tempfile::tempdir().unwrap();
    let index = indexed(dir.path());

    let output = farol(&["search", "+positions", "-i", &index, "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid JSON");
    assert!(parsed["count"].as_u64().unwrap() >= 1);
    assert!(parsed["results"][0]["uri"].is_string());
}

#[test]
fn ranking_parameters_are_wired_through() {
    let dir = tempfile::tempdir().unwrap();
    let index = indexed(dir.path());

    let default = farol(&["search", "index", "-i", &index, "--json"]);
    let tuned = farol(&["search", "index", "-i", &index, "--json", "--b", "0.0"]);

    let default: serde_json::Value = serde_json::from_str(&stdout(&default)).unwrap();
    let tuned: serde_json::Value = serde_json::from_str(&stdout(&tuned)).unwrap();
    assert_ne!(
        default["results"][0]["score"], tuned["results"][0]["score"],
        "disabling length normalisation should change the score"
    );
}

#[test]
fn a_missing_index_fails_with_a_helpful_message() {
    let output = farol(&["search", "rust", "-i", "/tmp/does-not-exist.idx"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("farol index"), "{stderr}");
}

#[test]
fn a_malformed_query_fails_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let index = indexed(dir.path());

    let output = farol(&["search", "\"unterminated", "-i", &index]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unterminated quote"), "{stderr}");
    assert!(!stderr.contains("panicked"));
}
