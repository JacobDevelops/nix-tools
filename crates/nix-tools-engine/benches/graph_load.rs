//! Measures derivation graph load wall time and peak resident memory.
//!
//! Timing comes from a monotonic clock around a full load of the payload; peak
//! memory comes from the kernel's own `VmHWM` high-water mark in
//! `/proc/self/status`, which needs no allocator hook and therefore no `unsafe`.
//! Each iteration loads the whole payload, so the high-water mark reflects the
//! most expensive single load rather than an accumulation.
//!
//! Environment overrides:
//!
//! - `NIX_TOOLS_GRAPH_FIXTURE`: path to a `nix derivation show` payload. When
//!   unset a deterministic synthetic payload of comparable shape is generated.
//! - `NIX_TOOLS_GRAPH_ROOTS`: file listing required root derivation paths, one
//!   per line. When unset no roots are required.
//! - `NIX_TOOLS_GRAPH_ITERATIONS`: load count, default 15.
//! - `NIX_TOOLS_GRAPH_MAX_NODES`: node limit, default 100000.

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use nix_tools_engine::DependencyGraph;

/// Derivation count of the synthetic payload, matching an observed real graph.
const SYNTHETIC_DERIVATIONS: usize = 3757;
/// Inputs per synthetic derivation, matching an observed real graph.
const SYNTHETIC_INPUTS: usize = 7;
/// Discarded `env` entries per synthetic derivation.
const SYNTHETIC_ENV_ENTRIES: usize = 24;
/// Padding per discarded `env` value, sized to reproduce the observed 42% `env` share.
const SYNTHETIC_ENV_PADDING: usize = 34;
/// Nix base32 alphabet used for synthetic store hashes.
const NIX_BASE32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

fn main() -> Result<(), Box<dyn Error>> {
    let iterations = parse_env("NIX_TOOLS_GRAPH_ITERATIONS", 15)?;
    let max_nodes = parse_env("NIX_TOOLS_GRAPH_MAX_NODES", 100_000)?;
    let (fixture, synthetic) = if let Some(path) = env::var_os("NIX_TOOLS_GRAPH_FIXTURE") {
        (PathBuf::from(path), false)
    } else {
        let path = env::temp_dir().join("nix-tools-synthetic-graph.json");
        fs::write(&path, synthetic_payload()?)?;
        (path, true)
    };
    let roots = match env::var_os("NIX_TOOLS_GRAPH_ROOTS") {
        Some(path) => fs::read_to_string(PathBuf::from(path))?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        None => BTreeSet::new(),
    };
    let payload_bytes = fs::metadata(&fixture)?.len();

    let baseline_peak = peak_rss_bytes();
    let mut samples = Vec::with_capacity(iterations);
    let mut nodes = 0;
    let mut retained = 0;
    for _ in 0..iterations {
        let started = Instant::now();
        let graph = load(&fixture, &roots, max_nodes)?;
        let elapsed = started.elapsed();
        nodes = graph.nodes().len();
        retained = retained_graph_bytes(&graph);
        drop(graph);
        samples.push(elapsed.as_nanos());
    }
    let loaded_peak = peak_rss_bytes();
    samples.sort_unstable();

    let mut report = String::new();
    writeln!(report, "{{")?;
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
        render_bytes(baseline_peak)
    )?;
    writeln!(
        report,
        "  \"peak_rss_bytes_after\": {}",
        render_bytes(loaded_peak)
    )?;
    write!(report, "}}")?;
    println!("{report}");
    Ok(())
}

/// Loads the payload the way a caller must with the current API.
///
/// The whole payload is materialised before parsing, so the peak covers both the
/// captured bytes and whatever the parser retains.
fn load(
    fixture: &PathBuf,
    roots: &BTreeSet<String>,
    max_nodes: usize,
) -> Result<DependencyGraph, Box<dyn Error>> {
    let bytes = fs::read(fixture)?;
    let graph = DependencyGraph::from_json(&bytes, roots, max_nodes)?;
    Ok(graph)
}

fn render_bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_owned(), |bytes| bytes.to_string())
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

fn parse_env(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
    match env::var(name) {
        Ok(value) => Ok(value.trim().parse::<usize>()?),
        Err(_) => Ok(default),
    }
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

/// Builds a deterministic payload with the field mix of a real nix graph.
fn synthetic_payload() -> Result<String, std::fmt::Error> {
    let mut payload = String::with_capacity(16 * 1024 * 1024);
    payload.push_str("{\"derivations\":{");
    for index in 0..SYNTHETIC_DERIVATIONS {
        if index > 0 {
            payload.push(',');
        }
        write_derivation(&mut payload, index)?;
    }
    payload.push_str("},\"version\":4}");
    Ok(payload)
}

fn write_derivation(payload: &mut String, index: usize) -> Result<(), std::fmt::Error> {
    let padding = "x".repeat(SYNTHETIC_ENV_PADDING);
    write!(
        payload,
        "\"{}-pkg-{index}.drv\":{{\"args\":[\"-e\",\"/nix/store/{}-builder.sh\"],\
         \"builder\":\"/nix/store/{}-bash-5.3p9/bin/bash\",\"env\":{{",
        store_hash(index),
        store_hash(index + 1_000_000),
        store_hash(2_000_000)
    )?;
    for entry in 0..SYNTHETIC_ENV_ENTRIES {
        if entry > 0 {
            payload.push(',');
        }
        write!(
            payload,
            "\"buildInput{entry}\":\"/nix/store/{}-dep-{entry}-{padding}\"",
            store_hash(index * 64 + entry + 3_000_000)
        )?;
    }
    payload.push_str("},\"inputs\":{\"drvs\":{");
    for offset in 1..=SYNTHETIC_INPUTS {
        let Some(dependency) = index.checked_sub(offset) else {
            break;
        };
        if offset > 1 {
            payload.push(',');
        }
        write!(
            payload,
            "\"{}-pkg-{dependency}.drv\":{{\"dynamicOutputs\":{{}},\"outputs\":[\"out\"]}}",
            store_hash(dependency)
        )?;
    }
    write!(
        payload,
        "}},\"srcs\":[\"{}-source\"]}},\"name\":\"pkg-{index}\",\
         \"outputs\":{{\"devdoc\":{{\"path\":\"{}-pkg-{index}-devdoc\"}},\
         \"out\":{{\"path\":\"{}-pkg-{index}\"}}}},\
         \"system\":\"x86_64-linux\",\"version\":4}}",
        store_hash(index + 4_000_000),
        store_hash(index + 5_000_000),
        store_hash(index + 6_000_000)
    )
}

/// Produces a unique 32-character nix-style hash for a seed.
fn store_hash(seed: usize) -> String {
    let mut hash = String::with_capacity(32);
    for position in 0..25 {
        let index = (position * 7 + 11) % NIX_BASE32.len();
        hash.push(char::from(NIX_BASE32[index]));
    }
    let mut remaining = seed;
    let mut digits = [0_u8; 7];
    for digit in &mut digits {
        *digit = NIX_BASE32[remaining % NIX_BASE32.len()];
        remaining /= NIX_BASE32.len();
    }
    for digit in digits {
        hash.push(char::from(digit));
    }
    hash
}
