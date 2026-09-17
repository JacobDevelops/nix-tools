//! Subprocess-level protocol lifecycle checks.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

#[test]
fn engine_rejects_explicit_global_configuration_before_starting_transport() {
    for interactive in [false, true] {
        for after_command in [false, true] {
            for option in [
                ["--nix", "nix"],
                ["--nix", "/unused-nix"],
                ["--substituter", "https://cache.example"],
                ["--trusted-public-key", "cache:key"],
            ] {
                let mut command = Command::new(env!("CARGO_BIN_EXE_nix-tools"));
                if !after_command {
                    command.args(option);
                }
                command.arg("engine");
                if interactive {
                    command.arg("--interactive");
                }
                if after_command {
                    command.args(option);
                }
                let output = command.output().unwrap();
                assert_eq!(output.status.code(), Some(2), "{option:?}");
                assert!(output.stdout.is_empty(), "transport started for {option:?}");
                assert!(String::from_utf8_lossy(&output.stderr).contains("request"));
            }
        }
    }
}

#[test]
fn invalid_attribute_components_are_usage_errors_and_process_errors_stay_external() {
    for (operation, category) in [("build_installables", "usage"), ("discover", "external")] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nix-tools"))
            .arg("engine")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let mut request = json!({"type":"request","version":1,"id":"invalid","operation":operation,"config":{"system":"x86_64-linux","nix_executable":"/nonexistent-nix-tools-review-test"},"flake":{"reference":"."}});
        if operation == "build_installables" {
            request["attribute_paths"] = json!([[""]]);
        }
        writeln!(input, "{request}").unwrap();
        let terminal: Value = loop {
            line.clear();
            assert!(output.read_line(&mut line).unwrap() > 0);
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["type"] != "progress" {
                break message;
            }
        };
        assert_eq!(terminal["type"], "error");
        assert_eq!(terminal["error"]["category"], category);
        if category == "usage" {
            assert_eq!(terminal["error"]["exit_code"], 2);
        }
        assert!(child.wait().unwrap().success());
    }
}

#[cfg(unix)]
#[test]
fn closing_stdin_cancels_an_active_nix_child() {
    use std::os::unix::fs::PermissionsExt;
    let directory =
        std::env::temp_dir().join(format!("nix-tools-protocol-eof-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let nix = directory.join("nix");
    let marker = directory.join("started");
    std::fs::write(
        &nix,
        format!(
            "#!/bin/sh\n: > '{}'\nwhile :; do :; done\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&nix, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nix-tools"))
        .arg("engine")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    let request = json!({"type":"request","version":1,"id":"1","operation":"flake_check","config":{"system":"x86_64-linux","nix_executable":nix},"flake":{"reference":"."}});
    writeln!(input, "{request}").unwrap();
    for _ in 0..200 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(input);
    let terminal: Value = loop {
        line.clear();
        assert!(output.read_line(&mut line).unwrap() > 0);
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["type"] != "progress" {
            break message;
        }
    };
    assert!(child.wait().unwrap().success());
    assert!(marker.exists());
    assert_eq!(terminal["type"], "result");
    assert_eq!(terminal["result"]["manifest"]["outcome"], "cancelled");
    assert_eq!(terminal["signal"], 15);
    assert_eq!(terminal["failure"]["exit_code"], 143);
    std::fs::remove_file(marker).unwrap();
    std::fs::remove_file(nix).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn invalid_version_has_one_terminal_error_and_clean_stdout() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nix-tools"))
        .arg("engine")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    let hello: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(hello["type"], "hello");
    let request = json!({"type":"request","version":99,"id":"bad","operation":"discover","config":{"system":"x86_64-linux"},"flake":{"reference":"."}});
    writeln!(input, "{request}").unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    let terminal: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(terminal["type"], "error");
    assert_eq!(terminal["id"], "bad");
    assert_eq!(terminal["error"]["category"], "usage");
    assert_eq!(terminal["error"]["exit_code"], 2);
    line.clear();
    assert_eq!(output.read_line(&mut line).unwrap(), 0);
    assert!(child.wait().unwrap().success());
}

#[test]
fn invalid_utf8_is_a_structured_error() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nix-tools"))
        .arg("engine")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"\xff\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let frames: Vec<Value> = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1]["type"], "error");
    assert_eq!(frames[1]["error"]["category"], "usage");
}
