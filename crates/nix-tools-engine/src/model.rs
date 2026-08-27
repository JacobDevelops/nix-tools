use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use nix_tools_core::process::{Cancellation, ProcessRunner};
use nix_tools_core::system::NixSystem;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A flake reference and the directory from which Nix should resolve it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlakeRef {
    /// Nix flake reference, such as `.` or `path:/workspace`.
    pub reference: String,
    /// Optional child working directory.
    pub working_directory: Option<PathBuf>,
}

impl FlakeRef {
    /// Creates a flake reference without applying repository policy.
    #[must_use]
    pub fn new(reference: impl Into<String>, working_directory: Option<PathBuf>) -> Self {
        Self {
            reference: reference.into(),
            working_directory,
        }
    }
}

/// A substituter the caller explicitly trusts for this engine invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedSubstituter {
    /// Nix store URL.
    pub url: String,
    /// Public signing keys trusted for paths obtained from this substituter.
    pub public_keys: BTreeSet<String>,
}

/// Hard bounds for evaluation, graph construction, diagnostics, and parallel work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceLimits {
    /// Maximum roots evaluated by one Nix child.
    pub evaluation_batch_size: usize,
    /// Maximum concurrent evaluation children.
    pub evaluation_concurrency: usize,
    /// Maximum substitution jobs and HTTP connections given to Nix.
    pub substitution_concurrency: usize,
    /// Maximum bytes retained from each child stream.
    pub max_process_output_bytes: usize,
    /// Maximum aggregate bytes retained for successful root identities.
    pub max_evaluation_memory_bytes: usize,
    /// Maximum selected roots.
    pub max_roots: usize,
    /// Maximum derivations accepted from `nix derivation show`.
    pub max_graph_nodes: usize,
    /// Maximum bytes retained in one structured diagnostic stream.
    pub max_diagnostic_bytes: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            evaluation_batch_size: 32,
            evaluation_concurrency: 4,
            substitution_concurrency: 4,
            max_process_output_bytes: 8 * 1024 * 1024,
            max_evaluation_memory_bytes: 32 * 1024 * 1024,
            max_roots: 4_096,
            max_graph_nodes: 65_536,
            max_diagnostic_bytes: 8 * 1024,
        }
    }
}

/// Engine-wide Nix and resource configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineConfig {
    /// Nix executable path or name.
    pub nix_executable: OsString,
    /// System whose standard flake outputs are addressed.
    pub system: NixSystem,
    /// Complete ordered set of trusted remote substituters.
    pub trusted_substituters: Vec<TrustedSubstituter>,
    /// Resource bounds and concurrency.
    pub limits: ResourceLimits,
}

impl EngineConfig {
    /// Creates a local-only configuration with bounded defaults.
    #[must_use]
    pub fn new(nix_executable: impl Into<OsString>, system: NixSystem) -> Self {
        Self {
            nix_executable: nix_executable.into(),
            system,
            trusted_substituters: Vec::new(),
            limits: ResourceLimits::default(),
        }
    }
}

/// Injectable wall clock used for manifest timing.
pub trait Clock: Send + Sync {
    /// Returns milliseconds since an implementation-defined epoch.
    fn now_millis(&self) -> u64;
}

/// Wall clock backed by [`SystemTime`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

/// Engine phase exposed to progress reporters and diagnostics.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Standard output discovery.
    Discovery,
    /// Selected root evaluation.
    Evaluation,
    /// Derivation graph construction.
    Graph,
    /// Local and trusted remote availability probes.
    Probe,
    /// Dependency-first realization.
    Realization,
}

/// Settled state of a derivation or requested root.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Every required output was already in the local store.
    Cached,
    /// At least one required output was advertised by a trusted substituter.
    Substituted,
    /// At least one required output was not advertised and was built.
    Built,
    /// Evaluation or realization failed.
    Failed,
    /// A prerequisite failed.
    Skipped,
    /// Cancellation settled the work.
    Cancelled,
}

/// Progress events emitted without imposing a renderer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgressEvent {
    /// A phase began.
    PhaseStarted(Phase),
    /// A phase ended.
    PhaseFinished(Phase),
    /// The validated derivation graph was discovered.
    GraphDiscovered(Vec<DerivationNode>),
    /// One derivation began realization.
    NodeStarted {
        /// Derivation path.
        drv_path: String,
    },
    /// One derivation settled.
    NodeFinished {
        /// Derivation path.
        drv_path: String,
        /// Final state.
        state: NodeState,
    },
    /// The operation observed cancellation.
    Cancelled {
        /// Signal supplied to the cancellation token.
        signal: i32,
    },
}

/// Destination for renderer-neutral engine progress.
pub trait ProgressSink: Send + Sync {
    /// Receives one immutable progress event.
    fn emit(&self, event: ProgressEvent);
}

/// Progress sink that discards every event.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn emit(&self, _event: ProgressEvent) {}
}

/// External dependencies injected into an engine instance.
#[derive(Clone, Copy)]
pub struct EngineDependencies<'a> {
    /// Shell-free child process runner.
    pub runner: &'a dyn ProcessRunner,
    /// Shared cancellation token.
    pub cancellation: &'a Cancellation,
    /// Manifest clock.
    pub clock: &'a dyn Clock,
    /// Renderer-neutral progress destination.
    pub progress: &'a dyn ProgressSink,
}

/// Standard flake output kind.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// `packages.<system>`.
    Package,
    /// `checks.<system>`.
    Check,
    /// `apps.<system>`.
    App,
}

impl TargetKind {
    /// Returns the singular stable kind name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::Check => "check",
            Self::App => "app",
        }
    }

    pub(crate) const fn attribute(self) -> &'static str {
        match self {
            Self::Package => "packages",
            Self::Check => "checks",
            Self::App => "apps",
        }
    }
}

/// Request to list standard flake outputs for the configured system.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoverRequest {
    /// Flake being inspected.
    pub flake: FlakeRef,
}

/// Standard output names, sorted and deduplicated.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiscoveredTargets {
    /// Package names.
    pub packages: Vec<String>,
    /// Check names.
    pub checks: Vec<String>,
    /// App names.
    pub apps: Vec<String>,
}

/// Request to realize selected `packages.<system>` names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildRequest {
    /// Flake containing the packages.
    pub flake: FlakeRef,
    /// Exact names selected by the caller, or empty to select every package.
    pub targets: Vec<String>,
}

/// Request to realize selected `checks.<system>` names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckRequest {
    /// Flake containing the checks.
    pub flake: FlakeRef,
    /// Exact names selected by the caller, or empty to select every check.
    pub targets: Vec<String>,
}

/// Request to realize and prepare one standard flake app.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    /// Flake containing the app.
    pub flake: FlakeRef,
    /// Exact app name selected by the caller.
    pub app: String,
    /// Arguments preserved for the caller's eventual exec.
    pub arguments: Vec<OsString>,
}

/// Realized executable returned to a command layer for policy-controlled exec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedRun {
    /// Evaluated app program.
    pub program: String,
    /// Exact arguments supplied in [`RunRequest`].
    pub arguments: Vec<OsString>,
    /// Realization record for the app's Nix string context.
    pub manifest: Manifest,
}

/// One derivation in a validated dependency graph.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DerivationNode {
    /// Canonical derivation path.
    pub drv_path: String,
    /// Input derivations and the outputs consumed from each.
    pub dependencies: BTreeMap<String, BTreeSet<String>>,
    /// Output names and their known paths.
    pub outputs: BTreeMap<String, Option<String>>,
}

/// Why a dependent node could not be realized.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DependencyFailure {
    /// Failed prerequisite derivation.
    pub dependency: String,
}

/// Availability of one required output.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityState {
    /// Present in the local store.
    Local,
    /// Advertised by a configured trusted substituter.
    TrustedRemote,
    /// Not found locally or in any healthy configured substituter.
    Missing,
    /// A probe failed, so absence is not proven.
    Unknown,
}

/// Deterministic availability record for one output path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Availability {
    /// Output path.
    pub path: String,
    /// Availability state.
    pub state: AvailabilityState,
    /// Advertising substituter, when remote.
    pub substituter: Option<String>,
    /// NAR byte size when reported.
    pub nar_bytes: Option<u64>,
    /// Download byte size when reported.
    pub download_bytes: Option<u64>,
}

/// Diagnostic severity.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// Non-fatal degradation.
    Warning,
    /// Failure affecting a root or node.
    Error,
}

/// Bounded structured engine diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    /// Phase producing the diagnostic.
    pub phase: Phase,
    /// Stable machine-readable code.
    pub code: String,
    /// Severity.
    pub severity: DiagnosticSeverity,
    /// Optional affected root or derivation.
    pub target: Option<String>,
    /// Human-readable message.
    pub message: String,
    /// Bounded captured standard output.
    pub stdout: String,
    /// Bounded captured standard error.
    pub stderr: String,
    /// Whether either diagnostic stream was truncated.
    pub truncated: bool,
}

/// Process count and summed child wall duration for one phase.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PhaseMetrics {
    /// Number of child invocations.
    pub processes: usize,
    /// Summed child wall duration.
    pub duration_ms: u64,
}

/// Realization metrics for one derivation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NodeMetrics {
    /// Derivation path.
    pub drv_path: String,
    /// Child wall duration, or zero when batched realization cannot attribute time to one node.
    pub duration_ms: u64,
}

/// Aggregate deterministic engine metrics.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ManifestMetrics {
    /// Caller clock at operation start.
    pub started_at_ms: u64,
    /// Caller clock at operation end.
    pub finished_at_ms: u64,
    /// Root evaluation work.
    pub evaluation: PhaseMetrics,
    /// Graph evaluation work.
    pub graph: PhaseMetrics,
    /// Availability probe work.
    pub probe: PhaseMetrics,
    /// Realization work.
    pub realization: PhaseMetrics,
    /// Per-node realization duration sorted by derivation path; batched work is timed by
    /// [`Self::realization`] and leaves individual node durations at zero.
    pub nodes: Vec<NodeMetrics>,
}

/// Result for one exact requested target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RootResult {
    /// Standard output kind.
    pub kind: TargetKind,
    /// Exact target name.
    pub name: String,
    /// Evaluated derivation path.
    pub drv_path: Option<String>,
    /// Selected output names and paths.
    pub outputs: BTreeMap<String, String>,
    /// Final state.
    pub state: NodeState,
}

/// Final result for one deduplicated derivation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NodeResult {
    /// Derivation path.
    pub drv_path: String,
    /// Dependency derivations.
    pub dependencies: Vec<String>,
    /// Required outputs.
    pub required_outputs: BTreeSet<String>,
    /// Produced or already available paths.
    pub produced_paths: Vec<String>,
    /// Final state.
    pub state: NodeState,
    /// Failed prerequisite when skipped.
    pub dependency_failure: Option<DependencyFailure>,
}

/// Aggregate manifest status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestOutcome {
    /// Every requested root succeeded.
    Success,
    /// At least one requested root failed or was skipped.
    Failed,
    /// Cancellation interrupted the operation.
    Cancelled,
}

/// Deterministic realization record returned to callers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Manifest {
    /// Manifest schema identifier.
    pub schema: &'static str,
    /// Configured Nix system.
    pub system: String,
    /// Requested roots sorted by kind and name.
    pub roots: Vec<RootResult>,
    /// Validated graph sorted by derivation path.
    pub graph: Vec<DerivationNode>,
    /// Output availability sorted by path.
    pub availability: Vec<Availability>,
    /// Node results sorted by derivation path.
    pub nodes: Vec<NodeResult>,
    /// Diagnostics sorted by phase, target, code, and message.
    pub diagnostics: Vec<Diagnostic>,
    /// Stable metrics and caller-clock timestamps.
    pub metrics: ManifestMetrics,
    /// Aggregate outcome.
    pub outcome: ManifestOutcome,
}

/// Public dispatch request for adapter-friendly command layers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineRequest {
    /// Discover standard outputs.
    Discover(DiscoverRequest),
    /// Build selected packages.
    Build(BuildRequest),
    /// Realize selected checks.
    Check(CheckRequest),
    /// Prepare a realized app invocation.
    Run(RunRequest),
}

/// Public dispatch response matching [`EngineRequest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineResponse {
    /// Standard output discovery.
    Discovery(DiscoveredTargets),
    /// Package or check realization.
    Realization(Manifest),
    /// Realized app invocation.
    PreparedRun(PreparedRun),
}

/// Adapter-friendly engine dispatch boundary.
pub trait FlakeEngine {
    /// Executes one typed request.
    ///
    /// # Errors
    ///
    /// Returns a configuration, cancellation, process, or protocol error before a structured
    /// partial manifest can be produced.
    fn execute(&self, request: EngineRequest) -> Result<EngineResponse, EngineError>;
}

/// Configuration, cancellation, process, or Nix protocol failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct EngineError {
    code: &'static str,
    message: String,
}

impl EngineError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Stable machine-readable code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// Human-readable detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}
