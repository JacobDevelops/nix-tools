//! Typed, shell-free operations over the standard flake output namespaces.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use nix_tools_core::outcome::{Error, Result};
use nix_tools_core::process::{
    Cancellation, InputPolicy, ProcessRunner, ProcessSpec, StreamPolicy,
};
use nix_tools_core::system::NixSystem;

/// A flake reference understood by the Nix CLI, such as `.` or a Git URI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Flake {
    reference: String,
    working_directory: Option<PathBuf>,
}

impl Flake {
    /// Creates a flake reference.
    ///
    /// # Errors
    ///
    /// An empty reference is rejected when an operation is attempted.
    #[must_use]
    pub fn new(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            working_directory: None,
        }
    }

    /// Resolves the flake reference from `working_directory` in every Nix operation.
    #[must_use]
    pub fn with_working_directory(mut self, working_directory: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(working_directory.into());
        self
    }

    /// Returns the reference passed to Nix.
    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Returns the directory from which relative flake references are resolved.
    #[must_use]
    pub fn working_directory(&self) -> Option<&Path> {
        self.working_directory.as_deref()
    }
}

/// One standard output namespace and optionally one named member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StandardFlake {
    kind: StandardFlakeKind,
    name: Option<String>,
    system: Option<NixSystem>,
}

/// Standard flake namespaces supported by Nix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StandardFlakeKind {
    /// The `packages.<system>` output.
    Package,
    /// The `checks.<system>` output.
    Check,
    /// The `apps.<system>` output.
    App,
}

impl StandardFlake {
    /// Selects a package by name.
    #[must_use]
    pub fn package(name: impl Into<String>) -> Self {
        Self::named(StandardFlakeKind::Package, name)
    }

    /// Selects a check by name.
    #[must_use]
    pub fn check(name: impl Into<String>) -> Self {
        Self::named(StandardFlakeKind::Check, name)
    }

    /// Selects an app by name.
    #[must_use]
    pub fn app(name: impl Into<String>) -> Self {
        Self::named(StandardFlakeKind::App, name)
    }

    /// Selects all members of a standard namespace.
    #[must_use]
    pub fn all(kind: StandardFlakeKind) -> Self {
        Self {
            kind,
            name: None,
            system: None,
        }
    }

    fn named(kind: StandardFlakeKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: Some(name.into()),
            system: None,
        }
    }

    /// Binds the request to a Nix system.
    #[must_use]
    pub fn for_system(mut self, system: NixSystem) -> Self {
        self.system = Some(system);
        self
    }

    /// Returns the Nix attribute path.
    #[must_use]
    pub fn attribute_path(&self) -> String {
        let system = self.system.map_or("<system>", NixSystem::as_str);
        let namespace = match self.kind {
            StandardFlakeKind::Package => "packages",
            StandardFlakeKind::Check => "checks",
            StandardFlakeKind::App => "apps",
        };
        match &self.name {
            Some(name) => format!("{namespace}.{system}.{}", nix_attribute_segment(name)),
            None => format!("{namespace}.{system}"),
        }
    }

    /// Returns a complete flake target suitable for the Nix CLI.
    #[must_use]
    pub fn target_for(&self, flake: &Flake) -> String {
        format!("{}#{}", flake.reference(), self.attribute_path())
    }
}

fn nix_attribute_segment(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len() + 2);
    escaped.push('"');
    let mut characters = name.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '$' if characters.peek() == Some(&'{') => escaped.push_str("\\$"),
            _ => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

/// Standard flake members discovered for one system.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FlakeContents {
    /// Package names in deterministic lexicographic order.
    pub packages: Vec<String>,
    /// Check names in deterministic lexicographic order.
    pub checks: Vec<String>,
    /// App names in deterministic lexicographic order.
    pub apps: Vec<String>,
}

/// Repository-owned policy deciding which discovered checks to run.
pub trait CheckSelector: Send + Sync {
    /// Selects check names from `checks` for a caller-defined scope.
    ///
    /// # Errors
    ///
    /// Returns an error when the caller's selection policy rejects the scope or candidates.
    fn select(&self, scope: &str, checks: &[String]) -> Result<Vec<String>>;
}

/// Public, injectable operations over standard flake outputs.
pub trait FlakeOperations {
    /// Discovers packages, checks, and apps for the configured system.
    ///
    /// # Errors
    ///
    /// Returns an error from bounded Nix evaluation or invalid returned JSON.
    fn discover(&self, flake: &Flake, cancellation: &Cancellation) -> Result<FlakeContents>;
    /// Discovers package names for the configured system.
    ///
    /// # Errors
    ///
    /// Returns an error from bounded Nix evaluation or invalid returned JSON.
    fn discover_packages(&self, flake: &Flake, cancellation: &Cancellation) -> Result<Vec<String>>;
    /// Discovers check names for the configured system.
    ///
    /// # Errors
    ///
    /// Returns an error from bounded Nix evaluation or invalid returned JSON.
    fn discover_checks(&self, flake: &Flake, cancellation: &Cancellation) -> Result<Vec<String>>;
    /// Discovers app names for the configured system.
    ///
    /// # Errors
    ///
    /// Returns an error from bounded Nix evaluation or invalid returned JSON.
    fn discover_apps(&self, flake: &Flake, cancellation: &Cancellation) -> Result<Vec<String>>;
}

/// Configured flake service with explicit process, Nix path, system, and output bound.
pub struct NixTools<'runner> {
    runner: &'runner dyn ProcessRunner,
    nix_path: OsString,
    system: NixSystem,
    output_limit: usize,
    environment: BTreeMap<OsString, OsString>,
}

impl<'runner> NixTools<'runner> {
    /// Creates a service. `runner`, `nix_path`, and `system` are injected for portability and tests.
    #[must_use]
    pub fn new(
        runner: &'runner dyn ProcessRunner,
        nix_path: OsString,
        system: NixSystem,
        output_limit: usize,
    ) -> Self {
        Self {
            runner,
            nix_path,
            system,
            output_limit,
            environment: BTreeMap::new(),
        }
    }

    /// Supplies the complete environment Nix should receive.
    #[must_use]
    pub fn with_environment(mut self, environment: BTreeMap<OsString, OsString>) -> Self {
        self.environment = environment;
        self
    }

    fn names(
        &self,
        flake: &Flake,
        kind: StandardFlakeKind,
        cancellation: &Cancellation,
    ) -> Result<Vec<String>> {
        ensure_reference(flake)?;
        let target = StandardFlake::all(kind)
            .for_system(self.system)
            .target_for(flake);
        let output = self.run(
            vec![
                OsString::from("eval"),
                OsString::from("--json"),
                target.into(),
                OsString::from("--apply"),
                OsString::from("builtins.attrNames"),
            ],
            flake.working_directory(),
            cancellation,
        )?;
        let mut names: Vec<String> = serde_json::from_slice(&output).map_err(|error| {
            Error::external(format!(
                "nix returned invalid JSON while listing outputs: {error}"
            ))
        })?;
        if names.iter().any(|name| !valid_attribute(name)) {
            return Err(Error::external(
                "nix returned an invalid standard output name",
            ));
        }
        names.sort_unstable();
        names.dedup();
        Ok(names)
    }

    fn run(
        &self,
        args: Vec<OsString>,
        working_directory: Option<&Path>,
        cancellation: &Cancellation,
    ) -> Result<Vec<u8>> {
        let mut spec = ProcessSpec::new(self.nix_path.clone()).args(args);
        spec.cwd = working_directory.map(Path::to_path_buf);
        spec.stdin = InputPolicy::Null;
        spec.stdout = StreamPolicy::Capture {
            limit: self.output_limit,
        };
        spec.stderr = StreamPolicy::Capture {
            limit: self.output_limit,
        };
        spec.env = self.environment.clone();
        let result = self.runner.run(&spec, cancellation)?;
        if !result.termination.success() {
            return Err(Error::child(
                result.termination.exit_code(),
                format!("{} failed", self.nix_path.to_string_lossy()),
            ));
        }
        if result.stdout.truncated || result.stderr.truncated {
            return Err(Error::external(
                "nix output exceeded the configured capture bound",
            ));
        }
        Ok(result.stdout.bytes)
    }
}

impl FlakeOperations for NixTools<'_> {
    fn discover(&self, flake: &Flake, cancellation: &Cancellation) -> Result<FlakeContents> {
        Ok(FlakeContents {
            packages: self.discover_packages(flake, cancellation)?,
            checks: self.discover_checks(flake, cancellation)?,
            apps: self.discover_apps(flake, cancellation)?,
        })
    }

    fn discover_packages(&self, flake: &Flake, cancellation: &Cancellation) -> Result<Vec<String>> {
        self.names(flake, StandardFlakeKind::Package, cancellation)
    }

    fn discover_checks(&self, flake: &Flake, cancellation: &Cancellation) -> Result<Vec<String>> {
        self.names(flake, StandardFlakeKind::Check, cancellation)
    }

    fn discover_apps(&self, flake: &Flake, cancellation: &Cancellation) -> Result<Vec<String>> {
        self.names(flake, StandardFlakeKind::App, cancellation)
    }
}

fn ensure_reference(flake: &Flake) -> Result<()> {
    if flake.reference().is_empty() {
        return Err(Error::usage("flake reference must not be empty"));
    }
    Ok(())
}

fn valid_attribute(name: &str) -> bool {
    !name.is_empty()
}
