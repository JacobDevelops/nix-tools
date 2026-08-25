//! Command-line interface for Bun lockfile conversion and cache entry creation.

use std::{
    fs,
    io::{self, Write as _},
    path::PathBuf,
    process::ExitCode,
};

use bun2nix::{ConvertOptions, Result, convert_lockfile, create_cache_entry, inspect_lockfile};
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(flatten)]
    convert: ConvertArgs,

    #[command(subcommand)]
    command: Option<Operation>,
}

#[derive(Clone, Debug, Args)]
struct ConvertArgs {
    /// JSONC Bun lockfile to convert.
    #[arg(short = 'l', long, default_value = "bun.lock")]
    lock_file: PathBuf,

    /// Output file; stdout is used when omitted.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Relative prefix from bun.nix to local package sources.
    #[arg(long, default_value = ".")]
    copy_prefix: String,
}

#[derive(Debug, Subcommand)]
enum Operation {
    /// Converts a lockfile to the canonical structured bun.nix expression.
    Convert(ConvertArgs),

    /// Emits a deterministic JSON dependency plan without fetching sources.
    #[command(alias = "plan")]
    Inspect(InspectArgs),

    /// Creates one Bun-compatible install-cache symlink.
    CacheEntry(CacheEntryArgs),
}

#[derive(Debug, Args)]
struct InspectArgs {
    /// JSONC Bun lockfile to inspect.
    #[arg(short = 'l', long, default_value = "bun.lock")]
    lock_file: PathBuf,

    /// Output file; stdout is used when omitted.
    #[arg(short, long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct CacheEntryArgs {
    /// Bun cache directory to populate.
    #[arg(long)]
    out: PathBuf,

    /// Raw package resolution from bun.lock.
    #[arg(long)]
    name: String,

    /// Extracted package directory to link.
    #[arg(long)]
    package: PathBuf,

    /// Hostname for a non-default npm registry.
    #[arg(long)]
    registry: Option<String>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bun2nix: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None => convert(cli.convert),
        Some(Operation::Convert(args)) => convert(args),
        Some(Operation::Inspect(args)) => inspect(args),
        Some(Operation::CacheEntry(args)) => {
            create_cache_entry(
                &args.out,
                &args.name,
                &args.package,
                args.registry.as_deref(),
            )?;
            Ok(())
        }
    }
}

fn convert(args: ConvertArgs) -> Result<()> {
    let lockfile = fs::read_to_string(args.lock_file)?;
    let expression = convert_lockfile(
        &lockfile,
        &ConvertOptions {
            copy_prefix: args.copy_prefix,
        },
    )?;
    write_output(args.output, &expression)
}

fn inspect(args: InspectArgs) -> Result<()> {
    let lockfile = fs::read_to_string(args.lock_file)?;
    let mut json = serde_json::to_string_pretty(&inspect_lockfile(&lockfile)?)?;
    json.push('\n');
    write_output(args.output, &json)
}

fn write_output(path: Option<PathBuf>, contents: &str) -> Result<()> {
    if let Some(path) = path {
        fs::write(path, contents)?;
    } else {
        io::stdout().lock().write_all(contents.as_bytes())?;
    }
    Ok(())
}
