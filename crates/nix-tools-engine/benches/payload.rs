//! Fixture resolution and process measurement shared by the graph benchmarks.
//!
//! Every benchmark here is self-sufficient: without `NIX_TOOLS_GRAPH_FIXTURE` it
//! generates a deterministic payload of the same shape as a real
//! `nix derivation show --recursive`, so no private fixture is needed to run one.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

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
static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct Fixture {
    pub path: PathBuf,
    pub synthetic: bool,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.synthetic {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Whether cargo invoked this binary as a benchmark rather than as a test.
///
/// `cargo bench` passes `--bench`; `cargo test --all-targets` runs the same
/// binary with no arguments, and a benchmark must not spend that gate's time.
#[must_use]
pub fn is_benchmark_run() -> bool {
    env::args().any(|argument| argument == "--bench")
}

/// Reports the skip that keeps `cargo test --all-targets` from running a benchmark.
pub fn skip(reason: &str) {
    println!("{{\"skipped\": \"{reason}\"}}");
}

/// Resolves the payload to measure, generating a synthetic one when none is named.
///
/// # Errors
///
/// Returns an error when the synthetic payload cannot be written.
pub fn resolve_fixture() -> io::Result<Fixture> {
    if let Some(path) = env::var_os("NIX_TOOLS_GRAPH_FIXTURE") {
        return Ok(Fixture {
            path: PathBuf::from(path),
            synthetic: false,
        });
    }
    create_synthetic_fixture(&env::temp_dir())
}

fn create_synthetic_fixture(directory: &std::path::Path) -> io::Result<Fixture> {
    for _ in 0..100 {
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "nix-tools-synthetic-graph-{}-{sequence}.json",
            std::process::id()
        ));
        let file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = write_synthetic_payload(file) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        return Ok(Fixture {
            path,
            synthetic: true,
        });
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique synthetic graph fixture",
    ))
}

/// Reads a positive numeric environment override.
///
/// # Errors
///
/// Returns an error when the variable is set but not a number.
pub fn parse_env(name: &str, default: usize) -> Result<usize, std::num::ParseIntError> {
    match env::var(name) {
        Ok(value) => value.trim().parse::<usize>(),
        Err(_) => Ok(default),
    }
}

/// Reads the kernel's peak resident set size for this process.
///
/// This is the kernel's own high-water mark, so it needs no allocator hook and
/// therefore no `unsafe`, and it survives the allocator not returning pages.
#[must_use]
pub fn peak_rss_bytes() -> Option<u64> {
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

/// Renders an optional byte count as JSON.
#[must_use]
pub fn render_bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_owned(), |bytes| bytes.to_string())
}

/// Builds a deterministic payload with the field mix of a real nix graph.
fn write_synthetic_payload(file: File) -> io::Result<()> {
    let mut payload = BufWriter::new(file);
    payload.write_all(b"{\"derivations\":{")?;
    for index in 0..SYNTHETIC_DERIVATIONS {
        if index > 0 {
            payload.write_all(b",")?;
        }
        write_derivation(&mut payload, index)?;
    }
    payload.write_all(b"},\"version\":4}")?;
    payload.flush()
}

fn write_derivation(payload: &mut impl io::Write, index: usize) -> io::Result<()> {
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
            payload.write_all(b",")?;
        }
        write!(
            payload,
            "\"buildInput{entry}\":\"/nix/store/{}-dep-{entry}-{padding}\"",
            store_hash(index * 64 + entry + 3_000_000)
        )?;
    }
    payload.write_all(b"},\"inputs\":{\"drvs\":{")?;
    for offset in 1..=SYNTHETIC_INPUTS {
        let Some(dependency) = index.checked_sub(offset) else {
            break;
        };
        if offset > 1 {
            payload.write_all(b",")?;
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

#[cfg(test)]
#[path = "payload_test.rs"]
mod tests;
