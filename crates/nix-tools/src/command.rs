//! Composable command operations backed directly by `nix-tools-engine`.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use nix_tools_core::outcome::{Error, ExitCode, Result};
use nix_tools_core::process::{Cancellation, ProcessRunner, ProcessSpec, StreamPolicy};
use nix_tools_engine::{
    BuildRequest, CheckRequest, DiscoverRequest, EngineRequest, EngineResponse,
    FlakeEngine as EngineFlakeEngine, FlakeRef, Manifest, ManifestOutcome, RunRequest,
};

use crate::{CheckSelector, Flake};

/// Caller-owned policy for the output of a realized app.
pub trait AppOutputPolicy: Send + Sync {
    /// Returns the standard-output stream handling policy.
    fn stdout(&self) -> StreamPolicy;
    /// Returns the standard-error stream handling policy.
    fn stderr(&self) -> StreamPolicy;
}

/// Captures a bounded head of each app stream without relaying it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundedAppOutput {
    limit: usize,
}

/// Explicit working-directory and environment policy for a realized app process.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AppExecutionPolicy {
    cwd: Option<PathBuf>,
    environment: BTreeMap<OsString, OsString>,
}

impl AppExecutionPolicy {
    /// Creates a minimal policy with no working directory and an empty environment.
    #[must_use]
    pub fn minimal() -> Self {
        Self::default()
    }

    /// Captures the current process working directory and environment explicitly.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the current working directory cannot be determined.
    pub fn inherit_current() -> Result<Self> {
        Ok(Self {
            cwd: Some(
                std::env::current_dir().map_err(|error| {
                    Error::io(format!("read current working directory: {error}"))
                })?,
            ),
            environment: std::env::vars_os().collect(),
        })
    }

    /// Sets the app working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Adds or replaces one app environment value.
    #[must_use]
    pub fn with_environment(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.environment
            .insert(key.as_ref().to_os_string(), value.as_ref().to_os_string());
        self
    }

    /// Returns the complete environment supplied to the app process.
    #[must_use]
    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    /// Applies this policy to an app process specification.
    pub fn apply(&self, spec: &mut ProcessSpec) {
        spec.cwd.clone_from(&self.cwd);
        spec.env.clone_from(&self.environment);
    }
}

impl BoundedAppOutput {
    /// Creates a bounded capture policy.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self { limit }
    }
}

impl AppOutputPolicy for BoundedAppOutput {
    fn stdout(&self) -> StreamPolicy {
        StreamPolicy::Capture { limit: self.limit }
    }

    fn stderr(&self) -> StreamPolicy {
        StreamPolicy::Capture { limit: self.limit }
    }
}

/// Composable standard build, check, and run operations backed by one injected Nix engine.
pub struct StandardCommands<'services> {
    engine: &'services dyn EngineFlakeEngine,
    runner: &'services dyn ProcessRunner,
    cancellation: &'services Cancellation,
    output: &'services dyn AppOutputPolicy,
    execution: &'services AppExecutionPolicy,
}

impl<'services> StandardCommands<'services> {
    /// Creates a dispatcher. `engine` and `cancellation` must refer to the same operation context.
    #[must_use]
    pub fn new(
        engine: &'services dyn EngineFlakeEngine,
        runner: &'services dyn ProcessRunner,
        cancellation: &'services Cancellation,
        output: &'services dyn AppOutputPolicy,
        execution: &'services AppExecutionPolicy,
    ) -> Self {
        Self {
            engine,
            runner,
            cancellation,
            output,
            execution,
        }
    }

    /// Builds one package.
    ///
    /// # Errors
    ///
    /// Returns an error from the engine or a failed/cancelled realization manifest.
    pub fn build(&self, flake: &Flake, name: &str) -> Result<()> {
        self.realize(
            EngineRequest::Build(BuildRequest {
                flake: engine_flake(flake),
                targets: vec![valid_name(name)?],
            }),
            "build",
        )
    }

    /// Discovers and builds every package together, allowing engine-wide graph deduplication.
    ///
    /// # Errors
    ///
    /// Returns an error from discovery, realization, or a failed/cancelled manifest.
    pub fn build_all(&self, flake: &Flake) -> Result<Vec<String>> {
        let packages = self.discover(flake)?.packages;
        self.realize(
            EngineRequest::Build(BuildRequest {
                flake: engine_flake(flake),
                targets: packages.clone(),
            }),
            "build",
        )?;
        Ok(packages)
    }

    /// Discovers and runs every check together, allowing engine-wide graph deduplication.
    ///
    /// # Errors
    ///
    /// Returns an error from discovery, realization, or a failed/cancelled manifest.
    pub fn check_all(&self, flake: &Flake) -> Result<Vec<String>> {
        let checks = self.discover(flake)?.checks;
        self.check_names(flake, &checks)?;
        Ok(checks)
    }

    /// Selects checks through caller policy, then realizes the selection together.
    ///
    /// # Errors
    ///
    /// Returns an error from discovery, selection, validation, realization, or a failed manifest.
    pub fn check_selected(
        &self,
        flake: &Flake,
        scope: &str,
        selector: &dyn CheckSelector,
    ) -> Result<Vec<String>> {
        let checks = self.discover(flake)?.checks;
        let mut selected = selector.select(scope, &checks)?;
        selected.sort_unstable();
        selected.dedup();
        if let Some(unknown) = selected.iter().find(|name| !checks.contains(*name)) {
            return Err(Error::usage(format!(
                "selector chose unknown check {unknown}"
            )));
        }
        self.check_names(flake, &selected)?;
        Ok(selected)
    }

    /// Realizes an app through the engine, then executes the prepared program.
    ///
    /// # Errors
    ///
    /// Returns an error from preparation, cancellation, process setup, or the realized app.
    pub fn run(&self, flake: &Flake, name: &str, trailing_args: &[OsString]) -> Result<()> {
        let response = self.execute(EngineRequest::Run(RunRequest {
            flake: engine_flake(flake),
            app: valid_name(name)?,
            arguments: trailing_args.to_vec(),
        }))?;
        let EngineResponse::PreparedRun(prepared) = response else {
            return Err(Error::internal(
                "engine returned a non-app response for run",
            ));
        };
        manifest_result(&prepared.manifest, "run", self.cancellation)?;
        let mut spec = ProcessSpec::new(prepared.program).args(prepared.arguments);
        self.execution.apply(&mut spec);
        spec.stdout = self.output.stdout();
        spec.stderr = self.output.stderr();
        self.runner
            .run(&spec, self.cancellation)?
            .require_success(&spec.program)?;
        Ok(())
    }

    fn discover(&self, flake: &Flake) -> Result<nix_tools_engine::DiscoveredTargets> {
        let response = self.execute(EngineRequest::Discover(DiscoverRequest {
            flake: engine_flake(flake),
        }))?;
        let EngineResponse::Discovery(discovered) = response else {
            return Err(Error::internal(
                "engine returned a non-discovery response for discovery",
            ));
        };
        Ok(discovered)
    }

    fn check_names(&self, flake: &Flake, names: &[String]) -> Result<()> {
        self.realize(
            EngineRequest::Check(CheckRequest {
                flake: engine_flake(flake),
                targets: names
                    .iter()
                    .map(|name| valid_name(name))
                    .collect::<Result<Vec<_>>>()?,
            }),
            "check",
        )
    }

    fn realize(&self, request: EngineRequest, operation: &str) -> Result<()> {
        let response = self.execute(request)?;
        let EngineResponse::Realization(manifest) = response else {
            return Err(Error::internal(format!(
                "engine returned a non-realization response for {operation}"
            )));
        };
        manifest_result(&manifest, operation, self.cancellation)
    }

    fn execute(&self, request: EngineRequest) -> Result<EngineResponse> {
        self.engine.execute(request).map_err(|error| {
            if error.code() == "cancelled" {
                Error::cancelled(
                    self.cancellation.signal().unwrap_or(2),
                    error.message().to_owned(),
                )
            } else {
                Error::external(error.message().to_owned())
            }
        })
    }
}

fn engine_flake(flake: &Flake) -> FlakeRef {
    FlakeRef::new(flake.reference(), None)
}

fn valid_name(name: &str) -> Result<String> {
    if name.is_empty() {
        Err(Error::usage(format!(
            "invalid standard flake output name {name}"
        )))
    } else {
        Ok(name.to_owned())
    }
}

/// Converts a settled engine manifest into the standard command result.
///
/// # Errors
///
/// Returns a child error with the first engine error diagnostic, or a cancellation error preserving
/// the recorded signal. A successful manifest returns `Ok(())`.
pub fn manifest_result(
    manifest: &Manifest,
    operation: &str,
    cancellation: &Cancellation,
) -> Result<()> {
    match manifest.outcome {
        ManifestOutcome::Success => Ok(()),
        ManifestOutcome::Cancelled => Err(Error::cancelled(
            cancellation.signal().unwrap_or(2),
            format!("{operation} cancelled"),
        )),
        ManifestOutcome::Failed => Err(Error::child(
            ExitCode::FAILURE,
            manifest
                .diagnostics
                .iter()
                .find(|diagnostic| {
                    diagnostic.severity == nix_tools_engine::DiagnosticSeverity::Error
                })
                .map_or_else(
                    || format!("{operation} failed"),
                    |diagnostic| diagnostic.message.clone(),
                ),
        )),
    }
}
