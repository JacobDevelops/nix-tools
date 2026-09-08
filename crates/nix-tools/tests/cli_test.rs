//! CLI contract tests for the reference client.

use std::process::Command;

#[cfg(unix)]
#[test]
fn scoped_run_passes_exact_target_and_arguments_through_the_engine() {
    use std::os::unix::fs::PermissionsExt;
    let directory =
        std::env::temp_dir().join(format!("nix-tools-scoped-run-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let nix = directory.join("nix");
    let app = directory.join("app");
    std::fs::write(&app, "#!/bin/sh\n[ \"$#\" = 2 ] && [ \"$1\" = 'two words' ] && [ \"$2\" = '--flag' ] || exit 20\nprintf 'arguments preserved\\n'\n").unwrap();
    std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o755)).unwrap();
    let response = serde_json::json!({"program": app, "context": {}}).to_string();
    std::fs::write(
        &nix,
        format!(
            "#!/bin/sh\n[ \"$NIX_TOOLS_ENGINE_APP\" = 'web:dev' ] || exit 19\nprintf '%s' '{}'\n",
            response.replace('\'', "'\\''")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&nix, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_nix-tools"))
        .args([
            "--nix",
            nix.to_str().unwrap(),
            "run",
            "web:dev",
            "--output=stream",
            "--",
            "two words",
            "--flag",
        ])
        .output()
        .unwrap();
    std::fs::remove_file(&nix).unwrap();
    std::fs::remove_file(&app).unwrap();
    std::fs::remove_dir(&directory).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("arguments preserved"));
}

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
    assert!(stdout.contains("default: tui"));
}

#[test]
fn version_reports_the_release_without_starting_nix() {
    let output = Command::new(env!("CARGO_BIN_EXE_nix-tools"))
        .arg("--version")
        .output()
        .expect("run nix-tools --version");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 version"),
        format!("nix-tools {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
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
