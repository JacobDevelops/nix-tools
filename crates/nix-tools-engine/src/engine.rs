use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, mpsc};
use std::thread;

use nix_tools_core::outcome::ErrorKind;
use nix_tools_core::process::{InputPolicy, ProcessResult, ProcessSpec, StreamPolicy};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    BuildRequest, CheckRequest, DependencyGraph, Diagnostic, DiagnosticSeverity, DiscoverRequest,
    DiscoveredTargets, EngineConfig, EngineDependencies, EngineError, EngineRequest,
    EngineResponse, FlakeEngine, Manifest, ManifestMetrics, ManifestOutcome, NodeState, Phase,
    PhaseMetrics, PreparedRun, ProgressEvent, RootResult, RunRequest, TargetKind,
};

const DISCOVERY_EXPRESSION: &str = r#"
let
  flake = builtins.getFlake (builtins.getEnv "NIX_TOOLS_ENGINE_FLAKE");
  system = builtins.getEnv "NIX_TOOLS_ENGINE_SYSTEM";
  names = kind: builtins.attrNames (flake.${kind}.${system} or {});
in {
  packages = names "packages";
  checks = names "checks";
  apps = names "apps";
}
"#;

const EVALUATION_EXPRESSION: &str = r#"
let
  flake = builtins.getFlake (builtins.getEnv "NIX_TOOLS_ENGINE_FLAKE");
  system = builtins.getEnv "NIX_TOOLS_ENGINE_SYSTEM";
  targets = builtins.fromJSON (builtins.getEnv "NIX_TOOLS_ENGINE_TARGETS");
  evaluate = target:
    let
      attrs = builtins.getAttr target.kind flake;
      values = builtins.getAttr system attrs;
      package = builtins.getAttr target.name values;
      identity = {
        drvPath = builtins.unsafeDiscardStringContext package.drvPath;
        outputs = builtins.listToAttrs (map (name: {
          inherit name;
          value = builtins.unsafeDiscardStringContext package.${name}.outPath;
        }) package.outputs);
        outputsToInstall = package.meta.outputsToInstall or package.outputs;
      };
    in builtins.tryEval (builtins.deepSeq identity identity);
in map evaluate targets
"#;

const ALL_EVALUATION_EXPRESSION: &str = r#"
let
  flake = builtins.getFlake (builtins.getEnv "NIX_TOOLS_ENGINE_FLAKE");
  system = builtins.getEnv "NIX_TOOLS_ENGINE_SYSTEM";
  kind = builtins.getEnv "NIX_TOOLS_ENGINE_KIND";
  maxRoots = builtins.fromJSON (builtins.getEnv "NIX_TOOLS_ENGINE_MAX_ROOTS");
  attrs = if builtins.hasAttr kind flake then builtins.getAttr kind flake else {};
  values = if builtins.hasAttr system attrs then builtins.getAttr system attrs else {};
  names = builtins.attrNames values;
  evaluate = name:
    let
      package = builtins.getAttr name values;
      identity = {
        drvPath = builtins.unsafeDiscardStringContext package.drvPath;
        outputs = builtins.listToAttrs (map (output: {
          name = output;
          value = builtins.unsafeDiscardStringContext package.${output}.outPath;
        }) package.outputs);
        outputsToInstall = package.meta.outputsToInstall or package.outputs;
      };
    in builtins.tryEval (builtins.deepSeq identity identity);
in if builtins.length names > maxRoots
   then { exceeded = true; count = builtins.length names; }
   else { exceeded = false; inherit names; attempts = map evaluate names; }
"#;

const APP_EXPRESSION: &str = r#"
let
  flake = builtins.getFlake (builtins.getEnv "NIX_TOOLS_ENGINE_FLAKE");
  system = builtins.getEnv "NIX_TOOLS_ENGINE_SYSTEM";
  appName = builtins.getEnv "NIX_TOOLS_ENGINE_APP";
  app = builtins.getAttr appName flake.apps.${system};
in {
  program = builtins.unsafeDiscardStringContext app.program;
  context = builtins.getContext app.program;
}
"#;

#[derive(Clone, Debug)]
struct EvaluatedRoot {
    kind: TargetKind,
    name: String,
    drv_path: String,
    outputs: BTreeMap<String, String>,
    selected_outputs: BTreeSet<String>,
}

#[derive(Deserialize)]
struct EvaluationAttempt {
    success: bool,
    value: Value,
}

#[derive(Deserialize)]
struct AllEvaluationAttempts {
    exceeded: bool,
    #[serde(default)]
    count: usize,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    attempts: Vec<EvaluationAttempt>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EvaluationIdentity {
    drv_path: String,
    outputs: BTreeMap<String, String>,
    outputs_to_install: Vec<String>,
}

struct EvaluationBatch {
    ordinal: usize,
    names: Vec<String>,
}

struct EvaluationBatchResult {
    ordinal: usize,
    names: Vec<String>,
    results: Vec<Result<EvaluationIdentity, Box<Diagnostic>>>,
    metrics: PhaseMetrics,
}

struct EvaluationState {
    roots: Vec<RootResult>,
    evaluated: Vec<EvaluatedRoot>,
    diagnostics: Vec<Diagnostic>,
    metrics: PhaseMetrics,
}

#[derive(Deserialize)]
struct AppIdentity {
    program: String,
    context: BTreeMap<String, AppContext>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppContext {
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    all_outputs: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct PathSizes {
    nar_bytes: Option<u64>,
    download_bytes: Option<u64>,
}

struct ProbeState {
    availability: BTreeMap<String, crate::Availability>,
    diagnostics: Vec<Diagnostic>,
    metrics: PhaseMetrics,
}

struct NodeExecution {
    state: Option<NodeState>,
    active_dependencies: BTreeSet<String>,
    required_outputs: BTreeSet<String>,
    produced_paths: Vec<String>,
    duration_ms: u64,
    dependency_failure: Option<crate::DependencyFailure>,
    expected_state: NodeState,
}

struct NodeRun {
    state: NodeState,
    produced_paths: Vec<String>,
    duration_ms: u64,
    process_duration_ms: u64,
    process_ran: bool,
    dependency_failure: Option<crate::DependencyFailure>,
    diagnostic: Option<Diagnostic>,
}

struct RealizationState {
    executions: BTreeMap<String, NodeExecution>,
    metrics: PhaseMetrics,
    node_metrics: Vec<crate::NodeMetrics>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Copy)]
struct GraphCompletion<'a> {
    metrics: PhaseMetrics,
    failure_fallback: bool,
    probe_phase_open: bool,
    out_link: Option<&'a std::path::Path>,
}

#[derive(Clone, Copy)]
struct RealizationPolicy<'a> {
    nonlocal_state: Option<NodeState>,
    out_link: Option<&'a std::path::Path>,
}

impl<'a> GraphCompletion<'a> {
    const fn realization_policy(self) -> RealizationPolicy<'a> {
        RealizationPolicy {
            nonlocal_state: if self.failure_fallback {
                Some(NodeState::Realized)
            } else {
                None
            },
            out_link: self.out_link,
        }
    }
}

/// Policy-free implementation of standard flake discovery and realization.
pub struct NixEngine<'a> {
    config: EngineConfig,
    dependencies: EngineDependencies<'a>,
}

impl<'a> NixEngine<'a> {
    /// Creates an engine after validating all resource and trust configuration.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when a required bound is zero or a substituter is malformed.
    pub fn new(
        config: EngineConfig,
        dependencies: EngineDependencies<'a>,
    ) -> Result<Self, EngineError> {
        validate_config(&config)?;
        Ok(Self {
            config,
            dependencies,
        })
    }

    /// Discovers standard packages, checks, and apps for the configured system.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, child-process, output-limit, or protocol error.
    pub fn discover(&self, request: &DiscoverRequest) -> Result<DiscoveredTargets, EngineError> {
        self.check_cancellation()?;
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseStarted(Phase::Discovery));
        let spec = self.eval_spec(&request.flake, DISCOVERY_EXPRESSION)?;
        let result = self.run(&spec, "discovery_process_failed")?;
        let discovered = self.parse_discovery(&result, &request.flake.reference)?;
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseFinished(Phase::Discovery));
        Ok(discovered)
    }

    /// Realizes exact names under `packages.<system>`.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration, cancellation, or a fatal Nix protocol failure prevents
    /// a structured manifest from being produced.
    pub fn build(&self, request: BuildRequest) -> Result<crate::Manifest, EngineError> {
        if request.out_link.is_some() && request.targets.len() != 1 {
            return Err(EngineError::new(
                "invalid_out_link_targets",
                "a build out link requires exactly one target",
            ));
        }
        self.realize_named(
            crate::TargetKind::Package,
            &request.flake,
            request.targets,
            request.out_link.as_deref(),
        )
    }

    /// Realizes exact names under `checks.<system>`.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration, cancellation, or a fatal Nix protocol failure prevents
    /// a structured manifest from being produced.
    pub fn check(&self, request: CheckRequest) -> Result<crate::Manifest, EngineError> {
        self.realize_named(
            crate::TargetKind::Check,
            &request.flake,
            request.targets,
            None,
        )
    }

    /// Evaluates an app program and its Nix string context, realizes every owning derivation, then
    /// returns an exact invocation for the command layer to exec.
    ///
    /// # Errors
    ///
    /// Returns an error when app evaluation or context realization fails.
    pub fn prepare_run(&self, request: RunRequest) -> Result<PreparedRun, EngineError> {
        self.prepare_app(request)
    }

    fn parse_discovery(
        &self,
        result: &ProcessResult,
        flake_reference: &str,
    ) -> Result<DiscoveredTargets, EngineError> {
        self.require_eval_success(result, "nix eval", "discovery_failed", flake_reference)?;
        require_complete_output(result, "discovery")?;
        let mut discovered: DiscoveredTargets = serde_json::from_slice(&result.stdout.bytes)
            .map_err(|error| {
                EngineError::new(
                    "invalid_discovery_json",
                    format!("parse standard flake output names: {error}"),
                )
            })?;
        sort_deduplicate(&mut discovered.packages);
        sort_deduplicate(&mut discovered.checks);
        sort_deduplicate(&mut discovered.apps);
        Ok(discovered)
    }

    fn nix_spec(&self, flake: &crate::FlakeRef) -> ProcessSpec {
        let mut spec = ProcessSpec::new(self.config.nix_executable.clone());
        spec.cwd.clone_from(&flake.working_directory);
        spec.env.insert(
            OsString::from("NIX_CONFIG"),
            OsString::from(self.nix_config()),
        );
        spec.stdin = InputPolicy::Null;
        spec.stdout = StreamPolicy::Capture {
            limit: self.config.limits.max_process_output_bytes,
        };
        spec.stderr = StreamPolicy::Capture {
            limit: self.config.limits.max_process_output_bytes,
        };
        spec
    }

    fn eval_spec(
        &self,
        flake: &crate::FlakeRef,
        expression: &str,
    ) -> Result<ProcessSpec, EngineError> {
        let reference = resolve_flake_reference(flake)?;
        let mut spec = self
            .nix_spec(flake)
            .args(["eval", "--impure", "--json", "--expr", expression]);
        spec.env.insert(
            OsString::from("NIX_TOOLS_ENGINE_FLAKE"),
            OsString::from(reference),
        );
        spec.env.insert(
            OsString::from("NIX_TOOLS_ENGINE_SYSTEM"),
            OsString::from(self.config.system.as_str()),
        );
        Ok(spec)
    }

    fn require_eval_success(
        &self,
        result: &ProcessResult,
        command: &str,
        code: &'static str,
        flake_reference: &str,
    ) -> Result<(), EngineError> {
        if result.termination.success() {
            return Ok(());
        }
        let stderr = bounded_redacted_stderr(
            &result.stderr.bytes,
            self.config.limits.max_diagnostic_bytes,
            std::iter::once(flake_reference).chain(
                self.config
                    .trusted_substituters
                    .iter()
                    .map(|cache| cache.url.as_str()),
            ),
        );
        let suffix = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        };
        Err(EngineError::new(
            code,
            format!("{command} failed with {:?}{suffix}", result.termination),
        ))
    }

    fn nix_config(&self) -> String {
        let substituters = self
            .config
            .trusted_substituters
            .iter()
            .map(|cache| cache.url.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let keys = self
            .config
            .trusted_substituters
            .iter()
            .flat_map(|cache| cache.public_keys.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "accept-flake-config = false\nbuilders =\nbuilders-use-substitutes = false\nexperimental-features = nix-command flakes\nfallback = false\nplugin-files =\nrequire-sigs = true\nsubstituters = {substituters}\ntrusted-substituters = {substituters}\ntrusted-public-keys = {keys}\n"
        )
    }

    fn run(&self, spec: &ProcessSpec, code: &'static str) -> Result<ProcessResult, EngineError> {
        self.dependencies
            .runner
            .run(spec, self.dependencies.cancellation)
            .map_err(|error| {
                if error.kind == ErrorKind::Cancelled {
                    let signal = self.dependencies.cancellation.signal().unwrap_or(0);
                    self.dependencies
                        .progress
                        .emit(ProgressEvent::Cancelled { signal });
                    EngineError::new("cancelled", error.message)
                } else {
                    EngineError::new(code, error.message)
                }
            })
    }

    fn check_cancellation(&self) -> Result<(), EngineError> {
        if let Some(signal) = self.dependencies.cancellation.signal() {
            self.dependencies
                .progress
                .emit(ProgressEvent::Cancelled { signal });
            return Err(EngineError::new(
                "cancelled",
                format!("Nix engine cancelled by signal {signal}"),
            ));
        }
        Ok(())
    }

    fn realize_named(
        &self,
        kind: TargetKind,
        flake: &crate::FlakeRef,
        mut targets: Vec<String>,
        out_link: Option<&std::path::Path>,
    ) -> Result<crate::Manifest, EngineError> {
        self.check_cancellation()?;
        sort_deduplicate(&mut targets);
        if targets.len() > self.config.limits.max_roots {
            return Err(EngineError::new(
                "root_limit_exceeded",
                format!(
                    "{} selected roots exceed the configured limit of {}",
                    targets.len(),
                    self.config.limits.max_roots
                ),
            ));
        }
        if targets.iter().any(String::is_empty) {
            return Err(EngineError::new(
                "invalid_target_name",
                "target names must not be empty",
            ));
        }
        let started_at_ms = self.dependencies.clock.now_millis();
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseStarted(Phase::Evaluation));
        let evaluation = if targets.is_empty() {
            self.evaluate_all_named_roots(kind, flake)
        } else {
            self.evaluate_named_roots(kind, flake, &targets)
        };
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseFinished(Phase::Evaluation));
        Ok(self.complete_realization(flake, evaluation, started_at_ms, out_link))
    }

    fn prepare_app(&self, request: RunRequest) -> Result<PreparedRun, EngineError> {
        self.check_cancellation()?;
        if request.app.is_empty() {
            return Err(EngineError::new(
                "invalid_app_name",
                "app name must not be empty",
            ));
        }
        let started_at_ms = self.dependencies.clock.now_millis();
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseStarted(Phase::Evaluation));
        let (identity, metrics) = self.evaluate_app_identity(&request)?;
        let AppIdentity { program, context } = identity;
        let evaluation = self.app_evaluation_state(&request.app, context, metrics)?;
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseFinished(Phase::Evaluation));
        let manifest = self.complete_realization(&request.flake, evaluation, started_at_ms, None);
        Ok(PreparedRun {
            program,
            arguments: request.arguments,
            manifest,
        })
    }

    fn evaluate_app_identity(
        &self,
        request: &RunRequest,
    ) -> Result<(AppIdentity, PhaseMetrics), EngineError> {
        let mut spec = self.eval_spec(&request.flake, APP_EXPRESSION)?;
        spec.env.insert(
            OsString::from("NIX_TOOLS_ENGINE_APP"),
            OsString::from(&request.app),
        );
        let process = self.run(&spec, "app_evaluation_process_failed")?;
        self.require_eval_success(
            &process,
            "nix eval",
            "app_evaluation_failed",
            &request.flake.reference,
        )?;
        require_complete_output(&process, "app evaluation")?;
        let identity: AppIdentity =
            serde_json::from_slice(&process.stdout.bytes).map_err(|error| {
                EngineError::new(
                    "invalid_app_identity",
                    format!("parse app program and string context: {error}"),
                )
            })?;
        if identity.program.is_empty() {
            return Err(EngineError::new(
                "empty_app_program",
                "evaluated app program must not be empty",
            ));
        }
        let mut evaluation_metrics = PhaseMetrics::default();
        record_process(&mut evaluation_metrics, &process);
        Ok((identity, evaluation_metrics))
    }

    fn app_evaluation_state(
        &self,
        app: &str,
        context: BTreeMap<String, AppContext>,
        metrics: PhaseMetrics,
    ) -> Result<EvaluationState, EngineError> {
        let mut evaluated = Vec::new();
        let mut roots = Vec::new();
        for (drv_path, context) in context
            .into_iter()
            .filter(|(path, _)| path.strip_suffix(".drv").is_some())
        {
            let selected_outputs = if context.all_outputs {
                BTreeSet::new()
            } else {
                context.outputs.into_iter().collect()
            };
            roots.push(RootResult {
                kind: TargetKind::App,
                name: app.to_owned(),
                drv_path: Some(drv_path.clone()),
                outputs: BTreeMap::new(),
                state: NodeState::Failed,
            });
            evaluated.push(EvaluatedRoot {
                kind: TargetKind::App,
                name: app.to_owned(),
                drv_path,
                outputs: BTreeMap::new(),
                selected_outputs,
            });
        }
        if evaluated.len() > self.config.limits.max_roots {
            return Err(EngineError::new(
                "root_limit_exceeded",
                format!(
                    "app string context contains {} derivations, exceeding the configured limit of {}",
                    evaluated.len(),
                    self.config.limits.max_roots
                ),
            ));
        }
        if roots.is_empty() {
            roots.push(RootResult {
                kind: TargetKind::App,
                name: app.to_owned(),
                drv_path: None,
                outputs: BTreeMap::new(),
                state: NodeState::Cached,
            });
        }
        Ok(EvaluationState {
            roots,
            evaluated,
            diagnostics: Vec::new(),
            metrics,
        })
    }

    fn evaluate_named_roots(
        &self,
        kind: TargetKind,
        flake: &crate::FlakeRef,
        targets: &[String],
    ) -> EvaluationState {
        let completed = self.run_evaluation_batches(kind, flake, targets);
        self.assemble_evaluation(kind, targets, completed)
    }

    fn evaluate_all_named_roots(
        &self,
        kind: TargetKind,
        flake: &crate::FlakeRef,
    ) -> EvaluationState {
        let mut spec = match self.eval_spec(flake, ALL_EVALUATION_EXPRESSION) {
            Ok(spec) => spec,
            Err(error) => {
                return EvaluationState {
                    roots: Vec::new(),
                    evaluated: Vec::new(),
                    diagnostics: vec![diagnostic(
                        Phase::Evaluation,
                        error.code(),
                        None,
                        error.message(),
                    )],
                    metrics: PhaseMetrics::default(),
                };
            }
        };
        spec.env.insert(
            OsString::from("NIX_TOOLS_ENGINE_KIND"),
            OsString::from(kind.attribute()),
        );
        spec.env.insert(
            OsString::from("NIX_TOOLS_ENGINE_MAX_ROOTS"),
            OsString::from(self.config.limits.max_roots.to_string()),
        );
        let mut metrics = PhaseMetrics::default();
        let process = match self.run(&spec, "evaluation_process_failed") {
            Ok(process) => process,
            Err(error) => {
                return EvaluationState {
                    roots: Vec::new(),
                    evaluated: Vec::new(),
                    diagnostics: vec![diagnostic(
                        Phase::Evaluation,
                        error.code(),
                        None,
                        error.message(),
                    )],
                    metrics,
                };
            }
        };
        record_process(&mut metrics, &process);
        if let Some(failure) = self.all_evaluation_process_failure(&process, metrics) {
            return failure;
        }
        let parsed = serde_json::from_slice::<AllEvaluationAttempts>(&process.stdout.bytes);
        let Ok(all) = parsed else {
            return EvaluationState {
                roots: Vec::new(),
                evaluated: Vec::new(),
                diagnostics: vec![process_diagnostic(
                    self,
                    Phase::Evaluation,
                    "invalid_evaluation_json",
                    None,
                    "parse combined name and identity evaluation",
                    &process,
                )],
                metrics,
            };
        };
        if all.exceeded {
            return EvaluationState {
                roots: Vec::new(),
                evaluated: Vec::new(),
                diagnostics: vec![diagnostic(
                    Phase::Evaluation,
                    "root_limit_exceeded",
                    None,
                    format!(
                        "{} discovered roots exceed the configured limit of {}",
                        all.count, self.config.limits.max_roots
                    ),
                )],
                metrics,
            };
        }
        let names = all.names;
        let results = self.parse_evaluation_attempts(&names, all.attempts, &process);
        self.assemble_evaluation(
            kind,
            &names,
            vec![EvaluationBatchResult {
                ordinal: 0,
                names: names.clone(),
                results,
                metrics,
            }],
        )
    }

    fn all_evaluation_process_failure(
        &self,
        process: &ProcessResult,
        metrics: PhaseMetrics,
    ) -> Option<EvaluationState> {
        (!process.termination.success() || process.stdout.truncated).then(|| EvaluationState {
            roots: Vec::new(),
            evaluated: Vec::new(),
            diagnostics: vec![process_diagnostic(
                self,
                Phase::Evaluation,
                if process.stdout.truncated {
                    "process_output_limit_exceeded"
                } else {
                    "evaluation_failed"
                },
                None,
                if process.stdout.truncated {
                    "evaluation output exceeded the configured process output limit".to_owned()
                } else {
                    format!("nix eval failed with {:?}", process.termination)
                },
                process,
            )],
            metrics,
        })
    }

    fn run_evaluation_batches(
        &self,
        kind: TargetKind,
        flake: &crate::FlakeRef,
        targets: &[String],
    ) -> Vec<EvaluationBatchResult> {
        let batches = targets
            .chunks(self.config.limits.evaluation_batch_size)
            .enumerate()
            .map(|(ordinal, names)| EvaluationBatch {
                ordinal,
                names: names.to_vec(),
            })
            .collect::<Vec<_>>();
        let worker_count = batches.len().min(self.config.limits.evaluation_concurrency);
        let queue = Mutex::new(VecDeque::from(batches));
        let (sender, receiver) = mpsc::sync_channel(worker_count.max(1));
        let mut completed = Vec::new();
        thread::scope(|scope| {
            for _ in 0..worker_count {
                let sender = sender.clone();
                let queue = &queue;
                scope.spawn(move || {
                    loop {
                        if self.dependencies.cancellation.signal().is_some() {
                            return;
                        }
                        let batch = queue
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .pop_front();
                        let Some(batch) = batch else {
                            return;
                        };
                        let result = self.evaluate_batch(kind, flake, batch);
                        if sender.send(result).is_err() {
                            return;
                        }
                    }
                });
            }
            drop(sender);
            completed.extend(receiver);
        });
        completed.sort_by_key(|batch| batch.ordinal);
        completed
    }

    fn assemble_evaluation(
        &self,
        kind: TargetKind,
        targets: &[String],
        completed: Vec<EvaluationBatchResult>,
    ) -> EvaluationState {
        let mut roots = targets
            .iter()
            .map(|name| RootResult {
                kind,
                name: name.clone(),
                drv_path: None,
                outputs: BTreeMap::new(),
                state: NodeState::Failed,
            })
            .collect::<Vec<_>>();
        let indices = roots
            .iter()
            .enumerate()
            .map(|(index, root)| (root.name.clone(), index))
            .collect::<BTreeMap<_, _>>();
        let mut evaluated = Vec::new();
        let mut diagnostics = Vec::new();
        let mut metrics = PhaseMetrics::default();
        let mut retained_bytes = 0_usize;
        for batch in completed {
            merge_phase_metrics(&mut metrics, batch.metrics);
            for (name, result) in batch.names.into_iter().zip(batch.results) {
                let Some(index) = indices.get(&name).copied() else {
                    diagnostics.push(diagnostic(
                        Phase::Evaluation,
                        "evaluation_target_mismatch",
                        Some(name),
                        "evaluation worker returned an unrequested target",
                    ));
                    continue;
                };
                match result {
                    Ok(identity) => {
                        match self.retain_evaluation_identity(
                            kind,
                            name,
                            identity,
                            &mut retained_bytes,
                        ) {
                            Ok(root) => {
                                let Some(manifest_root) = roots.get_mut(index) else {
                                    diagnostics.push(diagnostic(
                                        Phase::Evaluation,
                                        "evaluation_target_mismatch",
                                        Some(root.name),
                                        "evaluation result index exceeded requested roots",
                                    ));
                                    continue;
                                };
                                manifest_root.drv_path = Some(root.drv_path.clone());
                                manifest_root.outputs = root
                                    .outputs
                                    .iter()
                                    .filter(|(output, _)| root.selected_outputs.contains(*output))
                                    .map(|(output, path)| (output.clone(), path.clone()))
                                    .collect();
                                evaluated.push(root);
                            }
                            Err(failure) => diagnostics.push(*failure),
                        }
                    }
                    Err(failure) => diagnostics.push(*failure),
                }
            }
        }
        if let Some(signal) = self.dependencies.cancellation.signal() {
            for root in &mut roots {
                if root.drv_path.is_none() {
                    root.state = NodeState::Cancelled;
                }
            }
            self.dependencies
                .progress
                .emit(ProgressEvent::Cancelled { signal });
        }
        EvaluationState {
            roots,
            evaluated,
            diagnostics,
            metrics,
        }
    }

    fn retain_evaluation_identity(
        &self,
        kind: TargetKind,
        name: String,
        identity: EvaluationIdentity,
        retained_bytes: &mut usize,
    ) -> Result<EvaluatedRoot, Box<Diagnostic>> {
        let selected_outputs = identity
            .outputs_to_install
            .into_iter()
            .collect::<BTreeSet<_>>();
        let retained = identity
            .drv_path
            .len()
            .saturating_add(name.len())
            .saturating_add(identity.outputs.iter().fold(0, |total, (output, path)| {
                total
                    .saturating_add(output.len())
                    .saturating_add(path.len())
            }));
        if retained_bytes.saturating_add(retained) > self.config.limits.max_evaluation_memory_bytes
        {
            return Err(Box::new(diagnostic(
                Phase::Evaluation,
                "evaluation_memory_limit_exceeded",
                Some(name),
                format!(
                    "retained root identities exceeded the configured {} byte limit",
                    self.config.limits.max_evaluation_memory_bytes
                ),
            )));
        }
        if selected_outputs.is_empty() {
            return Err(Box::new(diagnostic(
                Phase::Evaluation,
                "empty_output_selection",
                Some(name),
                "evaluated target selected no outputs",
            )));
        }
        if let Some(output) = selected_outputs
            .iter()
            .find(|output| !identity.outputs.contains_key(*output))
        {
            return Err(Box::new(diagnostic(
                Phase::Evaluation,
                "missing_selected_output",
                Some(name),
                format!("evaluated target omitted selected output {output}"),
            )));
        }
        *retained_bytes = retained_bytes.saturating_add(retained);
        Ok(EvaluatedRoot {
            kind,
            name,
            drv_path: identity.drv_path,
            outputs: identity.outputs,
            selected_outputs,
        })
    }

    fn evaluate_batch(
        &self,
        kind: TargetKind,
        flake: &crate::FlakeRef,
        batch: EvaluationBatch,
    ) -> EvaluationBatchResult {
        let targets = batch
            .names
            .iter()
            .map(|name| json!({"kind": kind.attribute(), "name": name}))
            .collect::<Vec<_>>();
        let targets_json = match serde_json::to_string(&targets) {
            Ok(targets_json) => targets_json,
            Err(error) => {
                let failure = diagnostic(
                    Phase::Evaluation,
                    "target_serialization_failed",
                    None,
                    format!("serialize evaluation target batch: {error}"),
                );
                return EvaluationBatchResult {
                    ordinal: batch.ordinal,
                    names: batch.names.clone(),
                    results: batch
                        .names
                        .iter()
                        .map(|_| Err(Box::new(failure.clone())))
                        .collect(),
                    metrics: PhaseMetrics::default(),
                };
            }
        };
        let mut spec = match self.eval_spec(flake, EVALUATION_EXPRESSION) {
            Ok(spec) => spec,
            Err(error) => {
                let failure = diagnostic(Phase::Evaluation, error.code(), None, error.message());
                return EvaluationBatchResult {
                    ordinal: batch.ordinal,
                    names: batch.names.clone(),
                    results: batch
                        .names
                        .iter()
                        .map(|_| Err(Box::new(failure.clone())))
                        .collect(),
                    metrics: PhaseMetrics::default(),
                };
            }
        };
        spec.env.insert(
            OsString::from("NIX_TOOLS_ENGINE_TARGETS"),
            OsString::from(targets_json),
        );
        let mut metrics = PhaseMetrics::default();
        let results = match self.run(&spec, "evaluation_process_failed") {
            Err(error) => batch
                .names
                .iter()
                .map(|name| {
                    Err(Box::new(diagnostic(
                        Phase::Evaluation,
                        error.code(),
                        Some(name.clone()),
                        error.message(),
                    )))
                })
                .collect(),
            Ok(process) => {
                record_process(&mut metrics, &process);
                self.parse_evaluation_batch(&batch.names, &process)
            }
        };
        EvaluationBatchResult {
            ordinal: batch.ordinal,
            names: batch.names,
            results,
            metrics,
        }
    }

    fn parse_evaluation_batch(
        &self,
        names: &[String],
        result: &ProcessResult,
    ) -> Vec<Result<EvaluationIdentity, Box<Diagnostic>>> {
        let failure = if !result.termination.success() {
            Some((
                "evaluation_failed",
                format!("nix eval failed with {:?}", result.termination),
            ))
        } else if result.stdout.truncated {
            Some((
                "process_output_limit_exceeded",
                "evaluation output exceeded the configured process output limit".to_owned(),
            ))
        } else {
            None
        };
        if let Some((code, message)) = failure {
            return names
                .iter()
                .map(|name| {
                    Err(Box::new(process_diagnostic(
                        self,
                        Phase::Evaluation,
                        code,
                        Some(name.clone()),
                        message.clone(),
                        result,
                    )))
                })
                .collect();
        }
        let attempts: Vec<EvaluationAttempt> =
            match serde_json::from_slice::<Vec<EvaluationAttempt>>(&result.stdout.bytes) {
                Ok(attempts) if attempts.len() == names.len() => attempts,
                Ok(attempts) => {
                    let message = format!(
                        "nix returned {} evaluation results for {} roots",
                        attempts.len(),
                        names.len()
                    );
                    return names
                        .iter()
                        .map(|name| {
                            Err(Box::new(process_diagnostic(
                                self,
                                Phase::Evaluation,
                                "invalid_evaluation_count",
                                Some(name.clone()),
                                message.clone(),
                                result,
                            )))
                        })
                        .collect();
                }
                Err(error) => {
                    let message = format!("parse batched nix evaluation JSON: {error}");
                    return names
                        .iter()
                        .map(|name| {
                            Err(Box::new(process_diagnostic(
                                self,
                                Phase::Evaluation,
                                "invalid_evaluation_json",
                                Some(name.clone()),
                                message.clone(),
                                result,
                            )))
                        })
                        .collect();
                }
            };
        self.parse_evaluation_attempts(names, attempts, result)
    }

    fn parse_evaluation_attempts(
        &self,
        names: &[String],
        attempts: Vec<EvaluationAttempt>,
        result: &ProcessResult,
    ) -> Vec<Result<EvaluationIdentity, Box<Diagnostic>>> {
        if attempts.len() != names.len() {
            let message = format!(
                "nix returned {} evaluation results for {} roots",
                attempts.len(),
                names.len()
            );
            return names
                .iter()
                .map(|name| {
                    Err(Box::new(process_diagnostic(
                        self,
                        Phase::Evaluation,
                        "invalid_evaluation_count",
                        Some(name.clone()),
                        message.clone(),
                        result,
                    )))
                })
                .collect();
        }
        attempts
            .into_iter()
            .zip(names)
            .map(|(attempt, name)| {
                if !attempt.success {
                    return Err(Box::new(diagnostic(
                        Phase::Evaluation,
                        "derivation_evaluation_failed",
                        Some(name.clone()),
                        format!("Nix could not evaluate root {name}"),
                    )));
                }
                serde_json::from_value(attempt.value).map_err(|error| {
                    Box::new(diagnostic(
                        Phase::Evaluation,
                        "invalid_derivation_identity",
                        Some(name.clone()),
                        format!("parse derivation identity for {name}: {error}"),
                    ))
                })
            })
            .collect()
    }

    fn complete_realization(
        &self,
        flake: &crate::FlakeRef,
        evaluation: EvaluationState,
        started_at_ms: u64,
        out_link: Option<&std::path::Path>,
    ) -> Manifest {
        if evaluation.evaluated.is_empty() {
            return self.finish_manifest(
                evaluation.roots,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                evaluation.diagnostics,
                ManifestMetrics {
                    started_at_ms,
                    evaluation: evaluation.metrics,
                    ..ManifestMetrics::default()
                },
            );
        }

        let initial_probe = ProbeState {
            availability: BTreeMap::new(),
            diagnostics: Vec::new(),
            metrics: PhaseMetrics::default(),
        };
        if evaluation
            .evaluated
            .iter()
            .all(|root| root.kind != TargetKind::App && !root.selected_outputs.is_empty())
        {
            return self.complete_root_realization(flake, evaluation, started_at_ms, out_link);
        }

        self.complete_detailed_realization(
            flake,
            evaluation,
            started_at_ms,
            initial_probe,
            false,
            out_link,
        )
    }

    fn complete_root_realization(
        &self,
        flake: &crate::FlakeRef,
        mut evaluation: EvaluationState,
        started_at_ms: u64,
        out_link: Option<&std::path::Path>,
    ) -> Manifest {
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseStarted(Phase::Probe));
        let probe = self.probe_evaluated_roots(flake, &evaluation.evaluated);
        if !probe.availability.is_empty()
            && probe
                .availability
                .values()
                .all(|entry| entry.state == crate::AvailabilityState::Local)
            && out_link.is_none()
        {
            self.dependencies
                .progress
                .emit(ProgressEvent::PhaseFinished(Phase::Probe));
            return self.finish_local_roots(evaluation, started_at_ms, probe);
        }
        if evaluation.evaluated.len() == 1 {
            return self.complete_detailed_realization(
                flake,
                evaluation,
                started_at_ms,
                probe,
                true,
                out_link,
            );
        }
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseFinished(Phase::Probe));
        let roots = evaluation
            .evaluated
            .iter()
            .map(|root| root.drv_path.clone())
            .collect::<BTreeSet<_>>();
        let graph = match DependencyGraph::new(
            lightweight_root_nodes(&evaluation.evaluated),
            &roots,
            self.config.limits.max_roots,
        ) {
            Ok(graph) => graph,
            Err(error) => {
                evaluation.diagnostics.push(diagnostic(
                    Phase::Graph,
                    error.code(),
                    None,
                    error.message(),
                ));
                return self.finish_manifest(
                    evaluation.roots,
                    Vec::new(),
                    probe.availability.into_values().collect(),
                    Vec::new(),
                    evaluation.diagnostics,
                    ManifestMetrics {
                        started_at_ms,
                        evaluation: evaluation.metrics,
                        probe: probe.metrics,
                        ..ManifestMetrics::default()
                    },
                );
            }
        };
        self.complete_valid_graph(
            flake,
            evaluation,
            started_at_ms,
            &graph,
            GraphCompletion {
                metrics: PhaseMetrics::default(),
                failure_fallback: true,
                probe_phase_open: false,
                out_link,
            },
            probe,
        )
    }

    fn complete_detailed_realization(
        &self,
        flake: &crate::FlakeRef,
        mut evaluation: EvaluationState,
        started_at_ms: u64,
        mut initial_probe: ProbeState,
        probe_phase_open: bool,
        out_link: Option<&std::path::Path>,
    ) -> Manifest {
        for availability in initial_probe.availability.values_mut() {
            if availability.state == crate::AvailabilityState::Unknown {
                availability.state = crate::AvailabilityState::Missing;
            }
        }
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseStarted(Phase::Graph));
        let roots = evaluation
            .evaluated
            .iter()
            .map(|root| root.drv_path.clone())
            .collect::<BTreeSet<_>>();
        let graph_result = self.load_graph(flake, &roots);
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseFinished(Phase::Graph));
        let (graph, graph_metrics) = match graph_result {
            Ok(graph) => graph,
            Err(failure) => {
                evaluation.diagnostics.push(*failure);
                self.close_probe_phase(probe_phase_open);
                return self.finish_manifest(
                    evaluation.roots,
                    Vec::new(),
                    initial_probe.availability.into_values().collect(),
                    Vec::new(),
                    evaluation.diagnostics,
                    ManifestMetrics {
                        started_at_ms,
                        evaluation: evaluation.metrics,
                        probe: initial_probe.metrics,
                        ..ManifestMetrics::default()
                    },
                );
            }
        };
        if let Some(failure) = validate_root_identities(&graph, &evaluation.evaluated) {
            evaluation.diagnostics.push(failure);
            self.close_probe_phase(probe_phase_open);
            return self.finish_manifest(
                evaluation.roots,
                graph.nodes().values().cloned().collect(),
                initial_probe.availability.into_values().collect(),
                Vec::new(),
                evaluation.diagnostics,
                ManifestMetrics {
                    started_at_ms,
                    evaluation: evaluation.metrics,
                    graph: graph_metrics,
                    probe: initial_probe.metrics,
                    ..ManifestMetrics::default()
                },
            );
        }
        self.complete_valid_graph(
            flake,
            evaluation,
            started_at_ms,
            &graph,
            GraphCompletion {
                metrics: graph_metrics,
                failure_fallback: false,
                probe_phase_open,
                out_link,
            },
            initial_probe,
        )
    }

    fn probe_evaluated_roots(
        &self,
        flake: &crate::FlakeRef,
        roots: &[EvaluatedRoot],
    ) -> ProbeState {
        let paths = roots
            .iter()
            .flat_map(|root| {
                root.selected_outputs
                    .iter()
                    .filter_map(|output| root.outputs.get(output).cloned())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut availability = paths
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    crate::Availability {
                        path: path.clone(),
                        state: crate::AvailabilityState::Unknown,
                        substituter: None,
                        nar_bytes: None,
                        download_bytes: None,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut metrics = PhaseMetrics::default();
        let mut diagnostics = Vec::new();
        self.probe_local(
            flake,
            &paths,
            &mut availability,
            &mut metrics,
            &mut diagnostics,
        );
        ProbeState {
            availability,
            diagnostics,
            metrics,
        }
    }

    fn finish_local_roots(
        &self,
        mut evaluation: EvaluationState,
        started_at_ms: u64,
        probe: ProbeState,
    ) -> Manifest {
        let mut graph = BTreeMap::<String, crate::DerivationNode>::new();
        let mut required = BTreeMap::<String, BTreeSet<String>>::new();
        for root in &evaluation.evaluated {
            required
                .entry(root.drv_path.clone())
                .or_default()
                .extend(root.selected_outputs.iter().cloned());
            let node =
                graph
                    .entry(root.drv_path.clone())
                    .or_insert_with(|| crate::DerivationNode {
                        drv_path: root.drv_path.clone(),
                        dependencies: BTreeMap::new(),
                        outputs: BTreeMap::new(),
                    });
            node.outputs.extend(
                root.outputs
                    .iter()
                    .map(|(name, path)| (name.clone(), Some(path.clone()))),
            );
        }
        for root in &mut evaluation.roots {
            if root.drv_path.is_some() {
                root.state = NodeState::Cached;
            }
        }
        let nodes = graph
            .iter()
            .map(|(drv_path, node)| {
                let selected = required.get(drv_path).cloned().unwrap_or_default();
                crate::NodeResult {
                    drv_path: drv_path.clone(),
                    dependencies: Vec::new(),
                    required_outputs: selected.clone(),
                    produced_paths: selected
                        .iter()
                        .filter_map(|output| {
                            node.outputs.get(output).and_then(Option::as_ref).cloned()
                        })
                        .collect(),
                    state: NodeState::Cached,
                    dependency_failure: None,
                }
            })
            .collect();
        evaluation.diagnostics.extend(probe.diagnostics);
        self.finish_manifest(
            evaluation.roots,
            graph.into_values().collect(),
            probe.availability.into_values().collect(),
            nodes,
            evaluation.diagnostics,
            ManifestMetrics {
                started_at_ms,
                evaluation: evaluation.metrics,
                probe: probe.metrics,
                ..ManifestMetrics::default()
            },
        )
    }

    fn complete_valid_graph(
        &self,
        flake: &crate::FlakeRef,
        mut evaluation: EvaluationState,
        started_at_ms: u64,
        graph: &DependencyGraph,
        completion: GraphCompletion<'_>,
        initial_probe: ProbeState,
    ) -> Manifest {
        let realization_policy = completion.realization_policy();
        let GraphCompletion {
            metrics: mut graph_metrics,
            failure_fallback,
            probe_phase_open,
            ..
        } = completion;
        populate_app_outputs(graph, &evaluation.evaluated, &mut evaluation.roots);
        let selected = selected_outputs(graph, &evaluation.evaluated);
        let required = match graph.required_outputs(&selected) {
            Ok(required) => required,
            Err(error) => {
                evaluation.diagnostics.push(diagnostic(
                    Phase::Graph,
                    error.code(),
                    None,
                    error.message(),
                ));
                self.close_probe_phase(probe_phase_open);
                return self.finish_manifest(
                    evaluation.roots,
                    graph.nodes().values().cloned().collect(),
                    Vec::new(),
                    Vec::new(),
                    evaluation.diagnostics,
                    ManifestMetrics {
                        started_at_ms,
                        evaluation: evaluation.metrics,
                        graph: graph_metrics,
                        ..ManifestMetrics::default()
                    },
                );
            }
        };
        self.dependencies
            .progress
            .emit(ProgressEvent::GraphDiscovered(
                graph.nodes().values().cloned().collect(),
            ));

        if !failure_fallback && !probe_phase_open {
            self.dependencies
                .progress
                .emit(ProgressEvent::PhaseStarted(Phase::Probe));
        }
        let probe = if failure_fallback {
            initial_probe
        } else {
            self.probe_availability(flake, graph, &selected, initial_probe)
        };
        if !failure_fallback {
            self.dependencies
                .progress
                .emit(ProgressEvent::PhaseFinished(Phase::Probe));
        }
        evaluation.diagnostics.extend(probe.diagnostics);

        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseStarted(Phase::Realization));
        let mut realization = self.realize_graph(
            flake,
            graph,
            &selected,
            &required,
            &probe.availability,
            realization_policy,
        );
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseFinished(Phase::Realization));
        evaluation.diagnostics.append(&mut realization.diagnostics);
        let graph_nodes = if failure_fallback {
            self.failure_graph_nodes(
                flake,
                &mut evaluation,
                &mut realization,
                &mut graph_metrics,
                graph.nodes().values().cloned().collect(),
            )
        } else {
            graph.nodes().values().cloned().collect()
        };
        apply_root_states(&realization.executions, &mut evaluation.roots);
        let nodes = node_results(realization.executions);
        self.finish_manifest(
            evaluation.roots,
            graph_nodes,
            probe.availability.into_values().collect(),
            nodes,
            evaluation.diagnostics,
            ManifestMetrics {
                started_at_ms,
                evaluation: evaluation.metrics,
                graph: graph_metrics,
                probe: probe.metrics,
                realization: realization.metrics,
                nodes: realization.node_metrics,
                ..ManifestMetrics::default()
            },
        )
    }

    fn close_probe_phase(&self, open: bool) {
        if open {
            self.dependencies
                .progress
                .emit(ProgressEvent::PhaseFinished(Phase::Probe));
        }
    }

    fn failure_graph_nodes(
        &self,
        flake: &crate::FlakeRef,
        evaluation: &mut EvaluationState,
        realization: &mut RealizationState,
        graph_metrics: &mut PhaseMetrics,
        default: Vec<crate::DerivationNode>,
    ) -> Vec<crate::DerivationNode> {
        if !realization
            .executions
            .values()
            .any(|execution| execution.state == Some(NodeState::Failed))
        {
            return default;
        }
        let roots = evaluation
            .evaluated
            .iter()
            .map(|root| root.drv_path.clone())
            .collect::<BTreeSet<_>>();
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseStarted(Phase::Graph));
        let nodes = match self.load_graph(flake, &roots) {
            Ok((failure_graph, metrics)) => {
                merge_phase_metrics(graph_metrics, metrics);
                if let Some(failure) =
                    validate_root_identities(&failure_graph, &evaluation.evaluated)
                {
                    evaluation.diagnostics.push(failure);
                    self.dependencies
                        .progress
                        .emit(ProgressEvent::PhaseFinished(Phase::Graph));
                    return default;
                }
                self.dependencies
                    .progress
                    .emit(ProgressEvent::GraphDiscovered(
                        failure_graph.nodes().values().cloned().collect(),
                    ));
                let skipped =
                    apply_failure_dependencies(&failure_graph, &mut realization.executions);
                evaluation.diagnostics.retain(|diagnostic| {
                    diagnostic.code != "realization_failed"
                        || diagnostic
                            .target
                            .as_ref()
                            .is_none_or(|target| !skipped.contains(target))
                });
                failure_graph.nodes().values().cloned().collect()
            }
            Err(failure) => {
                evaluation.diagnostics.push(*failure);
                default
            }
        };
        self.dependencies
            .progress
            .emit(ProgressEvent::PhaseFinished(Phase::Graph));
        nodes
    }

    fn load_graph(
        &self,
        flake: &crate::FlakeRef,
        roots: &BTreeSet<String>,
    ) -> Result<(DependencyGraph, PhaseMetrics), Box<Diagnostic>> {
        let mut input = roots.iter().cloned().collect::<Vec<_>>().join("\n");
        input.push('\n');
        let mut spec = self
            .nix_spec(flake)
            .args(["derivation", "show", "--recursive", "--stdin"]);
        spec.stdin = InputPolicy::Bytes(input.into_bytes());
        let process = self
            .run(&spec, "derivation_graph_process_failed")
            .map_err(|error| {
                Box::new(diagnostic(
                    Phase::Graph,
                    error.code(),
                    None,
                    error.message(),
                ))
            })?;
        let mut metrics = PhaseMetrics::default();
        record_process(&mut metrics, &process);
        if !process.termination.success() {
            return Err(Box::new(process_diagnostic(
                self,
                Phase::Graph,
                "derivation_graph_failed",
                None,
                format!("nix derivation show failed with {:?}", process.termination),
                &process,
            )));
        }
        if process.stdout.truncated {
            return Err(Box::new(process_diagnostic(
                self,
                Phase::Graph,
                "process_output_limit_exceeded",
                None,
                "derivation graph output exceeded the configured process output limit",
                &process,
            )));
        }
        DependencyGraph::from_json(
            &process.stdout.bytes,
            roots,
            self.config.limits.max_graph_nodes,
        )
        .map(|graph| (graph, metrics))
        .map_err(|error| {
            Box::new(process_diagnostic(
                self,
                Phase::Graph,
                error.code(),
                None,
                error.message(),
                &process,
            ))
        })
    }

    fn probe_availability(
        &self,
        flake: &crate::FlakeRef,
        graph: &DependencyGraph,
        required: &BTreeMap<String, BTreeSet<String>>,
        initial: ProbeState,
    ) -> ProbeState {
        let paths = graph
            .nodes()
            .iter()
            .flat_map(|(drv_path, node)| {
                required
                    .get(drv_path)
                    .into_iter()
                    .flatten()
                    .filter_map(|name| node.outputs.get(name).and_then(Option::as_ref).cloned())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut availability = initial.availability;
        for path in &paths {
            availability
                .entry(path.clone())
                .or_insert_with(|| crate::Availability {
                    path: path.clone(),
                    state: crate::AvailabilityState::Unknown,
                    substituter: None,
                    nar_bytes: None,
                    download_bytes: None,
                });
        }
        let mut metrics = initial.metrics;
        let mut diagnostics = initial.diagnostics;
        if paths.is_empty() {
            return ProbeState {
                availability,
                diagnostics,
                metrics,
            };
        }

        let unknown_paths = paths
            .iter()
            .filter(|path| {
                availability
                    .get(*path)
                    .is_some_and(|entry| entry.state == crate::AvailabilityState::Unknown)
            })
            .cloned()
            .collect::<Vec<_>>();
        let local_failed = !unknown_paths.is_empty()
            && self.probe_local(
                flake,
                &unknown_paths,
                &mut availability,
                &mut metrics,
                &mut diagnostics,
            );
        let degraded_paths =
            self.probe_remotes(flake, &mut availability, &mut metrics, &mut diagnostics);
        for (path, entry) in &mut availability {
            if entry.state == crate::AvailabilityState::Unknown
                && !local_failed
                && !degraded_paths.contains(path)
            {
                entry.state = crate::AvailabilityState::Missing;
            }
        }
        ProbeState {
            availability,
            diagnostics,
            metrics,
        }
    }

    fn probe_local(
        &self,
        flake: &crate::FlakeRef,
        paths: &[String],
        availability: &mut BTreeMap<String, crate::Availability>,
        metrics: &mut PhaseMetrics,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> bool {
        match self.path_info(flake, paths, None) {
            Ok((present, process)) => {
                record_process(metrics, &process);
                for (path, sizes) in present {
                    if let Some(entry) = availability.get_mut(&path) {
                        entry.state = crate::AvailabilityState::Local;
                        entry.nar_bytes = sizes.nar_bytes;
                    }
                }
                false
            }
            Err(mut failure) => {
                let cancelled = failure.code == "cancelled";
                "local_cache_probe_failed".clone_into(&mut failure.code);
                failure.severity = if cancelled {
                    DiagnosticSeverity::Error
                } else {
                    DiagnosticSeverity::Warning
                };
                diagnostics.push(*failure);
                true
            }
        }
    }

    fn probe_remotes(
        &self,
        flake: &crate::FlakeRef,
        availability: &mut BTreeMap<String, crate::Availability>,
        metrics: &mut PhaseMetrics,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> BTreeSet<String> {
        let mut degraded_paths = BTreeSet::new();
        for cache in &self.config.trusted_substituters {
            let unresolved = availability
                .iter()
                .filter_map(|(path, entry)| {
                    (entry.state != crate::AvailabilityState::Local
                        && entry.state != crate::AvailabilityState::TrustedRemote)
                        .then_some(path.clone())
                })
                .collect::<Vec<_>>();
            if unresolved.is_empty() {
                break;
            }
            match self.path_info(flake, &unresolved, Some(&cache.url)) {
                Ok((present, process)) => {
                    record_process(metrics, &process);
                    for (path, sizes) in present {
                        if let Some(entry) = availability.get_mut(&path) {
                            entry.state = crate::AvailabilityState::TrustedRemote;
                            entry.substituter = Some(cache.url.clone());
                            entry.nar_bytes = sizes.nar_bytes;
                            entry.download_bytes = sizes.download_bytes.or(sizes.nar_bytes);
                        }
                    }
                }
                Err(mut failure) => {
                    degraded_paths.extend(unresolved);
                    let cancelled = failure.code == "cancelled";
                    "cache_probe_failed".clone_into(&mut failure.code);
                    failure.severity = if cancelled {
                        DiagnosticSeverity::Error
                    } else {
                        DiagnosticSeverity::Warning
                    };
                    failure.target = Some(cache.url.clone());
                    diagnostics.push(*failure);
                }
            }
        }
        degraded_paths
    }

    fn path_info(
        &self,
        flake: &crate::FlakeRef,
        paths: &[String],
        store: Option<&str>,
    ) -> Result<(BTreeMap<String, PathSizes>, ProcessResult), Box<Diagnostic>> {
        let mut spec = self
            .nix_spec(flake)
            .args(["path-info", "--json", "--stdin"]);
        if let Some(store) = store {
            spec.args
                .extend([OsString::from("--store"), OsString::from(store)]);
        } else {
            spec.args.push(OsString::from("--offline"));
        }
        let mut input = paths.join("\n");
        input.push('\n');
        spec.stdin = InputPolicy::Bytes(input.into_bytes());
        let process = self
            .run(&spec, "cache_probe_process_failed")
            .map_err(|error| {
                Box::new(diagnostic(
                    Phase::Probe,
                    error.code(),
                    store.map(str::to_owned),
                    error.message(),
                ))
            })?;
        if !process.termination.success() {
            return Err(Box::new(process_diagnostic(
                self,
                Phase::Probe,
                "cache_probe_failed",
                store.map(str::to_owned),
                format!("nix path-info failed with {:?}", process.termination),
                &process,
            )));
        }
        if process.stdout.truncated {
            return Err(Box::new(process_diagnostic(
                self,
                Phase::Probe,
                "process_output_limit_exceeded",
                store.map(str::to_owned),
                "path-info output exceeded the configured process output limit",
                &process,
            )));
        }
        parse_path_info(&process.stdout.bytes)
            .map(|paths| (paths, process.clone()))
            .map_err(|error| {
                Box::new(process_diagnostic(
                    self,
                    Phase::Probe,
                    error.code(),
                    store.map(str::to_owned),
                    error.message(),
                    &process,
                ))
            })
    }

    fn realize_graph(
        &self,
        flake: &crate::FlakeRef,
        graph: &DependencyGraph,
        selected: &BTreeMap<String, BTreeSet<String>>,
        required: &BTreeMap<String, BTreeSet<String>>,
        availability: &BTreeMap<String, crate::Availability>,
        policy: RealizationPolicy<'_>,
    ) -> RealizationState {
        let execution_required =
            match prune_execution_required(graph, selected, required, availability) {
                Ok(required) => required,
                Err(error) => {
                    return RealizationState {
                        executions: BTreeMap::new(),
                        metrics: PhaseMetrics::default(),
                        node_metrics: Vec::new(),
                        diagnostics: vec![diagnostic(
                            Phase::Realization,
                            error.code(),
                            None,
                            error.message(),
                        )],
                    };
                }
            };
        let (executions, diagnostics) = initialize_executions(
            graph,
            &execution_required,
            availability,
            policy.nonlocal_state,
            policy.out_link.is_some(),
        );
        let mut state = RealizationState {
            executions,
            metrics: PhaseMetrics::default(),
            node_metrics: Vec::new(),
            diagnostics,
        };
        self.run_realization_schedule(flake, graph, &mut state, policy.out_link);
        state
    }

    fn run_realization_schedule(
        &self,
        flake: &crate::FlakeRef,
        graph: &DependencyGraph,
        state: &mut RealizationState,
        out_link: Option<&std::path::Path>,
    ) {
        let pending = state
            .executions
            .iter()
            .filter(|(_, execution)| execution.state.is_none())
            .map(|(path, execution)| {
                (
                    path.clone(),
                    (execution.required_outputs.clone(), execution.expected_state),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if pending.is_empty() {
            return;
        }
        for path in pending.keys() {
            self.dependencies.progress.emit(ProgressEvent::NodeStarted {
                drv_path: path.clone(),
            });
        }
        let (mut results, recovery) = self.realize_nodes(flake, graph, &pending, out_link);
        if let Some(process) = recovery {
            record_process(&mut state.metrics, &process);
        }
        mark_dependency_failures(&mut results, &state.executions);
        for (path, result) in results {
            apply_node_run(state, path, result, self.dependencies.progress);
        }
    }

    fn realize_nodes(
        &self,
        flake: &crate::FlakeRef,
        graph: &DependencyGraph,
        required: &BTreeMap<String, (BTreeSet<String>, NodeState)>,
        out_link: Option<&std::path::Path>,
    ) -> (BTreeMap<String, NodeRun>, Option<ProcessResult>) {
        let installables = required
            .iter()
            .map(|(drv_path, (outputs, _))| {
                format!(
                    "{}^{}",
                    drv_path,
                    outputs.iter().cloned().collect::<Vec<_>>().join(",")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let workers = self.config.limits.substitution_concurrency.to_string();
        let mut spec = self.nix_spec(flake).args([
            "build",
            "--json",
            "--keep-going",
            "--option",
            "max-substitution-jobs",
            &workers,
            "--option",
            "http-connections",
            &workers,
            "--stdin",
        ]);
        if let Some(path) = out_link {
            spec.args.push("--out-link".into());
            spec.args.push(path.as_os_str().to_owned());
        } else {
            spec.args.push("--no-link".into());
        }
        spec.stdin = InputPolicy::Bytes(format!("{installables}\n").into_bytes());
        let process = match self.run(&spec, "realization_process_failed") {
            Ok(process) => process,
            Err(error) => {
                let state = if error.code() == "cancelled" {
                    NodeState::Cancelled
                } else {
                    NodeState::Failed
                };
                return (
                    required
                        .keys()
                        .map(|path| {
                            (
                                path.clone(),
                                NodeRun {
                                    state,
                                    produced_paths: Vec::new(),
                                    duration_ms: 0,
                                    process_duration_ms: 0,
                                    process_ran: false,
                                    dependency_failure: None,
                                    diagnostic: Some(diagnostic(
                                        Phase::Realization,
                                        error.code(),
                                        Some(path.clone()),
                                        error.message(),
                                    )),
                                },
                            )
                        })
                        .collect(),
                    None,
                );
            }
        };
        let mut results = required
            .iter()
            .enumerate()
            .map(|(index, (path, (outputs, expected_state)))| {
                let result = self.parse_node_run(
                    graph,
                    path,
                    outputs,
                    *expected_state,
                    index == 0,
                    &process,
                );
                (path.clone(), result)
            })
            .collect::<BTreeMap<_, _>>();
        let recovery = if process.termination.success() {
            None
        } else {
            self.recover_realized_nodes(flake, graph, required, &mut results)
        };
        (results, recovery)
    }

    fn recover_realized_nodes(
        &self,
        flake: &crate::FlakeRef,
        graph: &DependencyGraph,
        required: &BTreeMap<String, (BTreeSet<String>, NodeState)>,
        results: &mut BTreeMap<String, NodeRun>,
    ) -> Option<ProcessResult> {
        let unresolved = results
            .iter()
            .filter(|(_, result)| result.state == NodeState::Failed)
            .flat_map(|(path, _)| {
                required.get(path).into_iter().flat_map(|(outputs, _)| {
                    outputs.iter().filter_map(|output| {
                        graph
                            .get(path)
                            .and_then(|node| node.outputs.get(output))
                            .and_then(Option::as_ref)
                            .cloned()
                    })
                })
            })
            .collect::<Vec<_>>();
        if unresolved.is_empty() {
            return None;
        }
        let (present, process) = self.path_info(flake, &unresolved, None).ok()?;
        for (path, result) in results
            .iter_mut()
            .filter(|(_, result)| result.state == NodeState::Failed)
        {
            let Some((outputs, expected_state)) = required.get(path) else {
                continue;
            };
            let produced = outputs
                .iter()
                .filter_map(|output| {
                    graph
                        .get(path)
                        .and_then(|node| node.outputs.get(output))
                        .and_then(Option::as_ref)
                })
                .filter(|output| present.contains_key(*output))
                .cloned()
                .collect::<Vec<_>>();
            if produced.len() == outputs.len() {
                result.state = *expected_state;
                result.produced_paths = produced;
                result.diagnostic = None;
            }
        }
        Some(process)
    }

    fn parse_node_run(
        &self,
        graph: &DependencyGraph,
        path: &str,
        outputs: &BTreeSet<String>,
        expected_state: NodeState,
        process_ran: bool,
        process: &ProcessResult,
    ) -> NodeRun {
        let duration_ms = process.duration.as_millis().try_into().unwrap_or(u64::MAX);
        let parsed = graph
            .get(path)
            .ok_or_else(|| {
                EngineError::new(
                    "missing_graph_node",
                    format!("execution plan references missing derivation {path}"),
                )
            })
            .and_then(|node| parse_build_result(&process.stdout.bytes, node, outputs));
        match parsed {
            Ok(outputs) => NodeRun {
                state: expected_state,
                produced_paths: outputs.into_values().collect(),
                duration_ms: 0,
                process_duration_ms: if process_ran { duration_ms } else { 0 },
                process_ran,
                dependency_failure: None,
                diagnostic: None,
            },
            Err(error) => NodeRun {
                state: NodeState::Failed,
                produced_paths: Vec::new(),
                duration_ms: 0,
                process_duration_ms: if process_ran { duration_ms } else { 0 },
                process_ran,
                dependency_failure: None,
                diagnostic: Some(process_diagnostic(
                    self,
                    Phase::Realization,
                    if process.stdout.truncated {
                        "process_output_limit_exceeded"
                    } else {
                        "realization_failed"
                    },
                    Some(path.to_owned()),
                    if process.stdout.truncated {
                        "nix build output exceeded the configured process output limit".to_owned()
                    } else if process.termination.success() {
                        error.message().to_owned()
                    } else {
                        format!("nix build failed with {:?}", process.termination)
                    },
                    process,
                )),
            },
        }
    }

    fn finish_manifest(
        &self,
        mut roots: Vec<RootResult>,
        mut graph: Vec<crate::DerivationNode>,
        mut availability: Vec<crate::Availability>,
        mut nodes: Vec<crate::NodeResult>,
        mut diagnostics: Vec<Diagnostic>,
        mut metrics: ManifestMetrics,
    ) -> Manifest {
        roots.sort_by(|left, right| {
            (left.kind, &left.name, &left.drv_path).cmp(&(right.kind, &right.name, &right.drv_path))
        });
        graph.sort_by(|left, right| left.drv_path.cmp(&right.drv_path));
        availability.sort_by(|left, right| left.path.cmp(&right.path));
        nodes.sort_by(|left, right| left.drv_path.cmp(&right.drv_path));
        diagnostics.sort_by(diagnostic_order);
        metrics
            .nodes
            .sort_by(|left, right| left.drv_path.cmp(&right.drv_path));
        metrics.finished_at_ms = self.dependencies.clock.now_millis();
        let cancelled = self.dependencies.cancellation.signal().is_some()
            || roots.iter().any(|root| root.state == NodeState::Cancelled);
        if cancelled {
            for root in &mut roots {
                if root.state == NodeState::Failed && root.drv_path.is_some() {
                    root.state = NodeState::Cancelled;
                }
            }
        }
        let outcome = if cancelled {
            ManifestOutcome::Cancelled
        } else if diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity != DiagnosticSeverity::Error)
            && roots.iter().all(|root| {
                matches!(
                    root.state,
                    NodeState::Cached
                        | NodeState::Substituted
                        | NodeState::Built
                        | NodeState::Realized
                )
            })
        {
            ManifestOutcome::Success
        } else {
            ManifestOutcome::Failed
        };
        Manifest {
            schema: "nix-tools-engine/v1",
            system: self.config.system.to_string(),
            roots,
            graph,
            availability,
            nodes,
            diagnostics,
            metrics,
            outcome,
        }
    }
}

impl FlakeEngine for NixEngine<'_> {
    fn execute(&self, request: EngineRequest) -> Result<EngineResponse, EngineError> {
        match request {
            EngineRequest::Discover(request) => {
                self.discover(&request).map(EngineResponse::Discovery)
            }
            EngineRequest::Build(request) => self.build(request).map(EngineResponse::Realization),
            EngineRequest::Check(request) => self.check(request).map(EngineResponse::Realization),
            EngineRequest::Run(request) => {
                self.prepare_run(request).map(EngineResponse::PreparedRun)
            }
        }
    }
}

fn validate_root_identities(
    graph: &DependencyGraph,
    roots: &[EvaluatedRoot],
) -> Option<Diagnostic> {
    for root in roots {
        let node = graph.get(&root.drv_path)?;
        if root.kind == TargetKind::App {
            continue;
        }
        for output in &root.selected_outputs {
            let evaluated_path = root.outputs.get(output)?;
            let graph_path = node.outputs.get(output).and_then(Option::as_ref);
            if graph_path != Some(evaluated_path) {
                return Some(diagnostic(
                    Phase::Graph,
                    "root_output_identity_mismatch",
                    Some(root.name.clone()),
                    format!(
                        "graph output {output} for {} does not match its evaluated identity",
                        root.drv_path
                    ),
                ));
            }
        }
    }
    None
}

fn lightweight_root_nodes(roots: &[EvaluatedRoot]) -> BTreeMap<String, crate::DerivationNode> {
    let mut nodes = BTreeMap::new();
    for root in roots {
        let node = nodes
            .entry(root.drv_path.clone())
            .or_insert_with(|| crate::DerivationNode {
                drv_path: root.drv_path.clone(),
                dependencies: BTreeMap::new(),
                outputs: BTreeMap::new(),
            });
        node.outputs.extend(
            root.outputs
                .iter()
                .map(|(name, path)| (name.clone(), Some(path.clone()))),
        );
    }
    nodes
}

fn apply_failure_dependencies(
    graph: &DependencyGraph,
    executions: &mut BTreeMap<String, NodeExecution>,
) -> BTreeSet<String> {
    let selected = executions.keys().cloned().collect::<BTreeSet<_>>();
    for (path, execution) in executions.iter_mut() {
        execution.active_dependencies = graph
            .get(path)
            .into_iter()
            .flat_map(|node| node.dependencies.keys())
            .filter(|dependency| selected.contains(*dependency))
            .cloned()
            .collect();
    }
    let mut all_skipped = BTreeSet::new();
    loop {
        let skipped = executions
            .iter()
            .filter(|(_, execution)| execution.state == Some(NodeState::Failed))
            .filter_map(|(path, execution)| {
                execution.active_dependencies.iter().find_map(|dependency| {
                    executions.get(dependency).and_then(|dependency_execution| {
                        matches!(
                            dependency_execution.state,
                            Some(NodeState::Failed | NodeState::Skipped)
                        )
                        .then(|| (path.clone(), dependency.clone()))
                    })
                })
            })
            .collect::<Vec<_>>();
        if skipped.is_empty() {
            break;
        }
        for (path, dependency) in skipped {
            if let Some(execution) = executions.get_mut(&path) {
                execution.state = Some(NodeState::Skipped);
                execution.dependency_failure = Some(crate::DependencyFailure { dependency });
                all_skipped.insert(path);
            }
        }
    }
    all_skipped
}

fn populate_app_outputs(
    graph: &DependencyGraph,
    evaluated: &[EvaluatedRoot],
    roots: &mut [RootResult],
) {
    for evaluated_root in evaluated.iter().filter(|root| root.kind == TargetKind::App) {
        let Some(node) = graph.get(&evaluated_root.drv_path) else {
            continue;
        };
        let selected = if evaluated_root.selected_outputs.is_empty() {
            node.outputs.keys().cloned().collect::<BTreeSet<_>>()
        } else {
            evaluated_root.selected_outputs.clone()
        };
        let outputs = selected
            .iter()
            .filter_map(|output| {
                node.outputs
                    .get(output)
                    .and_then(Option::as_ref)
                    .map(|path| (output.clone(), path.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        if let Some(root) = roots.iter_mut().find(|root| {
            root.kind == TargetKind::App
                && root.name == evaluated_root.name
                && root.drv_path.as_ref() == Some(&evaluated_root.drv_path)
        }) {
            root.outputs = outputs;
        }
    }
}

fn selected_outputs(
    graph: &DependencyGraph,
    roots: &[EvaluatedRoot],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut selected = BTreeMap::<String, BTreeSet<String>>::new();
    for root in roots {
        let outputs = selected.entry(root.drv_path.clone()).or_default();
        if root.selected_outputs.is_empty() {
            if let Some(node) = graph.get(&root.drv_path) {
                outputs.extend(node.outputs.keys().cloned());
            }
        } else {
            outputs.extend(root.selected_outputs.iter().cloned());
        }
    }
    selected
}

fn parse_path_info(bytes: &[u8]) -> Result<BTreeMap<String, PathSizes>, EngineError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        EngineError::new(
            "invalid_path_info_json",
            format!("parse nix path-info JSON: {error}"),
        )
    })?;
    if let Some(object) = value.as_object() {
        return Ok(object
            .iter()
            .filter(|(_, metadata)| !metadata.is_null())
            .map(|(path, metadata)| (path.clone(), path_sizes(metadata)))
            .collect());
    }
    if let Some(array) = value.as_array() {
        return array
            .iter()
            .map(|entry| {
                let path = entry.get("path").and_then(Value::as_str).ok_or_else(|| {
                    EngineError::new(
                        "invalid_path_info_entry",
                        "nix path-info array entries must contain a path string",
                    )
                })?;
                Ok((path.to_owned(), path_sizes(entry)))
            })
            .collect();
    }
    Err(EngineError::new(
        "invalid_path_info_schema",
        "nix path-info JSON must be an object or array",
    ))
}

fn path_sizes(value: &Value) -> PathSizes {
    PathSizes {
        nar_bytes: value.get("narSize").and_then(Value::as_u64),
        download_bytes: value.get("downloadSize").and_then(Value::as_u64),
    }
}

fn prune_execution_required(
    graph: &DependencyGraph,
    selected: &BTreeMap<String, BTreeSet<String>>,
    required: &BTreeMap<String, BTreeSet<String>>,
    _availability: &BTreeMap<String, crate::Availability>,
) -> Result<BTreeMap<String, BTreeSet<String>>, EngineError> {
    selected
        .keys()
        .map(|path| {
            graph.get(path).ok_or_else(|| {
                EngineError::new(
                    "missing_graph_node",
                    format!("execution plan references missing derivation {path}"),
                )
            })?;
            let outputs = required.get(path).ok_or_else(|| {
                EngineError::new(
                    "missing_required_outputs",
                    format!("execution plan omitted required outputs for {path}"),
                )
            })?;
            Ok((path.clone(), outputs.clone()))
        })
        .collect()
}

fn initialize_executions(
    graph: &DependencyGraph,
    required: &BTreeMap<String, BTreeSet<String>>,
    availability: &BTreeMap<String, crate::Availability>,
    nonlocal_state: Option<NodeState>,
    force_realization: bool,
) -> (BTreeMap<String, NodeExecution>, Vec<Diagnostic>) {
    let mut executions = BTreeMap::new();
    let mut diagnostics = Vec::new();
    for (path, selected) in required {
        let Some(node) = graph.get(path) else {
            diagnostics.push(internal_schedule_diagnostic(path));
            executions.insert(
                path.clone(),
                NodeExecution {
                    state: Some(NodeState::Failed),
                    active_dependencies: BTreeSet::new(),
                    required_outputs: selected.clone(),
                    produced_paths: Vec::new(),
                    duration_ms: 0,
                    dependency_failure: None,
                    expected_state: nonlocal_state.unwrap_or(NodeState::Built),
                },
            );
            continue;
        };
        let selected = selected.clone();
        let states = selected
            .iter()
            .map(|output| {
                node.outputs
                    .get(output)
                    .and_then(Option::as_ref)
                    .and_then(|path| availability.get(path))
                    .map(|entry| entry.state)
            })
            .collect::<Vec<_>>();
        let cached = !states.is_empty()
            && states
                .iter()
                .all(|state| *state == Some(crate::AvailabilityState::Local));
        let substitutable = !states.is_empty()
            && states.iter().all(|state| {
                matches!(
                    state,
                    Some(crate::AvailabilityState::Local | crate::AvailabilityState::TrustedRemote)
                )
            })
            && states.contains(&Some(crate::AvailabilityState::TrustedRemote));
        let produced_paths = if cached {
            selected
                .iter()
                .filter_map(|output| node.outputs.get(output).and_then(Option::as_ref))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        executions.insert(
            path.clone(),
            NodeExecution {
                state: (cached && !force_realization).then_some(NodeState::Cached),
                active_dependencies: if cached || substitutable {
                    BTreeSet::new()
                } else {
                    node.dependencies
                        .keys()
                        .filter(|dependency| required.contains_key(*dependency))
                        .cloned()
                        .collect()
                },
                required_outputs: selected,
                produced_paths,
                duration_ms: 0,
                dependency_failure: None,
                expected_state: nonlocal_state.unwrap_or(if cached {
                    NodeState::Cached
                } else if substitutable {
                    NodeState::Substituted
                } else {
                    NodeState::Built
                }),
            },
        );
    }
    (executions, diagnostics)
}

fn internal_schedule_diagnostic(path: &str) -> Diagnostic {
    diagnostic(
        Phase::Realization,
        "schedule_invariant_failed",
        Some(path.to_owned()),
        "validated graph and execution plan diverged",
    )
}

fn mark_dependency_failures(
    results: &mut BTreeMap<String, NodeRun>,
    executions: &BTreeMap<String, NodeExecution>,
) {
    loop {
        let skipped = results
            .iter()
            .filter(|(_, result)| result.state == NodeState::Failed)
            .filter_map(|(path, _)| {
                executions.get(path).and_then(|execution| {
                    execution.active_dependencies.iter().find_map(|dependency| {
                        results
                            .get(dependency)
                            .filter(|result| {
                                matches!(result.state, NodeState::Failed | NodeState::Skipped)
                            })
                            .map(|_| (path.clone(), dependency.clone()))
                    })
                })
            })
            .collect::<Vec<_>>();
        if skipped.is_empty() {
            break;
        }
        for (path, dependency) in skipped {
            if let Some(result) = results.get_mut(&path) {
                result.state = NodeState::Skipped;
                result.dependency_failure = Some(crate::DependencyFailure { dependency });
                result.diagnostic = None;
            }
        }
    }
}

fn apply_node_run(
    state: &mut RealizationState,
    path: String,
    result: NodeRun,
    progress: &dyn crate::ProgressSink,
) {
    let Some(execution) = state.executions.get_mut(&path) else {
        state.diagnostics.push(internal_schedule_diagnostic(&path));
        return;
    };
    execution.state = Some(result.state);
    execution.produced_paths = result.produced_paths;
    execution.duration_ms = result.duration_ms;
    execution.dependency_failure = result.dependency_failure;
    if result.process_ran {
        record_node_process(&mut state.metrics, result.process_duration_ms);
    }
    state.node_metrics.push(crate::NodeMetrics {
        drv_path: path.clone(),
        duration_ms: result.duration_ms,
    });
    if let Some(failure) = result.diagnostic {
        state.diagnostics.push(failure);
    }
    progress.emit(ProgressEvent::NodeFinished {
        drv_path: path,
        state: result.state,
    });
}

fn parse_build_result(
    bytes: &[u8],
    node: &crate::DerivationNode,
    required: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>, EngineError> {
    let entries: Vec<Value> = serde_json::from_slice(bytes).map_err(|error| {
        EngineError::new(
            "invalid_build_json",
            format!("parse nix build JSON: {error}"),
        )
    })?;
    for entry in entries {
        let Some(object) = entry.as_object() else {
            return Err(EngineError::new(
                "invalid_build_entry",
                "nix build entries must be objects",
            ));
        };
        if object.get("drvPath").and_then(Value::as_str) != Some(&node.drv_path) {
            continue;
        }
        let raw_outputs = object
            .get("outputs")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                EngineError::new(
                    "missing_build_outputs",
                    format!("nix build entry for {} omitted outputs", node.drv_path),
                )
            })?;
        let outputs = raw_outputs
            .iter()
            .map(|(name, value)| {
                let path = value
                    .as_str()
                    .or_else(|| value.get("path").and_then(Value::as_str))
                    .ok_or_else(|| {
                        EngineError::new(
                            "invalid_build_output",
                            format!("nix build output {name} must contain a path string"),
                        )
                    })?;
                Ok((name.clone(), path.to_owned()))
            })
            .collect::<Result<BTreeMap<_, _>, EngineError>>()?;
        for output in required {
            let actual = outputs.get(output).ok_or_else(|| {
                EngineError::new(
                    "missing_required_build_output",
                    format!(
                        "nix build result omitted output {output} from {}",
                        node.drv_path
                    ),
                )
            })?;
            if let Some(expected) = node.outputs.get(output).and_then(Option::as_ref)
                && actual != expected
            {
                return Err(EngineError::new(
                    "build_output_identity_mismatch",
                    format!(
                        "nix build returned {actual} for {}^{output}, expected {expected}",
                        node.drv_path
                    ),
                ));
            }
        }
        return Ok(outputs
            .into_iter()
            .filter(|(name, _)| required.contains(name))
            .collect());
    }
    Err(EngineError::new(
        "missing_requested_build",
        format!(
            "nix build result omitted requested derivation {}",
            node.drv_path
        ),
    ))
}

fn apply_root_states(executions: &BTreeMap<String, NodeExecution>, roots: &mut [RootResult]) {
    for root in roots {
        if let Some(drv_path) = root.drv_path.as_ref()
            && let Some(state) = executions
                .get(drv_path)
                .and_then(|execution| execution.state)
        {
            root.state = state;
        }
    }
}

fn node_results(executions: BTreeMap<String, NodeExecution>) -> Vec<crate::NodeResult> {
    executions
        .into_iter()
        .map(|(drv_path, mut execution)| {
            execution.produced_paths.sort();
            crate::NodeResult {
                drv_path,
                dependencies: execution.active_dependencies.into_iter().collect(),
                required_outputs: execution.required_outputs,
                produced_paths: execution.produced_paths,
                state: execution.state.unwrap_or(NodeState::Failed),
                dependency_failure: execution.dependency_failure,
            }
        })
        .collect()
}

fn validate_config(config: &EngineConfig) -> Result<(), EngineError> {
    let limits = config.limits;
    let values = [
        ("evaluation_batch_size", limits.evaluation_batch_size),
        ("evaluation_concurrency", limits.evaluation_concurrency),
        ("substitution_concurrency", limits.substitution_concurrency),
        ("max_process_output_bytes", limits.max_process_output_bytes),
        (
            "max_evaluation_memory_bytes",
            limits.max_evaluation_memory_bytes,
        ),
        ("max_roots", limits.max_roots),
        ("max_graph_nodes", limits.max_graph_nodes),
        ("max_diagnostic_bytes", limits.max_diagnostic_bytes),
    ];
    if let Some((name, _)) = values.into_iter().find(|(_, value)| *value == 0) {
        return Err(EngineError::new(
            "invalid_resource_limit",
            format!("resource limit {name} must be greater than zero"),
        ));
    }
    let mut urls = BTreeSet::new();
    for substituter in &config.trusted_substituters {
        if !valid_nix_config_atom(&substituter.url) {
            return Err(EngineError::new(
                "invalid_substituter",
                "trusted substituter URL must be a non-empty single token",
            ));
        }
        if !urls.insert(&substituter.url) {
            return Err(EngineError::new(
                "duplicate_substituter",
                format!(
                    "trusted substituter appears more than once: {}",
                    substituter.url
                ),
            ));
        }
        if substituter.public_keys.is_empty() {
            return Err(EngineError::new(
                "missing_substituter_key",
                format!("trusted substituter {} has no public key", substituter.url),
            ));
        }
        if substituter
            .public_keys
            .iter()
            .any(|key| !valid_nix_config_atom(key))
        {
            return Err(EngineError::new(
                "invalid_substituter_key",
                "trusted substituter keys must be non-empty single tokens",
            ));
        }
    }
    Ok(())
}

fn valid_nix_config_atom(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| !byte.is_ascii_whitespace() && !byte.is_ascii_control())
}

fn resolve_flake_reference(flake: &crate::FlakeRef) -> Result<String, EngineError> {
    let (prefix, path, suffix) = if let Some(path_reference) = flake.reference.strip_prefix("path:")
    {
        let (path, suffix) = path_reference
            .split_once('?')
            .map_or((path_reference, ""), |(path, query)| (path, query));
        if path.is_empty() || Path::new(path).is_absolute() {
            return Ok(flake.reference.clone());
        }
        ("path:", path, (!suffix.is_empty()).then_some(suffix))
    } else if explicit_relative_path(&flake.reference) {
        ("", flake.reference.as_str(), None)
    } else {
        return Ok(flake.reference.clone());
    };
    let current_directory = std::env::current_dir().map_err(|error| {
        EngineError::new(
            "current_directory_failed",
            format!("resolve relative flake reference: {error}"),
        )
    })?;
    let base = flake.working_directory.as_ref().map_or_else(
        || current_directory.clone(),
        |working_directory| {
            if working_directory.is_absolute() {
                working_directory.clone()
            } else {
                current_directory.join(working_directory)
            }
        },
    );
    let resolved = lexical_normalize(&base.join(path));
    if prefix.is_empty()
        && let Some(reference) = git_flake_reference(&resolved)?
    {
        return Ok(reference);
    }
    let resolved = resolved.to_str().ok_or_else(|| {
        EngineError::new(
            "non_utf8_flake_path",
            "resolved flake filesystem path is not valid UTF-8",
        )
    })?;
    Ok(suffix.map_or_else(
        || format!("{prefix}{resolved}"),
        |query| format!("{prefix}{resolved}?{query}"),
    ))
}

fn git_flake_reference(path: &Path) -> Result<Option<String>, EngineError> {
    let Some(root) = path
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
    else {
        return Ok(None);
    };
    let relative = path.strip_prefix(root).map_err(|error| {
        EngineError::new(
            "git_flake_path_failed",
            format!("resolve flake path beneath Git root: {error}"),
        )
    })?;
    let root = root
        .to_str()
        .ok_or_else(|| EngineError::new("non_utf8_flake_path", "Git root is not valid UTF-8"))?;
    let mut reference = format!("git+file://{}", percent_encode_path(root));
    if !relative.as_os_str().is_empty() {
        let relative = relative.to_str().ok_or_else(|| {
            EngineError::new(
                "non_utf8_flake_path",
                "flake subdirectory is not valid UTF-8",
            )
        })?;
        reference.push_str("?dir=");
        reference.push_str(&percent_encode_path(relative));
    }
    Ok(Some(reference))
}

fn percent_encode_path(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn explicit_relative_path(reference: &str) -> bool {
    matches!(reference, "." | "..") || reference.starts_with("./") || reference.starts_with("../")
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn sort_deduplicate(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

fn require_complete_output(result: &ProcessResult, phase: &str) -> Result<(), EngineError> {
    if result.stdout.truncated {
        return Err(EngineError::new(
            "process_output_limit_exceeded",
            format!("{phase} output exceeded the configured process output limit"),
        ));
    }
    Ok(())
}

fn bounded_redacted_stderr<'a>(
    bytes: &[u8],
    limit: usize,
    sensitive_values: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut redacted = String::from_utf8_lossy(bytes).into_owned();
    for sensitive in sensitive_values
        .into_iter()
        .filter(|value| !value.is_empty())
    {
        redacted = redacted.replace(sensitive, "[REDACTED]");
    }
    redacted = redacted
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                '�'
            } else {
                character
            }
        })
        .collect();
    bounded_text(redacted.as_bytes(), limit).0
}

fn diagnostic(
    phase: Phase,
    code: &str,
    target: Option<String>,
    message: impl Into<String>,
) -> Diagnostic {
    Diagnostic {
        phase,
        code: code.to_owned(),
        severity: DiagnosticSeverity::Error,
        target,
        message: message.into(),
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
    }
}

fn process_diagnostic(
    engine: &NixEngine<'_>,
    phase: Phase,
    code: &str,
    target: Option<String>,
    message: impl Into<String>,
    result: &ProcessResult,
) -> Diagnostic {
    let limit = engine.config.limits.max_diagnostic_bytes;
    let (stdout, stdout_truncated) = bounded_text(&result.stdout.bytes, limit);
    let (stderr, stderr_truncated) = bounded_text(&result.stderr.bytes, limit);
    Diagnostic {
        phase,
        code: code.to_owned(),
        severity: DiagnosticSeverity::Error,
        target,
        message: message.into(),
        stdout,
        stderr,
        truncated: result.stdout.truncated
            || result.stderr.truncated
            || stdout_truncated
            || stderr_truncated,
    }
}

fn bounded_text(bytes: &[u8], limit: usize) -> (String, bool) {
    let end = bytes.len().min(limit);
    let mut end = end;
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    (
        String::from_utf8_lossy(&bytes[..end]).into_owned(),
        bytes.len() > end,
    )
}

fn record_process(metrics: &mut PhaseMetrics, result: &ProcessResult) {
    metrics.processes = metrics.processes.saturating_add(1);
    metrics.duration_ms = metrics
        .duration_ms
        .saturating_add(result.duration.as_millis().try_into().unwrap_or(u64::MAX));
}

fn record_node_process(metrics: &mut PhaseMetrics, duration_ms: u64) {
    metrics.processes = metrics.processes.saturating_add(1);
    metrics.duration_ms = metrics.duration_ms.saturating_add(duration_ms);
}

fn merge_phase_metrics(metrics: &mut PhaseMetrics, other: PhaseMetrics) {
    metrics.processes = metrics.processes.saturating_add(other.processes);
    metrics.duration_ms = metrics.duration_ms.saturating_add(other.duration_ms);
}

fn diagnostic_order(left: &Diagnostic, right: &Diagnostic) -> std::cmp::Ordering {
    (
        left.phase,
        &left.target,
        &left.code,
        &left.message,
        left.severity,
    )
        .cmp(&(
            right.phase,
            &right.target,
            &right.code,
            &right.message,
            right.severity,
        ))
}
