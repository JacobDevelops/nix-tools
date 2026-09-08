//! Measures an end-to-end graph load from a child process pipe.
//!
//! Spawns this binary as its own hermetic producer and parses the child's stdout
//! through `StreamPolicy::Consume`, the same mechanism the engine's graph phase
//! uses. Peak resident memory covers the whole spawn-and-parse, so the number
//! includes the pipe as well as the parser.
//!
//! Environment overrides:
//!
//! - `NIX_TOOLS_GRAPH_FIXTURE`: payload to emit. Without it a deterministic
//!   synthetic payload of comparable shape is generated.
//! - `NIX_TOOLS_GRAPH_ROOTS`: file listing required root derivation paths.
//! - `NIX_TOOLS_GRAPH_MAX_NODES`: node limit, default 100000.
//! - `NIX_TOOLS_GRAPH_EMIT`: internal; makes this process the producer child.

mod payload;

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
    if let Some(fixture) = env::var_os("NIX_TOOLS_GRAPH_EMIT") {
        std::io::stdout().lock().write_all(&fs::read(fixture)?)?;
        return Ok(());
    }
    if !payload::is_benchmark_run() {
        payload::skip("run with cargo bench");
        return Ok(());
    }

    let max_nodes = payload::parse_env("NIX_TOOLS_GRAPH_MAX_NODES", 100_000)?;
    let (fixture, synthetic) = payload::resolve_fixture()?;
    let roots = resolve_roots()?;
    let payload_bytes = fs::metadata(&fixture)?.len();

    let consumer = Arc::new(GraphConsumer {
        roots,
        max_nodes,
        graph: Mutex::new(None),
    });
    let mut spec = ProcessSpec::new(env::current_exe()?).env("NIX_TOOLS_GRAPH_EMIT", &fixture);
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
    let (outcome, nodes) = match slot.as_ref() {
        None => ("not_run", 0),
        Some(Err(error)) => (error.code(), 0),
        Some(Ok(graph)) => ("ok", graph.nodes().len()),
    };

    let mut report = String::new();
    writeln!(report, "{{")?;
    writeln!(report, "  \"synthetic\": {synthetic},")?;
    writeln!(report, "  \"payload_bytes\": {payload_bytes},")?;
    writeln!(report, "  \"outcome\": \"{outcome}\",")?;
    writeln!(report, "  \"nodes\": {nodes},")?;
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
        payload::render_bytes(payload::peak_rss_bytes())
    )?;
    write!(report, "}}")?;
    println!("{report}");
    Ok(())
}

/// Reads required root derivation paths, one per line.
fn resolve_roots() -> std::io::Result<BTreeSet<String>> {
    let Some(path) = env::var_os("NIX_TOOLS_GRAPH_ROOTS") else {
        return Ok(BTreeSet::new());
    };
    Ok(fs::read_to_string(path)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}
