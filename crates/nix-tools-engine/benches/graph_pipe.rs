//! Measures an end-to-end graph load from a child process pipe.
//!
//! Spawns this binary as its own hermetic producer and parses the child's stdout
//! through `StreamPolicy::Consume`, the same mechanism the engine's graph phase
//! uses. Reports peak resident memory over the whole spawn-and-parse, so the
//! number covers the pipe as well as the parser.
//!
//! Environment overrides:
//!
//! - `NIX_TOOLS_GRAPH_FIXTURE`: payload to emit. Without it the benchmark skips,
//!   so `cargo test --all-targets` does not run it.
//! - `NIX_TOOLS_GRAPH_ROOTS`: file listing required root derivation paths.
//! - `NIX_TOOLS_GRAPH_MAX_NODES`: node limit, default 100000.
//! - `NIX_TOOLS_GRAPH_EMIT`: internal; makes this process the producer child.

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufReader, Read, Write as _};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix_tools_core::process::{
    Cancellation, ProcessRunner, ProcessSpec, StdProcessRunner, StreamConsumer, StreamPolicy,
};
use nix_tools_core::redaction::Redactor;
use nix_tools_engine::{DependencyGraph, EngineError};

/// Parses a derivation graph straight off the child's pipe.
struct GraphConsumer {
    roots: BTreeSet<String>,
    max_nodes: usize,
    graph: Mutex<Option<Result<DependencyGraph, EngineError>>>,
}

impl StreamConsumer for GraphConsumer {
    fn consume(&self, reader: &mut dyn Read) -> std::io::Result<()> {
        let parsed =
            DependencyGraph::from_reader(BufReader::new(reader), &self.roots, self.max_nodes);
        if let Ok(mut slot) = self.graph.lock() {
            *slot = Some(parsed);
        }
        Ok(())
    }
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

    let max_nodes = match env::var("NIX_TOOLS_GRAPH_MAX_NODES") {
        Ok(value) => value.trim().parse::<usize>()?,
        Err(_) => 100_000,
    };
    let roots = match env::var_os("NIX_TOOLS_GRAPH_ROOTS") {
        Some(path) => fs::read_to_string(path)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        None => BTreeSet::new(),
    };
    let payload_bytes = fs::metadata(&fixture)?.len();

    let consumer = Arc::new(GraphConsumer {
        roots,
        max_nodes,
        graph: Mutex::new(None),
    });
    let mut spec = ProcessSpec::new(env::current_exe()?)
        .env("NIX_TOOLS_GRAPH_FIXTURE", &fixture)
        .env("NIX_TOOLS_GRAPH_EMIT", "1");
    spec.stdout = StreamPolicy::Consume {
        consumer: Arc::clone(&consumer) as Arc<dyn StreamConsumer>,
    };

    let runner = StdProcessRunner::without_output(Duration::from_millis(5), Redactor::default());
    let started = Instant::now();
    let result = runner.run(&spec, &Cancellation::default())?;
    let elapsed = started.elapsed();

    let slot = consumer
        .graph
        .lock()
        .map_err(|_| "consumer state poisoned")?;
    let outcome = match slot.as_ref() {
        None => ("not_run".to_owned(), 0),
        Some(Err(error)) => (error.code().to_owned(), 0),
        Some(Ok(graph)) => ("ok".to_owned(), graph.nodes().len()),
    };

    let mut report = String::new();
    writeln!(report, "{{")?;
    writeln!(report, "  \"payload_bytes\": {payload_bytes},")?;
    writeln!(report, "  \"outcome\": \"{}\",", outcome.0)?;
    writeln!(report, "  \"nodes\": {},", outcome.1)?;
    writeln!(
        report,
        "  \"stdout_retained_bytes\": {},",
        result.stdout.bytes.len()
    )?;
    writeln!(
        report,
        "  \"stdout_truncated\": {},",
        result.stdout.truncated
    )?;
    writeln!(report, "  \"wall_nanos\": {},", elapsed.as_nanos())?;
    writeln!(
        report,
        "  \"peak_rss_bytes\": {}",
        peak_rss_bytes().map_or_else(|| "null".to_owned(), |bytes| bytes.to_string())
    )?;
    write!(report, "}}")?;
    println!("{report}");
    Ok(())
}

/// Reports the skip that keeps `cargo test --all-targets` from running the benchmark.
fn skip(reason: &str) {
    println!("{{\"skipped\": \"{reason}\"}}");
}

/// Reads the kernel's peak resident set size for this process.
fn peak_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?
        .trim()
        .strip_suffix(" kB")?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(value * 1024)
}
