//! Measures the realized-app handoff independently of Nix evaluation.

#[path = "../src/app_exec.rs"]
mod app_exec;

use std::time::Duration;

#[cfg(test)]
use nix_tools::forward_termination_signals;

use nix_tools_core::process::{
    Cancellation, ProcessRunner, ProcessSpec, StdProcessRunner, StreamPolicy,
};
use nix_tools_core::redaction::Redactor;

fn main() {
    let mode = std::env::args().nth(1).expect("supervised or exec");
    let cancellation = Cancellation::default();
    nix_tools::forward_termination_signals(&cancellation).unwrap();
    let mut spec = if std::env::args().nth(2).as_deref() == Some("wait") {
        ProcessSpec::new("/bin/sleep").arg("60")
    } else {
        ProcessSpec::new("/bin/dd").args(["if=/dev/zero", "bs=65536", "count=8192", "status=none"])
    };
    if mode == "exec" {
        app_exec::replace_process(&spec, &cancellation).unwrap();
    } else {
        spec.stdout = StreamPolicy::RelayAndCapture {
            limit: 8 * 1024 * 1024,
        };
        spec.stderr = StreamPolicy::RelayAndCapture {
            limit: 8 * 1024 * 1024,
        };
        let runner = StdProcessRunner::new(Duration::from_millis(20), Redactor::default());
        let code = match runner.run(&spec, &cancellation) {
            Ok(result) if result.termination.success() => 0,
            Ok(result) => result.termination.exit_code().get(),
            Err(error) => error.exit_code.get(),
        };
        std::process::exit(i32::from(code));
    }
}
