//! Regression witness for the bounded-capture defect the streaming parse removed.
//!
//! This does not measure current behaviour. The engine's graph phase no longer
//! captures `nix derivation show` into a bounded buffer, so this reproduces what
//! used to happen: a payload larger than `max_process_output_bytes` was drained
//! and truncated, and the engine raised `process_output_limit_exceeded` before
//! the graph was ever parsed. Keeping it executable means a return to bounded
//! capture would be visible rather than argued about.
//!
//! Environment overrides:
//!
//! - `NIX_TOOLS_GRAPH_FIXTURE`: payload to emit. Without it a deterministic
//!   synthetic payload of comparable shape is generated.
//! - `NIX_TOOLS_GRAPH_CAPTURE_LIMIT`: capture limit, default 8388608.
//! - `NIX_TOOLS_GRAPH_EMIT`: internal; makes this process the producer child.

mod payload;

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

/// Former engine default for `ResourceLimits::max_process_output_bytes`.
const DEFAULT_CAPTURE_LIMIT: usize = 8 * 1024 * 1024;

fn main() -> Result<(), Box<dyn Error>> {
    if let Some(fixture) = env::var_os("NIX_TOOLS_GRAPH_EMIT") {
        std::io::stdout().lock().write_all(&fs::read(fixture)?)?;
        return Ok(());
    }
    if !payload::is_benchmark_run() {
        payload::skip("run with cargo bench");
        return Ok(());
    }

    let limit = payload::parse_env("NIX_TOOLS_GRAPH_CAPTURE_LIMIT", DEFAULT_CAPTURE_LIMIT)?;
    let (fixture, synthetic) = payload::resolve_fixture()?;
    let payload_bytes = fs::metadata(&fixture)?.len();

    let mut spec = ProcessSpec::new(env::current_exe()?).env("NIX_TOOLS_GRAPH_EMIT", &fixture);
    spec.stdout = StreamPolicy::Capture { limit };
    spec.stderr = StreamPolicy::Capture { limit };

    let runner = StdProcessRunner::without_output(Duration::from_millis(5), Redactor::default());
    let result = runner.run(&spec, &Cancellation::default())?;

    let mut report = String::new();
    writeln!(report, "{{")?;
    writeln!(report, "  \"synthetic\": {synthetic},")?;
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
        "  \"child_succeeded\": {},",
        result.termination.success()
    )?;
    writeln!(
        report,
        "  \"peak_rss_bytes\": {}",
        payload::render_bytes(payload::peak_rss_bytes())
    )?;
    write!(report, "}}")?;
    println!("{report}");
    Ok(())
}
