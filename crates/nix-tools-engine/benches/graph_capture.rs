//! Measures whether the configured process output limit truncates a graph payload.
//!
//! Spawns this binary as its own child so the producer is hermetic, captures the
//! child's stdout under the engine's default `max_process_output_bytes`, and
//! reports whether the capture truncated. Truncation is what the engine turns
//! into `process_output_limit_exceeded` before the graph is ever parsed.
//!
//! Environment overrides:
//!
//! - `NIX_TOOLS_GRAPH_FIXTURE`: payload to emit. Without it the benchmark skips,
//!   so `cargo test --all-targets` does not run it.
//! - `NIX_TOOLS_GRAPH_CAPTURE_LIMIT`: capture limit, default 8388608.
//! - `NIX_TOOLS_GRAPH_EMIT`: internal; makes this process the producer child.

use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::time::Duration;

use nix_tools_core::process::{
    Cancellation, ProcessRunner, ProcessSpec, StdProcessRunner, StreamPolicy,
};
use nix_tools_core::redaction::Redactor;

/// Engine default for `ResourceLimits::max_process_output_bytes`.
const DEFAULT_CAPTURE_LIMIT: usize = 8 * 1024 * 1024;

/// Reports the skip that keeps `cargo test --all-targets` from running the benchmark.
fn skip(reason: &str) {
    println!("{{\"skipped\": \"{reason}\"}}");
}

fn main() -> Result<(), Box<dyn Error>> {
    let Ok(fixture) = env::var("NIX_TOOLS_GRAPH_FIXTURE") else {
        skip("set NIX_TOOLS_GRAPH_FIXTURE");
        return Ok(());
    };
    if env::var_os("NIX_TOOLS_GRAPH_EMIT").is_some() {
        std::io::stdout().lock().write_all(&fs::read(&fixture)?)?;
        return Ok(());
    }

    let limit = match env::var("NIX_TOOLS_GRAPH_CAPTURE_LIMIT") {
        Ok(value) => value.trim().parse::<usize>()?,
        Err(_) => DEFAULT_CAPTURE_LIMIT,
    };
    let payload_bytes = fs::metadata(&fixture)?.len();
    let mut spec = ProcessSpec::new(env::current_exe()?)
        .env("NIX_TOOLS_GRAPH_FIXTURE", &fixture)
        .env("NIX_TOOLS_GRAPH_EMIT", "1");
    spec.stdout = StreamPolicy::Capture { limit };
    spec.stderr = StreamPolicy::Capture { limit };

    let runner = StdProcessRunner::without_output(Duration::from_millis(5), Redactor::default());
    let result = runner.run(&spec, &Cancellation::default())?;

    let mut report = String::new();
    writeln!(report, "{{")?;
    writeln!(report, "  \"payload_bytes\": {payload_bytes},")?;
    writeln!(report, "  \"capture_limit_bytes\": {limit},")?;
    writeln!(
        report,
        "  \"captured_bytes\": {},",
        result.stdout.bytes.len()
    )?;
    writeln!(report, "  \"truncated\": {},", result.stdout.truncated)?;
    writeln!(
        report,
        "  \"child_succeeded\": {}",
        result.termination.success()
    )?;
    write!(report, "}}")?;
    println!("{report}");
    Ok(())
}
