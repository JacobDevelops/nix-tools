#![forbid(unsafe_code)]

//! Thin reference client for the composable `nix-tools` library.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use nix_tools::{
    AppExecutionPolicy, CheckSelector, OutputMode, Runtime, RuntimeCommand, RuntimeConfig,
    RuntimeDependencies, SelectedCheckCommand, forward_termination_signals, plan_json,
};
use nix_tools_core::outcome::Error;
use nix_tools_core::process::{Cancellation, StdProcessRunner};
use nix_tools_core::redaction::Redactor;
use nix_tools_core::system::NixSystem;
use nix_tools_engine::{EngineConfig, FlakeRef, SystemClock, TrustedSubstituter};

#[derive(Debug, Parser)]
#[command(
    name = "nix-tools",
    version,
    about = "Reference client for reusable Nix flake tooling"
)]
struct Cli {
    /// Nix executable path supplied to the engine.
    #[arg(long, global = true, default_value = "nix")]
    nix: String,
    /// Additional trusted binary-cache URL; repeat with one `--trusted-public-key` per URL.
    #[arg(long, global = true)]
    substituter: Vec<String>,
    /// Signing key paired by position with `--substituter`.
    #[arg(long, global = true)]
    trusted_public_key: Vec<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build all packages, or one named package.
    Build {
        /// Flake reference supplied to the engine.
        #[arg(long, default_value = ".")]
        flake: String,
        /// Package name.
        package: Option<String>,
        /// Select the progress output interface.
        #[arg(long, value_enum, default_value_t)]
        output: CliOutputMode,
    },
    /// Run all checks or checks selected by `scope` or `scope:job`.
    Check {
        /// Flake reference supplied to the engine.
        #[arg(long, default_value = ".")]
        flake: String,
        /// Optional repository-neutral selector.
        selector: Option<String>,
        /// Select the progress output interface.
        #[arg(long, value_enum, default_value_t)]
        output: CliOutputMode,
    },
    /// Realize an app through the engine, then execute it.
    Run {
        /// Flake reference supplied to the engine.
        #[arg(long, default_value = ".")]
        flake: String,
        /// App name.
        app: String,
        /// Arguments passed to the realized app unchanged.
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Select the progress output interface.
        #[arg(long, value_enum, default_value_t)]
        output: CliOutputMode,
    },
    /// Produce deterministic schedule JSON from a provider-neutral JSON input file.
    Plan {
        /// Path to the graph, scheduling configuration, and optional timing history JSON.
        input: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum CliOutputMode {
    Stream,
    #[default]
    Tui,
}

impl CliOutputMode {
    const fn display(self) -> OutputMode {
        match self {
            Self::Stream => OutputMode::Stream,
            Self::Tui => OutputMode::Tui,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(error.exit_code.get())
        }
    }
}

fn run(cli: Cli) -> Result<(), Error> {
    let Cli {
        nix,
        substituter,
        trusted_public_key,
        command,
    } = cli;
    match command {
        Command::Plan { input } => run_plan(&input),
        command => {
            let output = command.output().expect("engine commands have output modes");
            run_engine(nix, substituter, trusted_public_key, output, command)
        }
    }
}

fn run_engine(
    nix: String,
    substituters: Vec<String>,
    public_keys: Vec<String>,
    output: CliOutputMode,
    command: Command,
) -> Result<(), Error> {
    let cancellation = Cancellation::default();
    forward_termination_signals(&cancellation)?;
    let clock = SystemClock;
    let runner = StdProcessRunner::new(Duration::from_millis(20), Redactor::default());
    let execution = AppExecutionPolicy::inherit_current()?;
    let mut config = EngineConfig::new(nix, NixSystem::host()?);
    config.trusted_substituters = trusted_substituters(substituters, public_keys)?;
    let runtime = Runtime::new(
        RuntimeConfig::new(config, execution),
        RuntimeDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
        },
    );
    let title = command.title();
    let command = match command {
        Command::Plan { .. } => unreachable!(),
        Command::Build { flake, package, .. } => {
            let flake = flake_ref(flake);
            let targets = match package {
                Some(package) => vec![package],
                None => Vec::new(),
            };
            RuntimeCommand::Build {
                title,
                flake,
                targets,
                out_link: None,
                output: output.display(),
            }
        }
        Command::Check {
            flake, selector, ..
        } => {
            let flake = flake_ref(flake);
            if let Some(selector) = selector {
                return runtime
                    .check_selected(SelectedCheckCommand {
                        title,
                        flake,
                        scope: selector,
                        selector: &ReferenceSelector,
                        output: output.display(),
                    })
                    .map(|_| ());
            }
            RuntimeCommand::Check {
                title,
                flake,
                targets: Vec::new(),
                output: output.display(),
            }
        }
        Command::Run {
            flake, app, args, ..
        } => RuntimeCommand::Run {
            title,
            flake: flake_ref(flake),
            app,
            arguments: args.into_iter().map(Into::into).collect(),
            output: output.display(),
        },
    };
    runtime.execute(command).map(|_| ())
}

struct ReferenceSelector;

impl CheckSelector for ReferenceSelector {
    fn select(&self, scope: &str, available: &[String]) -> Result<Vec<String>, Error> {
        select_checks(available.to_vec(), Some(scope))
    }
}

impl Command {
    const fn output(&self) -> Option<CliOutputMode> {
        match self {
            Self::Build { output, .. } | Self::Check { output, .. } | Self::Run { output, .. } => {
                Some(*output)
            }
            Self::Plan { .. } => None,
        }
    }

    fn title(&self) -> String {
        match self {
            Self::Build { package, .. } => package.as_ref().map_or_else(
                || "nt build".to_owned(),
                |package| format!("nt build {package}"),
            ),
            Self::Check { selector, .. } => selector.as_ref().map_or_else(
                || "nt check".to_owned(),
                |selector| format!("nt check {selector}"),
            ),
            Self::Run { app, .. } => format!("nt run {app}"),
            Self::Plan { .. } => "nt plan".to_owned(),
        }
    }
}

fn run_plan(input: &Path) -> Result<(), Error> {
    let input = std::fs::read(input)
        .map_err(|error| Error::io(format!("read {}: {error}", input.display())))?;
    let output = plan_json(&input)?;
    let output = String::from_utf8(output)
        .map_err(|error| Error::internal(format!("plan output was not UTF-8: {error}")))?;
    print!("{output}");
    Ok(())
}

fn flake_ref(reference: String) -> FlakeRef {
    FlakeRef::new(reference, None)
}

fn trusted_substituters(
    substituters: Vec<String>,
    public_keys: Vec<String>,
) -> Result<Vec<TrustedSubstituter>, Error> {
    if substituters.len() != public_keys.len() {
        return Err(Error::usage(
            "supply exactly one --trusted-public-key for each --substituter",
        ));
    }
    let mut trusted = vec![TrustedSubstituter {
        url: "https://cache.nixos.org".to_owned(),
        public_keys: BTreeSet::from([
            "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=".to_owned(),
        ]),
    }];
    trusted.extend(
        substituters
            .into_iter()
            .zip(public_keys)
            .map(|(url, public_key)| TrustedSubstituter {
                url,
                public_keys: BTreeSet::from([public_key]),
            }),
    );
    Ok(trusted)
}

fn select_checks(checks: Vec<String>, selector: Option<&str>) -> Result<Vec<String>, Error> {
    let Some(selector) = selector else {
        return Ok(checks);
    };
    let selected: Vec<String> = match selector.split_once(':') {
        Some((scope, job)) if !scope.is_empty() && !job.is_empty() => {
            let exact = format!("{scope}-{job}");
            checks.into_iter().filter(|check| check == &exact).collect()
        }
        Some(_) => return Err(Error::usage("check selector must be scope or scope:job")),
        None if !selector.is_empty() => {
            let prefix = format!("{selector}-");
            checks
                .into_iter()
                .filter(|check| check.starts_with(&prefix))
                .collect()
        }
        None => return Err(Error::usage("check selector must not be empty")),
    };
    if selected.is_empty() {
        Err(Error::not_found(format!(
            "no checks match selector {selector}"
        )))
    } else {
        Ok(selected)
    }
}

#[cfg(test)]
#[path = "main_test.rs"]
mod main_test;
