use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use nix_tools_core::outcome::{Error, Result};
use nix_tools_core::process::{
    Cancellation, CapturedStream, ChildTermination, ProcessResult, ProcessRunner, ProcessSpec,
};
use nix_tools_core::system::NixSystem;
use serde_json::{Value, json};

use super::{
    AvailabilityState, BuildRequest, CheckRequest, Clock, DiagnosticSeverity, DiscoverRequest,
    EngineConfig, EngineDependencies, FlakeRef, NixEngine, NodeState, Phase, PreparedRun,
    ProgressEvent, ProgressSink, ResourceLimits, RunRequest, TrustedSubstituter,
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
}

struct FakeRunner {
    discovered: Value,
    discovery_failure: Option<(i32, Vec<u8>)>,
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

impl Default for FakeRunner {
    fn default() -> Self {
        Self {
            discovered: json!({"packages": [], "checks": [], "apps": []}),
            discovery_failure: None,
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
        let targets: Vec<Value> = serde_json::from_str(
            &Self::env(spec, "NIX_TOOLS_ENGINE_TARGETS").expect("target JSON environment"),
        )
        .expect("target JSON");
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
                }
            })
            .collect::<Vec<_>>();
        let mut result = process(0, &json!(attempts));
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
        let installable = Self::stdin(spec).trim().to_owned();
        let drv_path = installable
            .split_once('^')
            .map_or(installable.as_str(), |(drv, _)| drv)
            .to_owned();
        self.builds.lock().expect("builds").push(drv_path.clone());
        if self.build_failures.contains(&drv_path) {
            return process_with_code(42, b"builder failed");
        }
        let output = graph_output(&self.graph, &drv_path);
        process(
            0,
            &json!([{"drvPath": drv_path, "outputs": {"out": output}}]),
        )
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
                    .contains_key(OsStr::new("NIX_TOOLS_ENGINE_TARGETS")) =>
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
                let drv_path = Self::stdin(spec)
                    .trim()
                    .split_once('^')
                    .map_or_else(|| Self::stdin(spec), |(drv_path, _)| drv_path.to_owned());
                if self.cancel_build.as_deref() == Some(&drv_path) {
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
        realization_concurrency: 2,
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
fn deduplicates_derivations_and_realizes_dependencies_first_once() {
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

    assert_eq!(
        *runner.builds.lock().expect("builds"),
        [DRV_A, DRV_B, DRV_C]
    );
    assert_eq!(manifest.nodes.len(), 3);
    assert_eq!(manifest.roots[0].drv_path.as_deref(), Some(DRV_C));
    assert_eq!(manifest.roots[1].drv_path.as_deref(), Some(DRV_C));
}

#[test]
fn skips_local_hits_and_substitutes_only_remote_hits() {
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

    let manifest = build(&remote, &["a"], limits());

    assert_eq!(*remote.builds.lock().expect("builds"), [DRV_A]);
    assert_eq!(manifest.nodes[0].state, NodeState::Substituted);
    assert_eq!(
        manifest.availability[0].state,
        AvailabilityState::TrustedRemote
    );
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

    assert_eq!(*runner.builds.lock().expect("builds"), [DRV_A, DRV_B]);
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
        NodeState::Built
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
        NodeState::Substituted
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
fn degrades_failed_cache_probe_and_builds_with_only_configured_trust() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);
    runner.degraded.insert("https://cache.example".to_owned());

    let manifest = build(&runner, &["a"], limits());

    assert_eq!(manifest.nodes[0].state, NodeState::Built);
    let degradation = manifest
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "cache_probe_failed")
        .expect("degradation");
    assert_eq!(degradation.severity, DiagnosticSeverity::Warning);
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
    assert_eq!(manifest.roots[1].state, NodeState::Built);
    assert_eq!(runner.builds.lock().expect("builds").len(), 2);
    assert_eq!(manifest.diagnostics[0].code, "realization_failed");
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

    engine
        .build(BuildRequest {
            flake: flake(),
            targets: vec!["a".to_owned()],
        })
        .expect("build");

    let events = progress.0.lock().expect("progress");
    for phase in [
        Phase::Evaluation,
        Phase::Graph,
        Phase::Probe,
        Phase::Realization,
    ] {
        assert!(events.contains(&ProgressEvent::PhaseStarted(phase)));
        assert!(events.contains(&ProgressEvent::PhaseFinished(phase)));
    }
}
