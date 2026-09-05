//! Complete interactive execution of standard Nix flake operations.

use std::ffi::OsString;
use std::path::PathBuf;

use nix_tools_core::outcome::{Error, Result};
use nix_tools_core::process::{Cancellation, ProcessRunner, ProcessSpec, StreamPolicy};
use nix_tools_engine::{
    BuildRequest, CheckRequest, Clock, DiscoverRequest, DiscoveredTargets, EngineConfig,
    EngineDependencies, FlakeRef, Manifest, NixEngine, NoProgress, RunRequest,
};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::ui::UiSession;
use crate::{AppExecutionPolicy, CheckSelector, manifest_result};

/// Runtime-wide engine, app execution, and output limits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    /// Complete engine configuration, including resource limits and trusted substituters.
    pub engine: EngineConfig,
    /// Working directory and environment inherited by realized applications.
    pub execution: AppExecutionPolicy,
    /// Maximum bytes captured from each realized application output stream.
    pub app_output_limit: usize,
}

impl RuntimeConfig {
    /// Creates runtime configuration with explicit engine and application execution policy.
    #[must_use]
    pub const fn new(engine: EngineConfig, execution: AppExecutionPolicy) -> Self {
        Self {
            engine,
            execution,
            app_output_limit: 8 * 1024 * 1024,
        }
    }
}

/// Process, cancellation, and clock adapters shared by one runtime.
#[derive(Clone, Copy)]
pub struct RuntimeDependencies<'services> {
    /// Executes Nix and realized applications.
    pub runner: &'services dyn ProcessRunner,
    /// Carries caller- or signal-requested cancellation through all phases.
    pub cancellation: &'services Cancellation,
    /// Supplies manifest timestamps.
    pub clock: &'services dyn Clock,
}

/// One standard flake operation with caller-owned naming and target selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeCommand {
    /// Builds all outputs when `targets` is empty, or the selected package outputs.
    Build {
        /// Title rendered by the interactive interface.
        title: String,
        /// Flake to realize.
        flake: FlakeRef,
        /// Package output names; empty delegates whole-command discovery to the engine.
        targets: Vec<String>,
        /// Optional result symlink path. Absent means no out link is created.
        out_link: Option<PathBuf>,
        /// Requested display interface. TUI falls back to streaming when unavailable.
        output: crate::OutputMode,
    },
    /// Checks all outputs when `targets` is empty, or the caller-selected checks.
    Check {
        /// Title rendered by the interactive interface.
        title: String,
        /// Flake to realize.
        flake: FlakeRef,
        /// Check output names selected by caller policy.
        targets: Vec<String>,
        /// Requested display interface. TUI falls back to streaming when unavailable.
        output: crate::OutputMode,
    },
    /// Realizes and executes one app.
    Run {
        /// Title rendered by the interactive interface.
        title: String,
        /// Flake containing the app.
        flake: FlakeRef,
        /// App output name.
        app: String,
        /// Arguments passed to the app without UTF-8 conversion.
        arguments: Vec<OsString>,
        /// Requested display interface. TUI falls back to streaming when unavailable.
        output: crate::OutputMode,
    },
}

/// Interactive selected-check operation with repository-owned selection policy.
pub struct SelectedCheckCommand<'selector> {
    /// Title rendered by the interactive interface.
    pub title: String,
    /// Flake containing the checks.
    pub flake: FlakeRef,
    /// Repository-defined scope passed to the selector.
    pub scope: String,
    /// Policy that maps the scope and discovered names to exact check names.
    pub selector: &'selector dyn CheckSelector,
    /// Requested display interface. TUI falls back to streaming when unavailable.
    pub output: crate::OutputMode,
}

/// Interactive standard-command runtime backed by one engine context.
pub struct Runtime<'services> {
    config: RuntimeConfig,
    dependencies: RuntimeDependencies<'services>,
}

impl<'services> Runtime<'services> {
    /// Creates a runtime. The cancellation adapter must also be passed to signal forwarding when
    /// the caller wants SIGINT and SIGTERM handled automatically.
    #[must_use]
    pub const fn new(config: RuntimeConfig, dependencies: RuntimeDependencies<'services>) -> Self {
        Self {
            config,
            dependencies,
        }
    }

    /// Discovers standard flake outputs for caller-owned selection policy.
    ///
    /// # Errors
    ///
    /// Returns configuration, cancellation, evaluation, or discovery protocol errors.
    pub fn discover(&self, flake: FlakeRef) -> Result<DiscoveredTargets> {
        let progress = NoProgress;
        let engine = NixEngine::new(
            self.config.engine.clone(),
            EngineDependencies {
                runner: self.dependencies.runner,
                cancellation: self.dependencies.cancellation,
                clock: self.dependencies.clock,
                progress: &progress,
            },
        )
        .map_err(|error| engine_error(&error, self.dependencies.cancellation))?;
        engine
            .discover(&DiscoverRequest { flake })
            .map_err(|error| engine_error(&error, self.dependencies.cancellation))
    }

    /// Runs the complete engine, progress-interface, and application-execution lifecycle.
    ///
    /// # Errors
    ///
    /// Returns configuration, engine, cancellation, realization, or application process errors.
    pub fn execute(&self, command: RuntimeCommand) -> Result<Manifest> {
        if matches!(command, RuntimeCommand::Run { .. }) {
            return self.execute_run(command);
        }
        let operation = command.operation();
        let manifest = self.execute_settled(command)?;
        manifest_result(&manifest, operation, self.dependencies.cancellation)?;
        Ok(manifest)
    }

    /// Realizes a build or check and returns its settled manifest without interpreting its outcome.
    ///
    /// Failed and cancelled realizations are returned as manifests so caller policy can inspect
    /// diagnostics, nodes, and metrics. App execution is deliberately unavailable through this
    /// method because running an app is not a settled realization.
    ///
    /// # Errors
    ///
    /// Returns configuration, fatal engine, or usage errors. A `Run` command is rejected before
    /// engine or process execution.
    pub fn execute_settled(&self, command: RuntimeCommand) -> Result<Manifest> {
        if matches!(command, RuntimeCommand::Run { .. }) {
            return Err(Error::usage(
                "settled execution supports only build and check commands",
            ));
        }
        let (title, output) = command.presentation();
        let mut ui = UiSession::detect(title, self.dependencies.cancellation.clone(), output);
        let engine = NixEngine::new(
            self.config.engine.clone(),
            EngineDependencies {
                runner: self.dependencies.runner,
                cancellation: self.dependencies.cancellation,
                clock: self.dependencies.clock,
                progress: ui.progress(),
            },
        )
        .map_err(|error| engine_error(&error, self.dependencies.cancellation))?;
        let result = match command {
            RuntimeCommand::Build {
                flake,
                targets,
                out_link,
                ..
            } => engine
                .build(BuildRequest {
                    flake,
                    targets,
                    out_link,
                })
                .map(CompletedCommand::Realization),
            RuntimeCommand::Check { flake, targets, .. } => engine
                .check(CheckRequest { flake, targets })
                .map(CompletedCommand::Realization),
            RuntimeCommand::Run { .. } => unreachable!("run rejected before engine setup"),
        }
        .map_err(|error| engine_error(&error, self.dependencies.cancellation));
        drop(engine);
        let completed = match result {
            Ok(completed) => completed,
            Err(error) => {
                ui.finish(None);
                return Err(error);
            }
        };
        match completed {
            CompletedCommand::Realization(manifest) => {
                ui.finish(Some(&manifest));
                Ok(manifest)
            }
        }
    }

    fn execute_run(&self, command: RuntimeCommand) -> Result<Manifest> {
        let RuntimeCommand::Run {
            title,
            flake,
            app,
            arguments,
            output,
        } = command
        else {
            unreachable!("execute_run receives only run commands")
        };
        let mut ui = UiSession::detect(title, self.dependencies.cancellation.clone(), output);
        let engine = NixEngine::new(
            self.config.engine.clone(),
            EngineDependencies {
                runner: self.dependencies.runner,
                cancellation: self.dependencies.cancellation,
                clock: self.dependencies.clock,
                progress: ui.progress(),
            },
        )
        .map_err(|error| engine_error(&error, self.dependencies.cancellation))?;
        let result = engine
            .prepare_run(RunRequest {
                flake,
                app,
                arguments,
            })
            .map_err(|error| engine_error(&error, self.dependencies.cancellation));
        drop(engine);
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                ui.finish(None);
                return Err(error);
            }
        };
        ui.finish(Some(&prepared.manifest));
        manifest_result(&prepared.manifest, "run", self.dependencies.cancellation)?;
        let mut process = ProcessSpec::new(prepared.program).args(prepared.arguments);
        self.config.execution.apply(&mut process);
        process.stdout = StreamPolicy::RelayAndCapture {
            limit: self.config.app_output_limit,
        };
        process.stderr = StreamPolicy::RelayAndCapture {
            limit: self.config.app_output_limit,
        };
        self.dependencies
            .runner
            .run(&process, self.dependencies.cancellation)?
            .require_success(&process.program)?;
        Ok(prepared.manifest)
    }

    /// Discovers, selects, and realizes checks within one interactive session.
    ///
    /// # Errors
    ///
    /// Returns configuration, discovery, selection, cancellation, or realization errors.
    /// An empty selection is rejected so it cannot expand to all checks.
    pub fn check_selected(&self, command: SelectedCheckCommand<'_>) -> Result<Manifest> {
        let mut ui = UiSession::detect(
            command.title,
            self.dependencies.cancellation.clone(),
            command.output,
        );
        let engine = NixEngine::new(
            self.config.engine.clone(),
            EngineDependencies {
                runner: self.dependencies.runner,
                cancellation: self.dependencies.cancellation,
                clock: self.dependencies.clock,
                progress: ui.progress(),
            },
        )
        .map_err(|error| engine_error(&error, self.dependencies.cancellation))?;
        let result = (|| {
            let checks = engine
                .discover(&DiscoverRequest {
                    flake: command.flake.clone(),
                })
                .map_err(|error| engine_error(&error, self.dependencies.cancellation))?
                .checks;
            let mut selected = command.selector.select(&command.scope, &checks)?;
            if selected.is_empty() {
                return Err(Error::usage("selector chose no checks"));
            }
            selected.sort_unstable();
            selected.dedup();
            if let Some(unknown) = selected.iter().find(|name| !checks.contains(*name)) {
                return Err(Error::usage(format!(
                    "selector chose unknown check {unknown}"
                )));
            }
            engine
                .check(CheckRequest {
                    flake: command.flake,
                    targets: selected,
                })
                .map_err(|error| engine_error(&error, self.dependencies.cancellation))
        })();
        drop(engine);
        match result {
            Ok(manifest) => {
                ui.finish(Some(&manifest));
                manifest_result(&manifest, "check", self.dependencies.cancellation)?;
                Ok(manifest)
            }
            Err(error) => {
                ui.finish(None);
                Err(error)
            }
        }
    }
}

impl RuntimeCommand {
    fn presentation(&self) -> (String, crate::OutputMode) {
        match self {
            Self::Build { title, output, .. }
            | Self::Check { title, output, .. }
            | Self::Run { title, output, .. } => (title.clone(), *output),
        }
    }

    const fn operation(&self) -> &'static str {
        match self {
            Self::Build { .. } => "build",
            Self::Check { .. } => "check",
            Self::Run { .. } => "run",
        }
    }
}

enum CompletedCommand {
    Realization(Manifest),
}

/// Forwards the first SIGINT or SIGTERM to a runtime cancellation token.
///
/// # Errors
///
/// Returns an I/O error when signal handlers cannot be installed.
pub fn forward_termination_signals(cancellation: &Cancellation) -> Result<()> {
    let mut signals = Signals::new([SIGINT, SIGTERM])
        .map_err(|error| Error::io(format!("install signal handlers: {error}")))?;
    let cancellation = cancellation.clone();
    std::thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            cancellation.request(signal);
        }
    });
    Ok(())
}

fn engine_error(error: &nix_tools_engine::EngineError, cancellation: &Cancellation) -> Error {
    if error.code() == "cancelled" {
        Error::cancelled(
            cancellation.signal().unwrap_or(SIGINT),
            error.message().to_owned(),
        )
    } else {
        Error::external(error.message().to_owned())
    }
}
