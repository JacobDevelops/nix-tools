use super::*;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

#[test]
fn exec_helper() {
    let Some(mode) = std::env::var_os("NIX_TOOLS_EXEC_TEST") else {
        return;
    };
    let mut spec = ProcessSpec::new("/bin/sh").args([
        OsString::from("-c"),
        if mode == "signal" {
            OsString::from("kill -TERM $$")
        } else {
            OsString::from(
                "test \"$$\" = \"$EXEC_PID\" || exit 92; printf '%s\\n' \"$PWD\" \"$EXEC_VALUE\"; printf '%s' \"$1\"; printf '%s' \"$1\" >&2; test -z \"${NIX_TOOLS_EXEC_TEST+x}\" || exit 91; IFS= read -r input; printf '%s\\n' \"$input\"; exit 23",
            )
        },
        OsString::from("app"),
        OsString::from_vec(vec![b'a', 0xff]),
    ]);
    spec.cwd = Some(std::env::temp_dir());
    spec.env
        .insert("EXEC_PID".into(), std::process::id().to_string().into());
    spec.env.insert("EXEC_VALUE".into(), "isolated".into());
    crate::forward_termination_signals(&Cancellation::default()).unwrap();
    replace_process(&spec, &Cancellation::default()).unwrap();
}

fn helper(mode: &str) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "app_exec::tests::exec_helper", "--nocapture"])
        .env("NIX_TOOLS_EXEC_TEST", mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn exec_preserves_pid_raw_arguments_stdin_environment_cwd_and_status() {
    use std::io::Write;
    let mut child = helper("output");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&[0xfe, b'\n'])
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(23));
    let expected = format!("{}\nisolated\na", std::env::temp_dir().display());
    let mut expected = expected.into_bytes();
    expected.extend([0xff, 0xfe, b'\n']);
    assert!(output.stdout.ends_with(&expected), "{:?}", output.stdout);
    assert_eq!(output.stderr, [b'a', 0xff]);
}

#[test]
fn exec_preserves_signal_termination_after_runtime_handlers() {
    let output = helper("signal").wait_with_output().unwrap();
    assert_eq!(output.status.signal(), Some(15));
}

#[test]
fn exec_checks_cancellation_before_replacement() {
    let cancellation = Cancellation::default();
    cancellation.request(15);
    let error = replace_process(&ProcessSpec::new("/does/not/exist"), &cancellation).unwrap_err();
    assert_eq!(error.exit_code.get(), 143);
}

#[test]
fn exec_reports_setup_failure() {
    let error = replace_process(
        &ProcessSpec::new("/does/not/exist"),
        &Cancellation::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("/does/not/exist"));
}
