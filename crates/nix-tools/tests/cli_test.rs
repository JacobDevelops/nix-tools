//! CLI contract tests for the reference client.

use std::process::Command;

#[test]
fn help_exposes_composable_reference_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_nix-tools"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("build"));
    assert!(stdout.contains("check"));
    assert!(stdout.contains("run"));
    assert!(!stdout.contains("--output <OUTPUT>"));
    assert!(!stdout.contains("--no-tui"));

    let output = Command::new(env!("CARGO_BIN_EXE_nix-tools"))
        .args(["check", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--output <OUTPUT>"));
    assert!(stdout.contains("stream"));
    assert!(stdout.contains("tui"));
}

#[test]
fn plan_emits_deterministic_json() {
    let input = std::env::temp_dir().join(format!("nix-tools-plan-{}.json", std::process::id()));
    std::fs::write(
        &input,
        r#"{"targets":[],"required_roots":[],"history":null,"now_ms":0,"config":{"default_duration_ms":1,"worker_startup_ms":0,"max_workers":1,"max_history_age_ms":1}}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_nix-tools"))
        .args(["plan", input.to_str().unwrap()])
        .output()
        .unwrap();
    std::fs::remove_file(&input).unwrap();

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["plan"]["target_count"], 0);
}
