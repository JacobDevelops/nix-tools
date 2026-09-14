//! Subprocess-level protocol lifecycle checks.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

#[cfg(unix)]
#[test]
fn closing_stdin_cancels_an_active_nix_child() {
    use std::os::unix::fs::PermissionsExt;
    let directory =
        std::env::temp_dir().join(format!("nix-tools-protocol-eof-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let nix = directory.join("nix");
    std::fs::write(
        &nix,
        "#!/bin/sh\nprintf 'started\\n' >&2\nwhile :; do :; done\n",
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
    loop {
        line.clear();
        assert!(output.read_line(&mut line).unwrap() > 0);
        let started: Value = serde_json::from_str(&line).unwrap();
        if started["event"]["data"]["line"] == "started\n" {
            break;
        }
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
    assert_eq!(terminal["type"], "error");
    assert_eq!(terminal["error"]["category"], "cancelled");
    assert_eq!(terminal["error"]["signal"], 15);
    assert_eq!(terminal["error"]["exit_code"], 143);
    assert!(child.wait().unwrap().success());
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
