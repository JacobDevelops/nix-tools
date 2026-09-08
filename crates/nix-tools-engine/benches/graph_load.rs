//! Measures derivation graph load wall time and peak resident memory.
//!
//! Timing comes from a monotonic clock around a full load of the payload; peak
//! memory comes from the kernel's own `VmHWM`. Each iteration loads the whole
//! payload, so the high-water mark reflects the most expensive single load
//! rather than an accumulation.
//!
//! Environment overrides:
//!
//! - `NIX_TOOLS_GRAPH_FIXTURE`: path to a `nix derivation show` payload. Without
//!   it a deterministic synthetic payload of comparable shape is generated.
//! - `NIX_TOOLS_GRAPH_ROOTS`: file listing required root derivation paths.
//! - `NIX_TOOLS_GRAPH_MODE`: `buffered` or `stream`, default `buffered`.
//! - `NIX_TOOLS_GRAPH_ITERATIONS`: load count, default 15.
//! - `NIX_TOOLS_GRAPH_MAX_NODES`: node limit, default 100000.

mod payload;

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Instant;

use nix_tools_engine::{DependencyGraph, ResourceLimits};

fn main() -> Result<(), Box<dyn Error>> {
    if !payload::is_benchmark_run() {
        payload::skip("run with cargo bench");
        return Ok(());
    }
    let mode = match env::var("NIX_TOOLS_GRAPH_MODE").as_deref() {
        Ok("stream") => Mode::Stream,
        Ok("buffered") | Err(_) => Mode::Buffered,
        Ok(other) => return Err(format!("unknown mode {other}").into()),
    };
    let iterations = payload::parse_env("NIX_TOOLS_GRAPH_ITERATIONS", 15)?;
    let max_nodes = payload::parse_env("NIX_TOOLS_GRAPH_MAX_NODES", 100_000)?;
    let max_retained_bytes = payload::parse_env(
        "NIX_TOOLS_GRAPH_MAX_RETAINED_BYTES",
        ResourceLimits::default().max_graph_retained_bytes,
    )?;
    let (fixture, synthetic) = payload::resolve_fixture()?;
    let roots = resolve_roots()?;
    let payload_bytes = fs::metadata(&fixture)?.len();

    let baseline_peak = payload::peak_rss_bytes();
    let mut samples = Vec::with_capacity(iterations);
    let mut nodes = 0;
    let mut retained = 0;
    for _ in 0..iterations {
        let started = Instant::now();
        let graph = load(&fixture, &roots, max_nodes, max_retained_bytes, mode)?;
        let elapsed = started.elapsed();
        nodes = graph.nodes().len();
        retained = retained_graph_bytes(&graph);
        drop(graph);
        samples.push(elapsed.as_nanos());
    }
    let loaded_peak = payload::peak_rss_bytes();
    samples.sort_unstable();

    let mut report = String::new();
    writeln!(report, "{{")?;
    writeln!(report, "  \"mode\": \"{}\",", mode.as_str())?;
    writeln!(report, "  \"synthetic\": {synthetic},")?;
    writeln!(report, "  \"payload_bytes\": {payload_bytes},")?;
    writeln!(report, "  \"nodes\": {nodes},")?;
    writeln!(report, "  \"retained_graph_bytes\": {retained},")?;
    writeln!(report, "  \"roots\": {},", roots.len())?;
    writeln!(report, "  \"iterations\": {iterations},")?;
    writeln!(report, "  \"wall_nanos_min\": {},", samples[0])?;
    writeln!(
        report,
        "  \"wall_nanos_median\": {},",
        samples[samples.len() / 2]
    )?;
    writeln!(
        report,
        "  \"wall_nanos_max\": {},",
        samples[samples.len() - 1]
    )?;
    writeln!(
        report,
        "  \"peak_rss_bytes_before\": {},",
        payload::render_bytes(baseline_peak)
    )?;
    writeln!(
        report,
        "  \"peak_rss_bytes_after\": {}",
        payload::render_bytes(loaded_peak)
    )?;
    write!(report, "}}")?;
    println!("{report}");
    Ok(())
}

/// Loads the payload through the mode under measurement.
///
/// `buffered` materialises the whole payload first, which is what the `&[u8]`
/// API forces on a caller; `stream` hands the parser a reader and never holds
/// the payload.
fn load(
    fixture: &Path,
    roots: &BTreeSet<String>,
    max_nodes: usize,
    max_retained_bytes: usize,
    mode: Mode,
) -> Result<DependencyGraph, Box<dyn Error>> {
    let graph = match mode {
        Mode::Buffered => {
            let bytes = fs::read(fixture)?;
            DependencyGraph::from_json(&bytes, roots, max_nodes, max_retained_bytes)?
        }
        Mode::Stream => {
            let reader = BufReader::new(File::open(fixture)?);
            DependencyGraph::from_reader(reader, roots, max_nodes, max_retained_bytes)?
        }
    };
    Ok(graph)
}

/// Payload delivery under measurement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    /// Whole payload in memory, then parse.
    Buffered,
    /// Parse straight from a reader.
    Stream,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Buffered => "buffered",
            Self::Stream => "stream",
        }
    }
}

/// Sums the string payload the graph actually keeps, independent of the allocator.
fn retained_graph_bytes(graph: &DependencyGraph) -> usize {
    graph
        .nodes()
        .iter()
        .map(|(path, node)| {
            path.len()
                + node.drv_path.len()
                + node
                    .outputs
                    .iter()
                    .map(|(name, output)| name.len() + output.as_ref().map_or(0, String::len))
                    .sum::<usize>()
                + node
                    .dependencies
                    .iter()
                    .map(|(dependency, outputs)| {
                        dependency.len() + outputs.iter().map(String::len).sum::<usize>()
                    })
                    .sum::<usize>()
        })
        .sum()
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
