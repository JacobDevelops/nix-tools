use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use nix_tools_core::outcome::{Error, Result};
#[cfg(feature = "nix-integration")]
use nix_tools_core::process::StdProcessRunner;
use nix_tools_core::process::{
    Cancellation, CapturedStream, ChildTermination, ProcessResult, ProcessRunner, ProcessSpec,
};
#[cfg(feature = "nix-integration")]
use nix_tools_core::redaction::Redactor;
use nix_tools_core::system::NixSystem;
use serde_json::{Value, json};

#[cfg(feature = "nix-integration")]
use super::SystemClock;
use super::{
    AvailabilityState, BuildRequest, CheckRequest, Clock, DiscoverRequest, EngineConfig,
    EngineDependencies, FlakeRef, NixEngine, NodeState, Phase, PreparedRun, ProgressEvent,
    ProgressSink, ResourceLimits, RunRequest, TrustedSubstituter,
};

const DRV_A: &str = "/nix/store/00000000000000000000000000000000-a.drv";
const DRV_B: &str = "/nix/store/11111111111111111111111111111111-b.drv";
const DRV_C: &str = "/nix/store/22222222222222222222222222222222-c.drv";
const OUT_A: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a";
const OUT_B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b";
const OUT_C: &str = "/nix/store/cccccccccccccccccccccccccccccccc-c";

#[derive(Clone)]
enum Evaluation {
    Success { drv_path: String, output: String },
    Failure,
}

struct FakeRunner {
    discovered: Value,
    discovery_failure: Option<(i32, Vec<u8>)>,
    evaluation_failure: Option<(i32, Vec<u8>)>,
    evaluations: BTreeMap<(String, String), Evaluation>,
    graph: Value,
    local: BTreeSet<String>,
    remote: BTreeMap<String, BTreeSet<String>>,
    degraded: BTreeSet<String>,
    build_failures: BTreeSet<String>,
    cancel_build: Option<String>,
    app_program: String,
    app_context: Value,
    truncate_evaluation: bool,
    calls: Mutex<Vec<ProcessSpec>>,
    builds: Mutex<Vec<String>>,
}

#[cfg(feature = "nix-integration")]
struct RecordingRunner {
    inner: StdProcessRunner,
    builds: Mutex<Vec<ProcessResult>>,
}

#[cfg(feature = "nix-integration")]
impl ProcessRunner for RecordingRunner {
    fn run(&self, spec: &ProcessSpec, cancellation: &Cancellation) -> Result<ProcessResult> {
        let result = self.inner.run(spec, cancellation)?;
        if FakeRunner::args(spec)
            .first()
            .is_some_and(|arg| arg == "build")
        {
            self.builds
                .lock()
                .expect("recorded builds")
                .push(result.clone());
        }
        Ok(result)
    }
}

impl Default for FakeRunner {
    fn default() -> Self {
        Self {
            discovered: json!({"packages": [], "checks": [], "apps": []}),
            discovery_failure: None,
            evaluation_failure: None,
            evaluations: BTreeMap::new(),
            graph: json!({}),
            local: BTreeSet::new(),
            remote: BTreeMap::new(),
            degraded: BTreeSet::new(),
            build_failures: BTreeSet::new(),
            cancel_build: None,
            app_program: String::new(),
            app_context: json!({}),
            truncate_evaluation: false,
            calls: Mutex::new(Vec::new()),
            builds: Mutex::new(Vec::new()),
        }
    }
}

impl FakeRunner {
    fn args(spec: &ProcessSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn stdin(spec: &ProcessSpec) -> String {
        match &spec.stdin {
            nix_tools_core::process::InputPolicy::Bytes(bytes) => {
                String::from_utf8(bytes.clone()).expect("UTF-8 stdin")
            }
            _ => String::new(),
        }
    }

    fn evaluation(&self, spec: &ProcessSpec) -> ProcessResult {
        if let Some((code, stderr)) = &self.evaluation_failure {
            return process_with_code(*code, stderr);
        }
        let targets: Vec<Value> = if let Some(targets) = Self::env(spec, "NIX_TOOLS_ENGINE_TARGETS")
        {
            serde_json::from_str(&targets).expect("target JSON")
        } else {
            let kind = Self::env(spec, "NIX_TOOLS_ENGINE_KIND").expect("target kind");
            self.evaluations
                .keys()
                .filter(|(candidate, _)| candidate == &kind)
                .map(|(_, name)| json!({"kind": kind, "name": name}))
                .collect()
        };
        if let Some(max_roots) = Self::env(spec, "NIX_TOOLS_ENGINE_MAX_ROOTS") {
            let max_roots = max_roots.parse::<usize>().expect("maximum roots");
            if targets.len() > max_roots {
                return process(0, &json!({"exceeded": true, "count": targets.len()}));
            }
        }
        let attempts = targets
            .iter()
            .map(|target| {
                let kind = target["kind"].as_str().expect("kind");
                let name = target["name"].as_str().expect("name");
                match self
                    .evaluations
                    .get(&(kind.to_owned(), name.to_owned()))
                    .expect("configured target")
                {
                    Evaluation::Success { drv_path, output } => json!({
                        "success": true,
                        "value": {
                            "drvPath": drv_path,
                            "outputs": {"out": output},
                            "outputsToInstall": ["out"]
                        }
                    }),
                    Evaluation::Failure => json!({ "success": false, "value": null }),
                }
            })
            .collect::<Vec<_>>();
        let value = if spec.env.contains_key(OsStr::new("NIX_TOOLS_ENGINE_KIND")) {
            json!({
                "exceeded": false,
                "names": targets.iter().map(|target| target["name"].clone()).collect::<Vec<_>>(),
                "attempts": attempts
            })
        } else {
            json!(attempts)
        };
        let mut result = process(0, &value);
        result.stdout.truncated = self.truncate_evaluation;
        result
    }

    fn env(spec: &ProcessSpec, name: &str) -> Option<String> {
        spec.env
            .get(OsStr::new(name))
            .map(|value| value.to_string_lossy().into_owned())
    }

    fn path_info(&self, args: &[String], spec: &ProcessSpec) -> ProcessResult {
        let requested = Self::stdin(spec)
            .lines()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let store = args
            .windows(2)
            .find_map(|pair| (pair[0] == "--store").then_some(pair[1].as_str()));
        if store.is_some_and(|store| self.degraded.contains(store)) {
            return process_with_code(23, b"cache unavailable");
        }
        let available = store.map_or(&self.local, |store| {
            self.remote.get(store).unwrap_or(&self.local)
        });
        let entries = requested
            .intersection(available)
            .map(|path| (path.clone(), json!({"path": path, "narSize": 10})))
            .collect::<serde_json::Map<_, _>>();
        process(0, &Value::Object(entries))
    }

    fn build(&self, spec: &ProcessSpec) -> ProcessResult {
        let drv_paths = Self::stdin(spec)
            .lines()
            .map(|installable| {
                installable
                    .split_once('^')
                    .map_or(installable, |(drv, _)| drv)
                    .to_owned()
            })
            .collect::<Vec<_>>();
        self.builds
            .lock()
            .expect("builds")
            .extend(drv_paths.iter().cloned());
        let entries = drv_paths
            .iter()
            .filter(|drv_path| !self.build_failures.contains(*drv_path))
            .map(|drv_path| {
                let output = graph_output(&self.graph, drv_path);
                json!({"drvPath": drv_path, "outputs": {"out": output}})
            })
            .collect::<Vec<_>>();
        let mut result = process(0, &json!(entries));
        if drv_paths
            .iter()
            .any(|drv_path| self.build_failures.contains(drv_path))
        {
            result.termination = ChildTermination::Exited(42);
            result.stderr.bytes = b"builder failed".to_vec();
        }
        result
    }

    fn calls(&self, command: &str) -> Vec<ProcessSpec> {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|spec| Self::args(spec).first().is_some_and(|arg| arg == command))
            .cloned()
            .collect()
    }
}

impl ProcessRunner for FakeRunner {
    fn run(&self, spec: &ProcessSpec, cancellation: &Cancellation) -> Result<ProcessResult> {
        if let Some(signal) = cancellation.signal() {
            return Err(Error::cancelled(signal, "fake cancelled"));
        }
        self.calls.lock().expect("calls").push(spec.clone());
        let args = Self::args(spec);
        match args.first().map(String::as_str) {
            Some("eval")
                if spec
                    .env
                    .contains_key(OsStr::new("NIX_TOOLS_ENGINE_TARGETS"))
                    || spec.env.contains_key(OsStr::new("NIX_TOOLS_ENGINE_KIND")) =>
            {
                Ok(self.evaluation(spec))
            }
            Some("eval") if spec.env.contains_key(OsStr::new("NIX_TOOLS_ENGINE_APP")) => {
                Ok(process(
                    0,
                    &json!({"program": self.app_program, "context": self.app_context}),
                ))
            }
            Some("eval") => self.discovery_failure.as_ref().map_or_else(
                || Ok(process(0, &self.discovered)),
                |(code, stderr)| Ok(process_with_code(*code, stderr)),
            ),
            Some("derivation") => Ok(process(0, &self.graph)),
            Some("path-info") => Ok(self.path_info(&args, spec)),
            Some("build") => {
                let should_cancel = Self::stdin(spec).lines().any(|installable| {
                    let drv_path = installable
                        .split_once('^')
                        .map_or(installable, |(drv_path, _)| drv_path);
                    self.cancel_build.as_deref() == Some(drv_path)
                });
                if should_cancel {
                    cancellation.request(2);
                    Err(Error::cancelled(2, "fake build cancelled"))
                } else {
                    Ok(self.build(spec))
                }
            }
            command => panic!("unexpected fake command: {command:?}"),
        }
    }
}

#[derive(Default)]
struct FakeClock {
    values: Mutex<VecDeque<u64>>,
}

impl FakeClock {
    fn with(values: impl IntoIterator<Item = u64>) -> Self {
        Self {
            values: Mutex::new(values.into_iter().collect()),
        }
    }
}

impl Clock for FakeClock {
    fn now_millis(&self) -> u64 {
        self.values.lock().expect("clock").pop_front().unwrap_or(0)
    }
}

#[derive(Default)]
struct FakeProgress(Mutex<Vec<ProgressEvent>>);

impl ProgressSink for FakeProgress {
    fn emit(&self, event: ProgressEvent) {
        self.0.lock().expect("progress").push(event);
    }
}

fn process(code: i32, value: &Value) -> ProcessResult {
    ProcessResult {
        termination: ChildTermination::Exited(code),
        stdout: CapturedStream {
            bytes: serde_json::to_vec(value).expect("JSON"),
            truncated: false,
        },
        stderr: CapturedStream::default(),
        combined: None,
        duration: Duration::from_millis(5),
    }
}

fn process_with_code(code: i32, stderr: &[u8]) -> ProcessResult {
    ProcessResult {
        termination: ChildTermination::Exited(code),
        stdout: CapturedStream::default(),
        stderr: CapturedStream {
            bytes: stderr.to_vec(),
            truncated: false,
        },
        combined: None,
        duration: Duration::from_millis(5),
    }
}

fn graph_output(graph: &Value, drv_path: &str) -> String {
    graph[drv_path]["outputs"]["out"]["path"]
        .as_str()
        .expect("graph output")
        .to_owned()
}

fn node(drv_path: &str, output: &str, dependencies: &[(&str, &[&str])]) -> (String, Value) {
    let input_drvs = dependencies
        .iter()
        .map(|(path, outputs)| (path.to_string(), json!({"outputs": outputs})))
        .collect::<serde_json::Map<_, _>>();
    (
        drv_path.to_owned(),
        json!({"outputs": {"out": {"path": output}}, "inputDrvs": input_drvs}),
    )
}

fn graph(nodes: impl IntoIterator<Item = (String, Value)>) -> Value {
    Value::Object(nodes.into_iter().collect())
}

fn evaluation(drv_path: &str, output: &str) -> Evaluation {
    Evaluation::Success {
        drv_path: drv_path.to_owned(),
        output: output.to_owned(),
    }
}

fn flake() -> FlakeRef {
    FlakeRef::new(".", Some(PathBuf::from("/workspace")))
}

fn limits() -> ResourceLimits {
    ResourceLimits {
        evaluation_batch_size: 2,
        evaluation_concurrency: 2,
        substitution_concurrency: 2,
        max_process_output_bytes: 64 * 1024,
        max_evaluation_memory_bytes: 64 * 1024,
        max_roots: 32,
        max_graph_nodes: 128,
        max_diagnostic_bytes: 8 * 1024,
    }
}

fn config(limits: ResourceLimits) -> EngineConfig {
    EngineConfig {
        nix_executable: OsString::from("custom-nix"),
        system: NixSystem::X86_64Linux,
        trusted_substituters: vec![TrustedSubstituter {
            url: "https://cache.example".to_owned(),
            public_keys: BTreeSet::from(["cache.example-1:public-key".to_owned()]),
        }],
        limits,
    }
}

fn build(runner: &FakeRunner, names: &[&str], limits: ResourceLimits) -> super::Manifest {
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([100, 200]);
    let progress = FakeProgress::default();
    let engine = NixEngine::new(
        config(limits),
        EngineDependencies {
            runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");
    engine
        .build(BuildRequest {
            flake: flake(),
            targets: names.iter().map(|name| (*name).to_owned()).collect(),
        })
        .expect("manifest")
}

#[test]
fn rejects_nix_config_line_injection_before_starting_a_process() {
    let runner = FakeRunner::default();
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([]);
    let progress = FakeProgress::default();
    let dependencies = EngineDependencies {
        runner: &runner,
        cancellation: &cancellation,
        clock: &clock,
        progress: &progress,
    };
    let mut injected_url = config(limits());
    injected_url.trusted_substituters[0].url = "https://cache.example\nsandbox = false".to_owned();

    let url_error = NixEngine::new(injected_url, dependencies)
        .err()
        .expect("a URL cannot add another Nix setting");

    let mut injected_key = config(limits());
    injected_key.trusted_substituters[0].public_keys =
        BTreeSet::from(["cache.example:key\nfallback = true".to_owned()]);
    let key_error = NixEngine::new(injected_key, dependencies)
        .err()
        .expect("a key cannot add another Nix setting");
    assert_eq!(url_error.code(), "invalid_substituter");
    assert_eq!(key_error.code(), "invalid_substituter_key");
}

#[test]
fn discovers_sorted_standard_flake_outputs() {
    let runner = FakeRunner {
        discovered: json!({
            "packages": ["zeta", "alpha", "alpha"],
            "checks": ["test", "fmt"],
            "apps": ["serve"]
        }),
        ..FakeRunner::default()
    };
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([]);
    let progress = FakeProgress::default();
    let engine = NixEngine::new(
        config(limits()),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");

    let discovered = engine
        .discover(&DiscoverRequest { flake: flake() })
        .expect("discover");

    assert_eq!(discovered.packages, ["alpha", "zeta"]);
    assert_eq!(discovered.checks, ["fmt", "test"]);
    assert_eq!(discovered.apps, ["serve"]);
    let spec = &runner.calls("eval")[0];
    assert_eq!(spec.program, OsStr::new("custom-nix"));
}

#[test]
fn resolves_filesystem_flake_references_and_preserves_opaque_references() {
    let runner = FakeRunner::default();
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([]);
    let progress = FakeProgress::default();
    let engine = NixEngine::new(
        config(limits()),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");
    let filesystem = [
        (".", Some("/workspace/repo"), "/workspace/repo".to_owned()),
        (
            "./flake",
            Some("/workspace/repo"),
            "/workspace/repo/flake".to_owned(),
        ),
        (
            "../flake",
            Some("/workspace/repo"),
            "/workspace/flake".to_owned(),
        ),
        (
            "path:../flake?dir=source",
            Some("/workspace/repo"),
            "path:/workspace/flake?dir=source".to_owned(),
        ),
    ];
    for (reference, working_directory, expected) in filesystem {
        engine
            .discover(&DiscoverRequest {
                flake: FlakeRef::new(reference, working_directory.map(PathBuf::from)),
            })
            .expect("discover filesystem reference");
        let calls = runner.calls("eval");
        assert_eq!(
            FakeRunner::env(calls.last().expect("eval"), "NIX_TOOLS_ENGINE_FLAKE")
                .expect("flake environment"),
            expected
        );
    }

    let git_root =
        std::env::temp_dir().join(format!("nix tools engine git {}", std::process::id()));
    let nested = git_root.join("nested flake");
    fs::create_dir_all(git_root.join(".git")).expect("create fake Git root");
    fs::create_dir_all(&nested).expect("create nested flake path");
    engine
        .discover(&DiscoverRequest {
            flake: FlakeRef::new(".", Some(nested)),
        })
        .expect("discover Git-tracked filesystem reference");
    let calls = runner.calls("eval");
    let expected_root = git_root.to_string_lossy().replace(' ', "%20");
    assert_eq!(
        FakeRunner::env(calls.last().expect("eval"), "NIX_TOOLS_ENGINE_FLAKE")
            .expect("flake environment"),
        format!("git+file://{expected_root}?dir=nested%20flake")
    );
    fs::remove_dir_all(git_root).expect("remove fake Git root");

    for reference in [
        "nixpkgs",
        "github:owner/repo",
        "git+https://example.test/repo",
        "/absolute/flake",
        "path:/absolute/flake",
    ] {
        engine
            .discover(&DiscoverRequest {
                flake: FlakeRef::new(reference, Some(PathBuf::from("/workspace/repo"))),
            })
            .expect("discover opaque reference");
        let calls = runner.calls("eval");
        assert_eq!(
            FakeRunner::env(calls.last().expect("eval"), "NIX_TOOLS_ENGINE_FLAKE")
                .expect("flake environment"),
            reference
        );
    }
}

#[test]
fn fatal_discovery_includes_bounded_stderr_with_flake_reference_redacted() {
    let secret_reference = "github:owner/private?token=SECRET_CANARY";
    let runner = FakeRunner {
        discovery_failure: Some((
            1,
            format!(
                "cannot fetch {secret_reference}: permission denied {}",
                "x".repeat(512)
            )
            .into_bytes(),
        )),
        ..FakeRunner::default()
    };
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([]);
    let progress = FakeProgress::default();
    let mut bounded = limits();
    bounded.max_diagnostic_bytes = 96;
    let engine = NixEngine::new(
        config(bounded),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");

    let error = engine
        .discover(&DiscoverRequest {
            flake: FlakeRef::new(secret_reference, None),
        })
        .expect_err("discovery failure");

    assert_eq!(error.code(), "discovery_failed");
    assert!(error.message().contains("permission denied"));
    assert!(error.message().contains("[REDACTED]"));
    assert!(!error.message().contains("SECRET_CANARY"));
    assert!(error.message().len() < 180);
}

#[test]
fn evaluates_roots_in_bounded_batches_and_tracks_injected_clock() {
    let names = ["a", "b", "c", "d", "e"];
    let mut runner = FakeRunner::default();
    for name in names {
        runner.evaluations.insert(
            ("packages".to_owned(), name.to_owned()),
            evaluation(DRV_A, OUT_A),
        );
    }
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);

    let manifest = build(&runner, &names, limits());

    let batches = runner
        .calls("eval")
        .into_iter()
        .filter(|spec| FakeRunner::env(spec, "NIX_TOOLS_ENGINE_TARGETS").is_some())
        .collect::<Vec<_>>();
    assert_eq!(batches.len(), 3);
    assert!(batches.iter().all(|spec| {
        let roots: Vec<Value> = serde_json::from_str(
            &FakeRunner::env(spec, "NIX_TOOLS_ENGINE_TARGETS").expect("targets"),
        )
        .expect("JSON");
        roots.len() <= 2
    }));
    assert!(batches.iter().all(|spec| matches!(
        spec.stdout,
        nix_tools_core::process::StreamPolicy::Capture { limit: 65_536 }
    )));
    assert_eq!(manifest.metrics.started_at_ms, 100);
    assert_eq!(manifest.metrics.finished_at_ms, 200);
    assert_eq!(manifest.metrics.evaluation.processes, 3);
}

#[test]
fn evaluates_all_names_and_identities_without_a_discovery_process() {
    let mut runner = FakeRunner::default();
    runner.evaluations.extend([
        (
            ("packages".to_owned(), "zeta".to_owned()),
            evaluation(DRV_B, OUT_B),
        ),
        (
            ("packages".to_owned(), "alpha".to_owned()),
            evaluation(DRV_A, OUT_A),
        ),
    ]);
    runner.graph = graph([node(DRV_A, OUT_A, &[]), node(DRV_B, OUT_B, &[])]);

    let manifest = build(&runner, &[], limits());

    assert_eq!(
        manifest
            .roots
            .iter()
            .map(|root| root.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(runner.calls("eval").len(), 1);
    assert!(
        runner.calls("eval")[0]
            .env
            .contains_key(OsStr::new("NIX_TOOLS_ENGINE_KIND"))
    );

    let mut checks = FakeRunner::default();
    checks.evaluations.insert(
        ("checks".to_owned(), "unit".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    checks.graph = graph([node(DRV_A, OUT_A, &[])]);
    let manifest = build_engine(&checks, limits())
        .check(CheckRequest {
            flake: flake(),
            targets: Vec::new(),
        })
        .expect("check all");
    assert_eq!(manifest.roots[0].name, "unit");
    assert_eq!(checks.calls("eval").len(), 1);
    assert_eq!(
        FakeRunner::env(&checks.calls("eval")[0], "NIX_TOOLS_ENGINE_KIND").as_deref(),
        Some("checks")
    );
}

#[test]
fn combined_evaluation_handles_empty_outputs_and_rejects_failed_processes() {
    let empty = FakeRunner::default();
    let manifest = build(&empty, &[], limits());
    assert!(manifest.roots.is_empty());
    assert_eq!(manifest.outcome, super::ManifestOutcome::Success);
    assert_eq!(empty.calls("eval").len(), 1);

    let failed = FakeRunner {
        evaluation_failure: Some((23, b"evaluation failed".to_vec())),
        ..FakeRunner::default()
    };
    let manifest = build(&failed, &[], limits());
    assert_eq!(manifest.diagnostics[0].code, "evaluation_failed");
    assert_eq!(manifest.outcome, super::ManifestOutcome::Failed);

    let truncated = FakeRunner {
        truncate_evaluation: true,
        ..FakeRunner::default()
    };
    let manifest = build(&truncated, &[], limits());
    assert_eq!(
        manifest.diagnostics[0].code,
        "process_output_limit_exceeded"
    );
}

#[test]
fn rejects_root_and_evaluation_memory_limits_deterministically() {
    let mut root_limited = limits();
    root_limited.max_roots = 1;
    let runner = FakeRunner::default();
    let error = build_engine(&runner, root_limited)
        .build(BuildRequest {
            flake: flake(),
            targets: vec!["a".to_owned(), "b".to_owned()],
        })
        .expect_err("root limit");
    assert_eq!(error.code(), "root_limit_exceeded");

    let mut runner = FakeRunner::default();
    runner.evaluations.extend([
        (
            ("packages".to_owned(), "a".to_owned()),
            evaluation(DRV_A, OUT_A),
        ),
        (
            ("packages".to_owned(), "b".to_owned()),
            evaluation(DRV_B, OUT_B),
        ),
    ]);
    let manifest = build(&runner, &[], root_limited);
    assert_eq!(manifest.diagnostics[0].code, "root_limit_exceeded");
    assert_eq!(manifest.outcome, super::ManifestOutcome::Failed);
    assert_eq!(runner.calls("eval").len(), 1);
    assert_eq!(
        FakeRunner::env(&runner.calls("eval")[0], "NIX_TOOLS_ENGINE_MAX_ROOTS").as_deref(),
        Some("1")
    );
    assert!(runner.calls("derivation").is_empty());
    assert!(runner.calls("path-info").is_empty());
    assert!(runner.calls("build").is_empty());

    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    let mut memory_limited = limits();
    memory_limited.max_evaluation_memory_bytes = 1;
    let manifest = build(&runner, &["a"], memory_limited);
    assert_eq!(manifest.roots[0].state, NodeState::Failed);
    assert_eq!(
        manifest.diagnostics[0].code,
        "evaluation_memory_limit_exceeded"
    );
    assert!(runner.calls("derivation").is_empty());
}

fn build_engine(runner: &FakeRunner, limits: ResourceLimits) -> NixEngine<'_> {
    let cancellation = Box::leak(Box::new(Cancellation::default()));
    let clock = Box::leak(Box::new(FakeClock::with([100, 200])));
    let progress = Box::leak(Box::new(FakeProgress::default()));
    NixEngine::new(
        config(limits),
        EngineDependencies {
            runner,
            cancellation,
            clock,
            progress,
        },
    )
    .expect("engine")
}

#[test]
fn reports_truncated_process_output_without_parsing_it() {
    let mut runner = FakeRunner {
        truncate_evaluation: true,
        ..FakeRunner::default()
    };
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );

    let manifest = build(&runner, &["a"], limits());

    assert_eq!(manifest.roots[0].state, NodeState::Failed);
    assert_eq!(
        manifest.diagnostics[0].code,
        "process_output_limit_exceeded"
    );
}

#[test]
fn deduplicates_root_installables_and_maps_build_json_without_loading_a_graph() {
    let mut runner = FakeRunner::default();
    runner.evaluations.extend([
        (
            ("packages".to_owned(), "first".to_owned()),
            evaluation(DRV_C, OUT_C),
        ),
        (
            ("packages".to_owned(), "alias".to_owned()),
            evaluation(DRV_C, OUT_C),
        ),
    ]);
    runner.graph = graph([
        node(DRV_A, OUT_A, &[]),
        node(DRV_B, OUT_B, &[(DRV_A, &["out"])]),
        node(DRV_C, OUT_C, &[(DRV_B, &["out"])]),
    ]);

    let manifest = build(&runner, &["first", "alias"], limits());

    assert_eq!(*runner.builds.lock().expect("builds"), [DRV_C]);
    assert_eq!(runner.calls("eval").len(), 1);
    assert!(runner.calls("derivation").is_empty());
    assert_eq!(runner.calls("path-info").len(), 1);
    assert_eq!(runner.calls("build").len(), 1);
    assert_eq!(
        FakeRunner::stdin(&runner.calls("build")[0]),
        format!("{DRV_C}^out\n")
    );
    assert_eq!(manifest.nodes.len(), 1);
    assert_eq!(manifest.graph.len(), 1);
    assert!(manifest.graph[0].dependencies.is_empty());
    assert_eq!(manifest.nodes[0].produced_paths, [OUT_C]);
    assert_eq!(manifest.nodes[0].state, NodeState::Realized);
    assert_eq!(manifest.metrics.evaluation.processes, 1);
    assert_eq!(manifest.metrics.evaluation.duration_ms, 5);
    assert_eq!(manifest.metrics.probe.processes, 1);
    assert_eq!(manifest.metrics.probe.duration_ms, 5);
    assert_eq!(manifest.metrics.realization.processes, 1);
    assert_eq!(manifest.metrics.realization.duration_ms, 5);
    assert_eq!(manifest.metrics.graph.processes, 0);
    let probed = runner
        .calls("path-info")
        .into_iter()
        .flat_map(|spec| {
            FakeRunner::stdin(&spec)
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(probed, BTreeSet::from([OUT_C.to_owned()]));
    assert_eq!(manifest.roots[0].drv_path.as_deref(), Some(DRV_C));
    assert_eq!(manifest.roots[1].drv_path.as_deref(), Some(DRV_C));
}

#[test]
fn single_root_uses_detailed_remote_preflight_while_local_hits_stay_fast() {
    let mut local = FakeRunner::default();
    local.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    local.graph = graph([node(DRV_A, OUT_A, &[])]);
    local.local.insert(OUT_A.to_owned());

    let manifest = build(&local, &["a"], limits());

    assert!(local.builds.lock().expect("builds").is_empty());
    assert_eq!(manifest.nodes[0].state, NodeState::Cached);
    assert_eq!(manifest.availability[0].state, AvailabilityState::Local);
    assert_eq!(local.calls("eval").len(), 1);
    assert_eq!(local.calls("path-info").len(), 1);
    assert!(local.calls("derivation").is_empty());

    let mut remote = FakeRunner::default();
    remote.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    remote.graph = graph([node(DRV_A, OUT_A, &[])]);
    remote.remote.insert(
        "https://cache.example".to_owned(),
        BTreeSet::from([OUT_A.to_owned()]),
    );

    let cancellation = Cancellation::default();
    let clock = FakeClock::with([100, 200]);
    let progress = FakeProgress::default();
    let engine = NixEngine::new(
        config(limits()),
        EngineDependencies {
            runner: &remote,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");
    let manifest = engine
        .build(BuildRequest {
            flake: flake(),
            targets: vec!["a".to_owned()],
        })
        .expect("build");

    assert_eq!(*remote.builds.lock().expect("builds"), [DRV_A]);
    assert_eq!(manifest.nodes[0].state, NodeState::Substituted);
    assert_eq!(
        manifest.availability[0].state,
        AvailabilityState::TrustedRemote
    );
    assert_eq!(remote.calls("derivation").len(), 1);
    assert_eq!(remote.calls("path-info").len(), 2);
    assert_eq!(remote.calls("eval").len(), 1);
    assert_eq!(remote.calls("build").len(), 1);
    assert_eq!(manifest.metrics.evaluation.processes, 1);
    assert_eq!(manifest.metrics.graph.processes, 1);
    assert_eq!(manifest.metrics.probe.processes, 2);
    assert_eq!(manifest.metrics.realization.processes, 1);
    assert_eq!(probe_phase_events(&progress), ["started", "finished"]);
}

fn probe_phase_events(progress: &FakeProgress) -> Vec<&'static str> {
    progress
        .0
        .lock()
        .expect("progress")
        .iter()
        .filter_map(|event| match event {
            ProgressEvent::PhaseStarted(Phase::Probe) => Some("started"),
            ProgressEvent::PhaseFinished(Phase::Probe) => Some("finished"),
            _ => None,
        })
        .collect()
}

#[test]
fn failing_single_root_closes_the_one_probe_phase_before_realization() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);
    runner.build_failures.insert(DRV_A.to_owned());
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([100, 200]);
    let progress = FakeProgress::default();
    let engine = NixEngine::new(
        config(limits()),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");

    let manifest = engine
        .build(BuildRequest {
            flake: flake(),
            targets: vec!["a".to_owned()],
        })
        .expect("manifest");

    assert_eq!(manifest.roots[0].state, NodeState::Failed);
    assert_eq!(runner.calls("build").len(), 1);
    assert_eq!(probe_phase_events(&progress), ["started", "finished"]);
}

#[test]
fn cached_root_prunes_its_failing_build_inputs() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "root".to_owned()),
        evaluation(DRV_C, OUT_C),
    );
    runner.graph = graph([
        node(DRV_A, OUT_A, &[]),
        node(DRV_B, OUT_B, &[(DRV_A, &["out"])]),
        node(DRV_C, OUT_C, &[(DRV_B, &["out"])]),
    ]);
    runner.local.insert(OUT_C.to_owned());
    runner.build_failures.insert(DRV_B.to_owned());

    let manifest = build(&runner, &["root"], limits());

    assert!(runner.builds.lock().expect("builds").is_empty());
    assert!(runner.calls("derivation").is_empty());
    assert_eq!(runner.calls("eval").len(), 1);
    assert_eq!(runner.calls("path-info").len(), 1);
    assert_eq!(manifest.metrics.graph.processes, 0);
    assert_eq!(manifest.roots[0].state, NodeState::Cached);
    assert_eq!(
        manifest
            .nodes
            .iter()
            .map(|node| node.drv_path.as_str())
            .collect::<Vec<_>>(),
        [DRV_C]
    );
    assert!(
        manifest
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != "realization_failed")
    );
}

#[test]
fn local_root_shortcut_does_not_mask_an_evaluation_failure() {
    let mut runner = FakeRunner::default();
    runner.evaluations.extend([
        (
            ("packages".to_owned(), "good".to_owned()),
            evaluation(DRV_A, OUT_A),
        ),
        (
            ("packages".to_owned(), "bad".to_owned()),
            Evaluation::Failure,
        ),
    ]);
    runner.local.insert(OUT_A.to_owned());

    let manifest = build(&runner, &["good", "bad"], limits());

    assert_eq!(
        manifest
            .roots
            .iter()
            .find(|root| root.name == "good")
            .unwrap()
            .state,
        NodeState::Cached
    );
    assert_eq!(
        manifest
            .roots
            .iter()
            .find(|root| root.name == "bad")
            .unwrap()
            .state,
        NodeState::Failed
    );
    assert_eq!(manifest.outcome, super::ManifestOutcome::Failed);
    assert!(runner.calls("derivation").is_empty());
}

#[test]
fn cached_root_does_not_prune_an_independently_selected_dependency() {
    let mut runner = FakeRunner::default();
    runner.evaluations.extend([
        (
            ("packages".to_owned(), "root".to_owned()),
            evaluation(DRV_C, OUT_C),
        ),
        (
            ("packages".to_owned(), "dependency".to_owned()),
            evaluation(DRV_B, OUT_B),
        ),
    ]);
    runner.graph = graph([
        node(DRV_A, OUT_A, &[]),
        node(DRV_B, OUT_B, &[(DRV_A, &["out"])]),
        node(DRV_C, OUT_C, &[(DRV_B, &["out"])]),
    ]);
    runner.local.insert(OUT_C.to_owned());

    let manifest = build(&runner, &["root", "dependency"], limits());

    assert_eq!(*runner.builds.lock().expect("builds"), [DRV_B]);
    let local_probes = runner
        .calls("path-info")
        .into_iter()
        .filter(|spec| {
            !FakeRunner::args(spec)
                .iter()
                .any(|argument| argument == "--store")
        })
        .collect::<Vec<_>>();
    assert_eq!(local_probes.len(), 1);
    assert_eq!(
        FakeRunner::stdin(&local_probes[0])
            .lines()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([OUT_B, OUT_C])
    );
    assert_eq!(
        manifest
            .roots
            .iter()
            .find(|root| root.name == "root")
            .expect("root")
            .state,
        NodeState::Cached
    );
    assert_eq!(
        manifest
            .roots
            .iter()
            .find(|root| root.name == "dependency")
            .expect("dependency")
            .state,
        NodeState::Realized
    );
}

#[test]
fn remote_root_ignores_failure_of_independently_selected_pruned_dependency() {
    let mut runner = FakeRunner::default();
    runner.evaluations.extend([
        (
            ("packages".to_owned(), "root".to_owned()),
            evaluation(DRV_C, OUT_C),
        ),
        (
            ("packages".to_owned(), "dependency".to_owned()),
            evaluation(DRV_B, OUT_B),
        ),
    ]);
    runner.graph = graph([
        node(DRV_B, OUT_B, &[]),
        node(DRV_C, OUT_C, &[(DRV_B, &["out"])]),
    ]);
    runner.remote.insert(
        "https://cache.example".to_owned(),
        BTreeSet::from([OUT_C.to_owned()]),
    );
    runner.build_failures.insert(DRV_B.to_owned());

    let manifest = build(&runner, &["root", "dependency"], limits());

    assert_eq!(
        manifest
            .roots
            .iter()
            .find(|root| root.name == "root")
            .expect("root")
            .state,
        NodeState::Realized
    );
    assert_eq!(
        manifest
            .roots
            .iter()
            .find(|root| root.name == "dependency")
            .expect("dependency")
            .state,
        NodeState::Failed
    );
    assert_eq!(manifest.outcome, super::ManifestOutcome::Failed);
    let builds = runner.builds.lock().expect("builds");
    assert!(builds.iter().any(|path| path == DRV_B));
    assert!(builds.iter().any(|path| path == DRV_C));
}

#[test]
fn single_root_detailed_path_reports_remote_probe_degradation() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);
    runner.degraded.insert("https://cache.example".to_owned());

    let manifest = build(&runner, &["a"], limits());

    assert_eq!(manifest.nodes[0].state, NodeState::Built);
    assert!(
        manifest
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "cache_probe_failed")
    );
    assert_eq!(runner.calls("path-info").len(), 2);
    let build = &runner.calls("build")[0];
    let nix_config = build
        .env
        .get(OsStr::new("NIX_CONFIG"))
        .expect("NIX_CONFIG")
        .to_string_lossy();
    assert!(nix_config.contains("substituters = https://cache.example"));
    assert!(nix_config.contains("trusted-public-keys = cache.example-1:public-key"));
    assert!(nix_config.contains("fallback = false"));
    assert!(!nix_config.contains("cache.nixos.org"));
}

#[test]
fn continues_independent_work_after_partial_failure() {
    let mut runner = FakeRunner::default();
    runner.evaluations.extend([
        (
            ("packages".to_owned(), "a".to_owned()),
            evaluation(DRV_A, OUT_A),
        ),
        (
            ("packages".to_owned(), "b".to_owned()),
            evaluation(DRV_B, OUT_B),
        ),
    ]);
    runner.graph = graph([node(DRV_A, OUT_A, &[]), node(DRV_B, OUT_B, &[])]);
    runner.build_failures.insert(DRV_A.to_owned());

    let manifest = build(&runner, &["a", "b"], limits());

    assert_eq!(manifest.roots[0].state, NodeState::Failed);
    assert_eq!(manifest.roots[1].state, NodeState::Realized);
    assert_eq!(runner.calls("build").len(), 1);
    assert_eq!(runner.calls("derivation").len(), 1);
    assert_eq!(runner.calls("path-info").len(), 2);
    assert!(FakeRunner::args(&runner.calls("build")[0]).contains(&"--keep-going".to_owned()));
    assert_eq!(runner.builds.lock().expect("builds").len(), 2);
    assert_eq!(manifest.metrics.realization.processes, 2);
    assert_eq!(manifest.metrics.realization.duration_ms, 10);
    assert!(
        manifest
            .metrics
            .nodes
            .iter()
            .all(|node| node.duration_ms == 0)
    );
    assert_eq!(manifest.diagnostics[0].code, "realization_failed");
}

#[test]
fn marks_omitted_dependents_skipped_while_independent_roots_succeed() {
    let mut runner = FakeRunner::default();
    runner.evaluations.extend([
        (
            ("packages".to_owned(), "a".to_owned()),
            evaluation(DRV_A, OUT_A),
        ),
        (
            ("packages".to_owned(), "b".to_owned()),
            evaluation(DRV_B, OUT_B),
        ),
        (
            ("packages".to_owned(), "c".to_owned()),
            evaluation(DRV_C, OUT_C),
        ),
    ]);
    runner.graph = graph([
        node(DRV_A, OUT_A, &[]),
        node(DRV_B, OUT_B, &[(DRV_A, &["out"])]),
        node(DRV_C, OUT_C, &[]),
    ]);
    runner
        .build_failures
        .extend([DRV_A.to_owned(), DRV_B.to_owned()]);

    let cancellation = Cancellation::default();
    let clock = FakeClock::with([100, 200]);
    let progress = FakeProgress::default();
    let engine = NixEngine::new(
        config(limits()),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");
    let manifest = engine
        .build(BuildRequest {
            flake: flake(),
            targets: ["a", "b", "c"].into_iter().map(str::to_owned).collect(),
        })
        .expect("build");

    assert_eq!(manifest.roots[0].state, NodeState::Failed);
    assert_eq!(manifest.roots[1].state, NodeState::Skipped);
    assert_eq!(manifest.roots[2].state, NodeState::Realized);
    let dependent = manifest
        .nodes
        .iter()
        .find(|node| node.drv_path == DRV_B)
        .expect("dependent");
    assert_eq!(
        dependent
            .dependency_failure
            .as_ref()
            .map(|failure| failure.dependency.as_str()),
        Some(DRV_A)
    );
    let events = progress.0.lock().expect("progress");
    let graphs = events
        .iter()
        .filter_map(|event| match event {
            ProgressEvent::GraphDiscovered(nodes) => Some(nodes),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(graphs.len(), 2);
    assert!(
        graphs[1]
            .iter()
            .find(|node| node.drv_path == DRV_B)
            .is_some_and(|node| node.dependencies.contains_key(DRV_A))
    );
}

#[test]
fn failure_fallback_rejects_mismatched_root_identity() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.evaluations.insert(
        ("packages".to_owned(), "b".to_owned()),
        evaluation(DRV_B, OUT_B),
    );
    runner.graph = graph([node(DRV_A, OUT_B, &[]), node(DRV_B, OUT_B, &[])]);
    runner.build_failures.insert(DRV_A.to_owned());

    let manifest = build(&runner, &["a", "b"], limits());

    assert!(
        manifest
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "root_output_identity_mismatch")
    );
    assert_eq!(manifest.graph[0].outputs["out"].as_deref(), Some(OUT_A));
    assert_eq!(runner.calls("build").len(), 1);
}

#[test]
#[cfg(feature = "nix-integration")]
fn real_nix_keep_going_preserves_an_independent_success_after_failure() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("nix-tools-engine-{nonce}"));
    fs::create_dir(&directory).expect("temporary flake directory");
    let bash = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|directory| directory.join("bash"))
        .find(|candidate| candidate.is_file())
        .and_then(|path| fs::canonicalize(path).ok())
        .expect("bash in PATH");
    fs::write(
        directory.join("flake.nix"),
        format!(
            r#"{{
  inputs = {{}};
  outputs = {{ self }}: {{
    packages.{system}.succeed = let drv = builtins.derivation {{
      name = "nix-tools-succeed-{nonce}";
      system = "{system}";
      builder = builtins.storePath "{bash}";
      args = [ "-c" "echo success > $out" ];
    }}; in drv // {{ outputs = [ "out" ]; out = drv; meta.outputsToInstall = [ "out" ]; }};
    packages.{system}.fail = let drv = builtins.derivation {{
      name = "nix-tools-fail-{nonce}";
      system = "{system}";
      builder = builtins.storePath "{bash}";
      args = [ "-c" "exit 1" ];
    }}; in drv // {{ outputs = [ "out" ]; out = drv; meta.outputsToInstall = [ "out" ]; }};
  }};
}}"#,
            system = NixSystem::host().expect("host system"),
            bash = bash.display(),
        ),
    )
    .expect("flake");
    let runner = RecordingRunner {
        inner: StdProcessRunner::new(Duration::from_millis(10), Redactor::default()),
        builds: Mutex::new(Vec::new()),
    };
    let cancellation = Cancellation::default();
    let clock = SystemClock;
    let progress = FakeProgress::default();
    let engine = NixEngine::new(
        EngineConfig::new("nix", NixSystem::host().expect("host system")),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");

    let manifest = engine
        .build(BuildRequest {
            flake: FlakeRef::new(".", Some(directory.clone())),
            targets: vec!["fail".to_owned(), "succeed".to_owned()],
        })
        .expect("structured manifest");

    fs::remove_dir_all(directory).expect("remove temporary flake");
    let builds = runner.builds.lock().expect("recorded builds");
    assert_eq!(builds.len(), 1);
    assert!(builds[0].stdout.bytes.is_empty());
    assert_eq!(manifest.roots[0].state, NodeState::Failed);
    assert_eq!(manifest.roots[1].state, NodeState::Realized);
    assert_eq!(manifest.metrics.realization.processes, 2);
}

#[test]
fn evaluates_app_string_context_and_realizes_owner_before_preparing_exec() {
    let arguments = vec![
        OsString::from("--literal"),
        OsString::from("argument with spaces"),
    ];
    let mut runner = FakeRunner {
        app_program: format!("{OUT_A}/bin/app"),
        app_context: json!({DRV_A: {"outputs": ["out"]}}),
        ..FakeRunner::default()
    };
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([100, 200]);
    let progress = FakeProgress::default();
    let engine = NixEngine::new(
        config(limits()),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");

    let PreparedRun {
        program,
        arguments: prepared_arguments,
        manifest,
    } = engine
        .prepare_run(RunRequest {
            flake: flake(),
            app: "default".to_owned(),
            arguments: arguments.clone(),
        })
        .expect("prepare run");

    assert_eq!(program, format!("{OUT_A}/bin/app"));
    assert_eq!(prepared_arguments, arguments);
    assert_eq!(manifest.nodes[0].state, NodeState::Built);
    let app_eval = &runner.calls("eval")[0];
    let expression = FakeRunner::args(app_eval).join(" ");
    assert!(expression.contains("builtins.getContext"));
    assert!(expression.contains("unsafeDiscardStringContext"));
    assert!(
        runner
            .calls("build")
            .iter()
            .any(|spec| FakeRunner::stdin(spec).starts_with(DRV_A))
    );
}

#[test]
fn prepare_run_preserves_failed_realization_manifest() {
    let mut runner = FakeRunner {
        app_program: format!("{OUT_A}/bin/app"),
        app_context: json!({DRV_A: {"outputs": ["out"]}}),
        ..FakeRunner::default()
    };
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);
    runner.build_failures.insert(DRV_A.to_owned());

    let prepared = prepare_run(&runner).expect("prepared failed run");

    assert_eq!(prepared.program, format!("{OUT_A}/bin/app"));
    assert_eq!(prepared.manifest.outcome, super::ManifestOutcome::Failed);
    assert_eq!(prepared.manifest.nodes[0].state, NodeState::Failed);
    assert_eq!(prepared.manifest.diagnostics[0].code, "realization_failed");
}

#[test]
fn prepare_run_preserves_cancelled_realization_manifest() {
    let mut runner = FakeRunner {
        app_program: format!("{OUT_A}/bin/app"),
        app_context: json!({DRV_A: {"outputs": ["out"]}}),
        cancel_build: Some(DRV_A.to_owned()),
        ..FakeRunner::default()
    };
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);

    let prepared = prepare_run(&runner).expect("prepared cancelled run");

    assert_eq!(prepared.manifest.outcome, super::ManifestOutcome::Cancelled);
    assert_eq!(prepared.manifest.nodes[0].state, NodeState::Cancelled);
    assert_eq!(prepared.manifest.diagnostics[0].code, "cancelled");
}

fn prepare_run(runner: &FakeRunner) -> std::result::Result<PreparedRun, super::EngineError> {
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([100, 200]);
    let progress = FakeProgress::default();
    NixEngine::new(
        config(limits()),
        EngineDependencies {
            runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )?
    .prepare_run(RunRequest {
        flake: flake(),
        app: "default".to_owned(),
        arguments: Vec::new(),
    })
}

#[test]
fn check_requests_use_the_standard_checks_namespace() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("checks".to_owned(), "test".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([100, 200]);
    let progress = FakeProgress::default();
    let engine = NixEngine::new(
        config(limits()),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");

    let manifest = engine
        .check(CheckRequest {
            flake: flake(),
            targets: vec!["test".to_owned()],
        })
        .expect("check");

    assert_eq!(manifest.roots[0].kind.as_str(), "check");
    assert_eq!(manifest.nodes[0].state, NodeState::Built);
}

#[test]
fn cancellation_before_dispatch_starts_no_processes() {
    let runner = FakeRunner::default();
    let cancellation = Cancellation::default();
    cancellation.request(2);
    let clock = FakeClock::with([]);
    let progress = FakeProgress::default();
    let engine = NixEngine::new(
        config(limits()),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");

    let error = engine
        .build(BuildRequest {
            flake: flake(),
            targets: vec!["a".to_owned()],
        })
        .expect_err("cancelled");

    assert_eq!(error.code(), "cancelled");
    assert!(runner.calls.lock().expect("calls").is_empty());
    assert!(
        progress
            .0
            .lock()
            .expect("progress")
            .contains(&ProgressEvent::Cancelled { signal: 2 })
    );
}

#[test]
fn progress_finishes_each_started_phase() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.evaluations.insert(
        ("packages".to_owned(), "b".to_owned()),
        evaluation(DRV_B, OUT_B),
    );
    runner.graph = graph([node(DRV_A, OUT_A, &[]), node(DRV_B, OUT_B, &[])]);
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([100, 200]);
    let progress = FakeProgress::default();
    let engine = NixEngine::new(
        config(limits()),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine");

    let manifest = engine
        .build(BuildRequest {
            flake: flake(),
            targets: vec!["a".to_owned(), "b".to_owned()],
        })
        .expect("build");

    let events = progress.0.lock().expect("progress");
    for phase in [Phase::Evaluation, Phase::Probe, Phase::Realization] {
        assert!(events.contains(&ProgressEvent::PhaseStarted(phase)));
        assert!(events.contains(&ProgressEvent::PhaseFinished(phase)));
    }
    assert!(!events.contains(&ProgressEvent::PhaseStarted(Phase::Graph)));
    let probe_events = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                ProgressEvent::PhaseStarted(Phase::Probe)
                    | ProgressEvent::PhaseFinished(Phase::Probe)
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        probe_events,
        [
            &ProgressEvent::PhaseStarted(Phase::Probe),
            &ProgressEvent::PhaseFinished(Phase::Probe),
        ]
    );
    let finished = events
        .iter()
        .filter_map(|event| match event {
            ProgressEvent::NodeFinished { drv_path, state } => Some((drv_path, state)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    for node in &manifest.nodes {
        assert_eq!(finished.get(&node.drv_path), Some(&&node.state));
    }
}
