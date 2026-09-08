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
    StreamPolicy,
};
#[cfg(feature = "nix-integration")]
use nix_tools_core::redaction::Redactor;
use nix_tools_core::system::NixSystem;
use serde_json::{Value, json};

#[cfg(feature = "nix-integration")]
use super::SystemClock;
use super::{
    AvailabilityState, BuildRequest, CheckRequest, Clock, DiscoverRequest, EngineConfig,
    EngineDependencies, FlakeRef, GraphMode, ManifestOutcome, NixEngine, NodeState, Phase,
    PreparedRun, ProgressEvent, ProgressSink, ResourceLimits, RunRequest, TrustedSubstituter,
};

const DRV_A: &str = "/nix/store/00000000000000000000000000000000-a.drv";
const DRV_B: &str = "/nix/store/11111111111111111111111111111111-b.drv";
const DRV_C: &str = "/nix/store/22222222222222222222222222222222-c.drv";
const OUT_A: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a";
const OUT_B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b";
const OUT_C: &str = "/nix/store/cccccccccccccccccccccccccccccccc-c";
/// The message nix prints last when a builder fails, and the one a diagnostic must keep.
const BUILD_ERROR: &str = "error: builder for a.drv failed with exit code 42";
/// What the raw stderr capture holds once the JSON log format is selected: no plain text at all.
const BUILD_ENVELOPE: &[u8] =
    br#"@nix {"action":"msg","level":0,"msg":"unreconstructed envelope"}"#;
const BUILD_PANIC: &str = "fake realization run panicked";

/// How one realization process misbehaves beyond the failures its derivations already carry.
#[derive(Clone, Copy, Eq, PartialEq)]
enum BuildQuirk {
    None,
    /// The raw JSON capture hit its bound even though the rebuilt log is complete.
    TruncatedCapture,
    /// The runner panics, as a builder that aborts the process would.
    Panic,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CleanupProbe {
    Normal,
    Partial,
    Stall,
    Malformed,
}

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
    local_after_build: BTreeSet<String>,
    truncate_local_after_build: bool,
    cleanup_probe: CleanupProbe,
    remote: BTreeMap<String, BTreeSet<String>>,
    degraded: BTreeSet<String>,
    build_failures: BTreeSet<String>,
    build_log_lines: usize,
    build_quirk: BuildQuirk,
    out_link_failure: bool,
    cancel_build: Option<String>,
    confirmed_results: Vec<Value>,
    stopped_builds: Vec<String>,
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
            local_after_build: BTreeSet::new(),
            truncate_local_after_build: false,
            cleanup_probe: CleanupProbe::Normal,
            remote: BTreeMap::new(),
            degraded: BTreeSet::new(),
            build_failures: BTreeSet::new(),
            build_log_lines: 0,
            build_quirk: BuildQuirk::None,
            out_link_failure: false,
            cancel_build: None,
            confirmed_results: Vec::new(),
            stopped_builds: Vec::new(),
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
        let built = !self.builds.lock().expect("builds").is_empty();
        let entries = requested
            .iter()
            .map(|path| {
                let metadata = (available.contains(path)
                    || (store.is_none() && built && self.local_after_build.contains(path)))
                .then(|| json!({"path": path, "narSize": 10}));
                (path.clone(), metadata.unwrap_or(Value::Null))
            })
            .collect::<serde_json::Map<_, _>>();
        let mut result = process(0, &Value::Object(entries));
        if store.is_none() && built && self.truncate_local_after_build {
            result.stdout.truncated = true;
        }
        if store.is_none() && built && self.cleanup_probe == CleanupProbe::Partial {
            result.termination = ChildTermination::Exited(1);
        }
        if store.is_none() && built && self.cleanup_probe == CleanupProbe::Malformed {
            result.stdout.bytes = b"{".to_vec();
        }
        result
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
        assert!(self.build_quirk != BuildQuirk::Panic, "{BUILD_PANIC}");
        if let StreamPolicy::Observe { observer, .. } = &spec.stderr {
            for (index, drv_path) in drv_paths.iter().enumerate() {
                let id = u64::try_from(index).expect("activity identifier") + 1;
                observer.line(
                    format!(
                        r#"@nix {{"action":"start","id":{id},"type":105,"fields":["{drv_path}","x86_64-linux","",1]}}"#
                    )
                    .as_bytes(),
                );
                for line in 0..self.build_log_lines {
                    observer.line(
                        format!(
                            r#"@nix {{"action":"result","id":{id},"type":101,"fields":["configure: checking chatter {line} {}"]}}"#,
                            "x".repeat(64)
                        )
                        .as_bytes(),
                    );
                }
            }
            for payload in &self.confirmed_results {
                observer.line(
                    format!(
                        "@nix {}",
                        json!({"action":"result","type":110,"payload":payload})
                    )
                    .as_bytes(),
                );
            }
            if drv_paths
                .iter()
                .any(|drv_path| self.build_failures.contains(drv_path))
            {
                observer.line(
                    format!(r#"@nix {{"action":"msg","level":0,"msg":"{BUILD_ERROR}"}}"#)
                        .as_bytes(),
                );
            }
        }
        let entries = drv_paths
            .iter()
            .filter(|drv_path| !self.build_failures.contains(*drv_path))
            .map(|drv_path| {
                let output = graph_output(&self.graph, drv_path);
                json!({"drvPath": drv_path, "outputs": {"out": output}})
            })
            .collect::<Vec<_>>();
        let mut result = process(0, &json!(entries));
        if self.out_link_failure {
            result.termination = ChildTermination::Exited(1);
            result.stderr.bytes = b"cannot create result symlink".to_vec();
        }
        if drv_paths
            .iter()
            .any(|drv_path| self.build_failures.contains(drv_path))
        {
            result.termination = ChildTermination::Exited(42);
            result.stderr.bytes = BUILD_ENVELOPE.to_vec();
            result.stderr.truncated = self.build_quirk == BuildQuirk::TruncatedCapture;
        }
        result
    }

    fn derivation_graph(&self, spec: &ProcessSpec) -> ProcessResult {
        let bytes = serde_json::to_vec(&self.graph).expect("graph JSON");
        if let StreamPolicy::Consume { consumer, .. } = &spec.stdout {
            consumer
                .consume(&mut bytes.as_slice())
                .expect("consume graph");
        } else {
            return process(0, &self.graph);
        }
        process_with_code(0, b"")
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
            Some("derivation") => Ok(self.derivation_graph(spec)),
            Some("path-info") => {
                if self.cleanup_probe == CleanupProbe::Stall
                    && !self.builds.lock().expect("builds").is_empty()
                {
                    while cancellation.signal().is_none() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    return Err(Error::cancelled(2, "cleanup deadline"));
                }
                Ok(self.path_info(&args, spec))
            }
            Some("build") => {
                let should_cancel = Self::stdin(spec).lines().any(|installable| {
                    let drv_path = installable
                        .split_once('^')
                        .map_or(installable, |(drv_path, _)| drv_path);
                    self.cancel_build.as_deref() == Some(drv_path)
                });
                if should_cancel {
                    self.builds
                        .lock()
                        .expect("builds")
                        .push(self.cancel_build.clone().expect("cancelled build"));
                    if let StreamPolicy::Observe { observer, .. } = &spec.stderr {
                        for drv in &self.stopped_builds {
                            observer.line(format!(r#"@nix {{"action":"start","id":1,"type":105,"fields":["{drv}"]}}"#).as_bytes());
                            observer.line(br#"@nix {"action":"stop","id":1}"#);
                        }
                        for payload in &self.confirmed_results {
                            observer.line(
                                format!(
                                    "@nix {}",
                                    json!({"action": "result", "type": 110, "payload": payload})
                                )
                                .as_bytes(),
                            );
                        }
                    }
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

struct LiveProgress(std::sync::mpsc::Sender<ProgressEvent>);

impl ProgressSink for LiveProgress {
    fn emit(&self, event: ProgressEvent) {
        let _ = self.0.send(event);
    }
}

struct LiveRunner {
    inner: FakeRunner,
    events: Mutex<std::sync::mpsc::Receiver<ProgressEvent>>,
    observed: Mutex<Vec<ProgressEvent>>,
    expected: usize,
    retry: bool,
    succeed: bool,
    probes: Mutex<Vec<std::time::Instant>>,
}

impl ProcessRunner for LiveRunner {
    fn run(&self, spec: &ProcessSpec, cancellation: &Cancellation) -> Result<ProcessResult> {
        let args = FakeRunner::args(spec);
        if args.first().is_some_and(|arg| arg == "build") {
            self.inner
                .builds
                .lock()
                .expect("builds")
                .push(DRV_A.to_owned());
            if let StreamPolicy::Observe { observer, .. } = &spec.stderr {
                for (id, drv) in self.inner.stopped_builds.iter().enumerate() {
                    for _ in 0..2 {
                        observer.line(
                            format!(
                                "@nix {}",
                                json!({"action":"start", "id":id + 1, "type":105, "fields":[drv]})
                            )
                            .as_bytes(),
                        );
                        observer.line(
                            format!("@nix {}", json!({"action":"stop", "id":id + 1})).as_bytes(),
                        );
                    }
                }
                observer.line(br#"@nix {"action":"result","id":1,"type":101,"fields":["pipe still flowing"]}"#);
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let mut finished = BTreeSet::new();
            let mut sent_during_probe = false;
            while std::time::Instant::now() < deadline {
                if let Ok(event) = self
                    .events
                    .lock()
                    .expect("events")
                    .recv_timeout(Duration::from_millis(20))
                {
                    if let ProgressEvent::NodeFinished {
                        drv_path,
                        state: NodeState::Realized,
                    } = &event
                    {
                        finished.insert(drv_path.clone());
                    }
                    self.observed.lock().expect("observed").push(event);
                }
                if !sent_during_probe && !self.probes.lock().expect("probes").is_empty() {
                    if let StreamPolicy::Observe { observer, .. } = &spec.stderr {
                        observer.line(br#"@nix {"action":"result","id":1,"type":101,"fields":["log during probe"]}"#);
                    }
                    sent_during_probe = true;
                }
                if (self.expected > 0 && finished.len() == self.expected)
                    || (self.expected == 0 && self.probes.lock().expect("probes").len() >= 3)
                {
                    break;
                }
            }
            if self.succeed {
                if let StreamPolicy::Observe { observer, .. } = &spec.stderr {
                    for payload in &self.inner.confirmed_results {
                        observer.line(
                            format!(
                                "@nix {}",
                                json!({"action":"result","type":110,"payload":payload})
                            )
                            .as_bytes(),
                        );
                    }
                }
                return Ok(process(
                    0,
                    &json!([
                        {"drvPath": DRV_A, "outputs": {"out": OUT_A}},
                        {"drvPath": DRV_B, "outputs": {"out": OUT_B}}
                    ]),
                ));
            }
            cancellation.request(2);
            return Err(Error::cancelled(2, "live build cancelled"));
        }
        if args.first().is_some_and(|arg| arg == "path-info")
            && !self.inner.builds.lock().expect("builds").is_empty()
        {
            let mut probes = self.probes.lock().expect("probes");
            probes.push(std::time::Instant::now());
            if self.retry && probes.len() == 1 {
                return Ok(process(0, &json!({})));
            }
        }
        self.inner.run(spec, cancellation)
    }
}

fn live_build(
    inner: FakeRunner,
    expected: usize,
    retry: bool,
    succeed: bool,
) -> (LiveRunner, super::Manifest) {
    live_build_mode(inner, expected, retry, succeed, GraphMode::Automatic)
}

fn live_build_mode(
    inner: FakeRunner,
    expected: usize,
    retry: bool,
    succeed: bool,
    graph_mode: GraphMode,
) -> (LiveRunner, super::Manifest) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let runner = LiveRunner {
        inner,
        events: Mutex::new(receiver),
        observed: Mutex::new(Vec::new()),
        expected,
        retry,
        succeed,
        probes: Mutex::new(Vec::new()),
    };
    let cancellation = Cancellation::default();
    let clock = FakeClock::default();
    let progress = LiveProgress(sender);
    let mut bounds = limits();
    bounds.max_graph_nodes = 1024;
    let mut engine_config = config(bounds);
    engine_config.graph_mode = graph_mode;
    let manifest = NixEngine::new(
        engine_config,
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .expect("engine")
    .build(BuildRequest {
        flake: flake(),
        targets: vec!["a".to_owned(), "b".to_owned()],
        out_link: None,
    })
    .expect("manifest");
    (runner, manifest)
}

fn live_fixture() -> FakeRunner {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.evaluations.insert(
        ("packages".to_owned(), "b".to_owned()),
        evaluation(DRV_B, OUT_B),
    );
    runner.graph = graph([
        node(DRV_A, OUT_A, &[]),
        node(DRV_B, OUT_B, &[]),
        node(DRV_C, OUT_C, &[]),
    ]);
    runner
}

#[test]
fn live_confirmation_precedes_runner_return_and_retries_registration() {
    for drv in [DRV_A, DRV_C] {
        let mut inner = live_fixture();
        inner.stopped_builds.push(drv.to_owned());
        inner
            .local_after_build
            .insert(if drv == DRV_A { OUT_A } else { OUT_C }.to_owned());
        let (runner, manifest) = live_build(inner, 1, true, false);
        let events = runner.observed.lock().expect("events");
        assert!(events.iter().any(|event| matches!(event, ProgressEvent::NodeFinished { drv_path, state: NodeState::Realized } if drv_path == drv)), "confirmed while runner is still active");
        assert!(events.iter().any(|event| matches!(event, ProgressEvent::NodeLogLine { line, .. } if line == "pipe still flowing")));
        assert_eq!(
            manifest
                .nodes
                .iter()
                .find(|node| node.drv_path == drv)
                .expect("retained result")
                .state,
            NodeState::Realized
        );
        let probes = runner.probes.lock().expect("probes");
        assert!(probes[1].duration_since(probes[0]) >= Duration::from_millis(75));
        for spec in runner
            .inner
            .calls("path-info")
            .iter()
            .chain(runner.inner.calls("derivation").iter())
        {
            assert!(FakeRunner::args(spec).contains(&"--offline".to_owned()));
        }
    }
}

#[test]
fn live_confirmation_requires_every_output_and_probe_failures_are_advisory() {
    for quirk in [
        CleanupProbe::Normal,
        CleanupProbe::Malformed,
        CleanupProbe::Stall,
    ] {
        let mut inner = live_fixture();
        inner.stopped_builds.push(DRV_C.to_owned());
        inner.graph[DRV_C]["outputs"]["dev"] = json!({"path": OUT_B});
        inner.local_after_build.insert(OUT_C.to_owned());
        inner.cleanup_probe = quirk;
        let started = std::time::Instant::now();
        let (runner, manifest) = live_build(inner, 0, false, false);
        assert!(started.elapsed() < Duration::from_secs(4));
        let events = runner.observed.lock().expect("events");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProgressEvent::NodeProvisionalFinished { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProgressEvent::NodeLogLine { .. }))
        );
        assert!(events.iter().any(|event| matches!(event, ProgressEvent::NodeLogLine { line, .. } if line == "log during probe")));
        assert!(!events.iter().any(|event| matches!(
            event,
            ProgressEvent::NodeFinished {
                state: NodeState::Realized,
                ..
            }
        )));
        assert_eq!(manifest.outcome, ManifestOutcome::Cancelled);
        let probes = runner.probes.lock().expect("probes");
        assert!(
            probes.len() >= 2,
            "live probes ran before cancellation cleanup"
        );
        assert!(probes.len() <= 6, "bounded retry rate");
    }
}

#[test]
fn live_confirmation_batches_large_job_sets_without_starvation() {
    for missing in [0, 128] {
        let mut inner = live_fixture();
        for index in 0..300 {
            let drv = format!("/nix/store/00000000000000000000000000000000-job-{index:04}.drv");
            let output = format!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-job-{index:04}");
            inner.graph[&drv] = node(&drv, &output, &[]).1;
            inner.stopped_builds.push(drv);
            if index >= missing {
                inner.local_after_build.insert(output);
            }
        }
        let expected = 300 - missing;
        let (runner, manifest) = live_build(inner, expected, false, false);
        let events = runner.observed.lock().expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    ProgressEvent::NodeFinished {
                        state: NodeState::Realized,
                        ..
                    }
                ))
                .count(),
            expected
        );
        assert_eq!(
            manifest
                .nodes
                .iter()
                .filter(|node| node.state == NodeState::Realized)
                .count(),
            expected
        );
        assert!(runner.probes.lock().expect("probes").len() <= 5);
        let metadata = runner.inner.calls("derivation");
        assert!(metadata.len() <= 4);
        assert!(
            metadata
                .iter()
                .all(|spec| FakeRunner::stdin(spec).lines().count() <= 128)
        );
        let covered = metadata
            .iter()
            .flat_map(|spec| {
                FakeRunner::stdin(spec)
                    .lines()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(covered.len(), 300);
    }
}

#[test]
fn live_confirmation_process_end_interrupts_stalled_probe() {
    let mut inner = live_fixture();
    inner.stopped_builds.push(DRV_A.to_owned());
    inner.cleanup_probe = CleanupProbe::Stall;
    let started = std::time::Instant::now();
    let (runner, manifest) = live_build(inner, 0, false, true);
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "process exit interrupts the active probe deadline"
    );
    assert_eq!(manifest.outcome, ManifestOutcome::Success);
    let events = runner.observed.lock().expect("events");
    assert!(events.iter().any(|event| matches!(event, ProgressEvent::NodeLogLine { line, .. } if line == "log during probe")));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProgressEvent::NodeFinished { .. }))
    );
}

#[test]
fn live_confirmation_finalization_preserves_authoritative_dispositions() {
    for (status, state) in [
        ("AlreadyValid", NodeState::Cached),
        ("Substituted", NodeState::Substituted),
        ("ResolvesToAlreadyValid", NodeState::Realized),
    ] {
        let mut inner = live_fixture();
        inner.stopped_builds.push(DRV_A.to_owned());
        inner.local_after_build.insert(OUT_A.to_owned());
        inner.confirmed_results.push(json!({"success":true,"status":status,"path":{"drvPath":DRV_A,"outputs":["out"]},"builtOutputs":{"out":{"outPath":OUT_A}}}));
        let (_, manifest) = live_build_mode(inner, 1, false, true, GraphMode::Complete);
        assert_eq!(
            manifest
                .nodes
                .iter()
                .find(|node| node.drv_path == DRV_A)
                .expect("root result")
                .state,
            state
        );
    }
}

#[test]
fn live_confirmation_retains_transitive_results_on_success() {
    let mut inner = live_fixture();
    inner.stopped_builds = vec![DRV_A.to_owned(), DRV_C.to_owned()];
    inner.local_after_build = BTreeSet::from([OUT_A.to_owned(), OUT_C.to_owned()]);
    inner.confirmed_results.push(json!({"success":true,"status":"Built","path":{"drvPath":DRV_A,"outputs":["out"]},"builtOutputs":{"out":{"outPath":OUT_A}}}));
    let (runner, manifest) = live_build(inner, 2, false, true);
    assert_eq!(manifest.outcome, ManifestOutcome::Success);
    assert_eq!(
        manifest
            .nodes
            .iter()
            .find(|node| node.drv_path == DRV_C)
            .expect("transitive result")
            .state,
        NodeState::Realized
    );
    assert_eq!(
        manifest
            .nodes
            .iter()
            .find(|node| node.drv_path == DRV_A)
            .expect("root result")
            .state,
        NodeState::Built
    );
    assert_eq!(
        runner
            .observed
            .lock()
            .expect("events")
            .iter()
            .filter(|event| matches!(
                event,
                ProgressEvent::NodeFinished {
                    state: NodeState::Realized,
                    ..
                }
            ))
            .count(),
        2
    );
}

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
        max_graph_retained_bytes: 1024 * 1024,
        max_graph_stream_bytes: 8 * 1024 * 1024,
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
        graph_mode: GraphMode::Automatic,
        limits,
    }
}

fn build_with_graph_mode(
    runner: &FakeRunner,
    names: &[&str],
    limits: ResourceLimits,
    graph_mode: GraphMode,
) -> std::result::Result<super::Manifest, super::EngineError> {
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([100, 200]);
    let progress = FakeProgress::default();
    let mut engine_config = config(limits);
    engine_config.graph_mode = graph_mode;
    NixEngine::new(
        engine_config,
        EngineDependencies {
            runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )?
    .build(BuildRequest {
        flake: flake(),
        targets: names.iter().map(|name| (*name).to_owned()).collect(),
        out_link: None,
    })
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
            out_link: None,
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
    runner.local.insert(OUT_A.to_owned());

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
            out_link: None,
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
            out_link: None,
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

#[test]
fn build_uses_the_requested_out_link_instead_of_no_link() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);
    runner.local.insert(OUT_A.to_owned());
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
            out_link: Some(PathBuf::from("result")),
        })
        .expect("build");

    let args = FakeRunner::args(&runner.calls("build")[0]);
    assert!(args.windows(2).any(|pair| pair == ["--out-link", "result"]));
    assert!(!args.contains(&"--no-link".to_owned()));
}

#[test]
fn build_out_link_requires_exactly_one_target() {
    let runner = FakeRunner::default();
    for targets in [Vec::new(), vec!["a".to_owned(), "b".to_owned()]] {
        let error = build_engine(&runner, limits())
            .build(BuildRequest {
                flake: flake(),
                targets,
                out_link: Some(PathBuf::from("result")),
            })
            .expect_err("out link target count");

        assert_eq!(error.code(), "invalid_out_link_targets");
    }
    assert!(runner.calls.lock().expect("calls").is_empty());
}

#[test]
fn build_out_link_failure_is_not_recovered_from_existing_outputs() {
    for (include_build_result, authoritative) in [(false, false), (true, false), (true, true)] {
        let mut runner = FakeRunner::default();
        runner.evaluations.insert(
            ("packages".to_owned(), "a".to_owned()),
            evaluation(DRV_A, OUT_A),
        );
        runner.graph = graph([node(DRV_A, OUT_A, &[])]);
        runner.local.insert(OUT_A.to_owned());
        runner.out_link_failure = true;
        if authoritative {
            runner.confirmed_results.push(json!({"success":true,"status":"Built","path":{"drvPath":DRV_A,"outputs":["out"]},"builtOutputs":{"out":{"outPath":OUT_A}}}));
        }
        if !include_build_result {
            runner.build_failures.insert(DRV_A.to_owned());
        }

        let manifest = build_engine(&runner, limits())
            .build(BuildRequest {
                flake: flake(),
                targets: vec!["a".to_owned()],
                out_link: Some(PathBuf::from("result")),
            })
            .expect("settled manifest");

        assert_eq!(manifest.outcome, ManifestOutcome::Failed);
        assert_eq!(manifest.roots[0].state, NodeState::Failed);
        assert!(
            manifest
                .diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.code == "realization_failed" })
        );
    }
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
            out_link: None,
        })
        .expect("manifest");

    assert_eq!(manifest.roots[0].state, NodeState::Failed);
    assert_eq!(runner.calls("build").len(), 1);
    assert_eq!(probe_phase_events(&progress), ["started", "finished"]);
}

#[test]
fn single_root_graph_failure_closes_the_open_probe_phase() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
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
            out_link: None,
        })
        .expect("manifest");

    assert_eq!(manifest.outcome, ManifestOutcome::Failed);
    assert_eq!(probe_phase_events(&progress), ["started", "finished"]);
}

#[test]
fn single_root_identity_mismatch_closes_the_open_probe_phase() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([node(DRV_A, OUT_B, &[])]);
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
            out_link: None,
        })
        .expect("manifest");

    assert!(
        manifest
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "root_output_identity_mismatch")
    );
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
fn complete_graph_mode_includes_shared_build_inputs_for_local_roots() {
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
    runner.graph = graph([
        node(DRV_C, OUT_C, &[]),
        node(DRV_A, OUT_A, &[(DRV_C, &["out"])]),
        node(DRV_B, OUT_B, &[(DRV_C, &["out"])]),
    ]);
    runner
        .local
        .extend([OUT_A.to_owned(), OUT_B.to_owned(), OUT_C.to_owned()]);

    let manifest = build_with_graph_mode(&runner, &["a", "b"], limits(), GraphMode::Complete)
        .expect("complete graph manifest");

    assert_eq!(
        manifest
            .graph
            .iter()
            .map(|node| node.drv_path.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([DRV_A, DRV_B, DRV_C])
    );
    assert_eq!(runner.calls("derivation").len(), 1);
    assert!(runner.builds.lock().expect("builds").is_empty());
    let shared = manifest
        .nodes
        .iter()
        .find(|node| node.drv_path == DRV_C)
        .expect("shared build input result");
    assert_eq!(shared.state, NodeState::Cached);
    assert_eq!(shared.produced_paths, [OUT_C]);
}

#[test]
fn complete_graph_mode_does_not_rebuild_a_missing_input_behind_a_local_root() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "root".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([
        node(DRV_C, OUT_C, &[]),
        node(DRV_A, OUT_A, &[(DRV_C, &["out"])]),
    ]);
    runner.local.insert(OUT_A.to_owned());

    let manifest = build_with_graph_mode(&runner, &["root"], limits(), GraphMode::Complete)
        .expect("complete graph manifest");

    assert!(runner.builds.lock().expect("builds").is_empty());
    assert!(manifest.nodes.iter().all(|node| node.drv_path != DRV_C));
    assert_eq!(
        manifest
            .availability
            .iter()
            .find(|entry| entry.path == OUT_C)
            .expect("dependency availability")
            .state,
        AvailabilityState::Missing
    );
}

#[test]
fn complete_graph_mode_does_not_build_unselected_outputs_of_cached_roots() {
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
    runner.graph = graph([
        (
            DRV_A.to_owned(),
            json!({
                "outputs": {"out": {"path": OUT_A}, "dev": {"path": OUT_C}},
                "inputDrvs": {}
            }),
        ),
        node(DRV_B, OUT_B, &[(DRV_A, &["dev"])]),
    ]);
    runner.local.extend([OUT_A.to_owned(), OUT_B.to_owned()]);

    let manifest = build_with_graph_mode(&runner, &["a", "b"], limits(), GraphMode::Complete)
        .expect("complete graph manifest");

    assert!(runner.builds.lock().expect("builds").is_empty());
    assert_eq!(manifest.outcome, ManifestOutcome::Success);
    assert!(
        manifest
            .roots
            .iter()
            .all(|root| root.state == NodeState::Cached)
    );
    let root = manifest
        .nodes
        .iter()
        .find(|node| node.drv_path == DRV_A)
        .expect("root a");
    assert_eq!(root.required_outputs, BTreeSet::from(["out".to_owned()]));
    assert_eq!(root.produced_paths, [OUT_A]);
    assert_eq!(
        manifest
            .graph
            .iter()
            .find(|node| node.drv_path == DRV_B)
            .expect("root b")
            .dependencies[DRV_A],
        BTreeSet::from(["dev".to_owned()])
    );
}

#[test]
fn complete_graph_mode_observes_dependency_outputs_of_a_selected_root() {
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
    runner.graph = graph([
        (
            DRV_A.to_owned(),
            json!({
                "outputs": {"out": {"path": OUT_A}, "dev": {"path": OUT_C}},
                "inputDrvs": {}
            }),
        ),
        node(DRV_B, OUT_B, &[(DRV_A, &["dev"])]),
    ]);
    runner.local.insert(OUT_A.to_owned());
    runner.local_after_build.insert(OUT_C.to_owned());

    let manifest = build_with_graph_mode(&runner, &["a", "b"], limits(), GraphMode::Complete)
        .expect("complete graph manifest");

    assert_eq!(*runner.builds.lock().expect("builds"), [DRV_B]);
    assert_eq!(manifest.outcome, ManifestOutcome::Success);
    let root = manifest
        .nodes
        .iter()
        .find(|node| node.drv_path == DRV_A)
        .expect("root a");
    assert_eq!(
        root.required_outputs,
        BTreeSet::from(["dev".to_owned(), "out".to_owned()])
    );
    assert_eq!(root.produced_paths, [OUT_A, OUT_C]);
    assert_eq!(root.state, NodeState::Realized);
    assert_eq!(
        manifest
            .availability
            .iter()
            .find(|entry| entry.path == OUT_C)
            .expect("dev availability")
            .state,
        AvailabilityState::Local
    );
}

#[test]
fn automatic_graph_mode_keeps_the_local_root_shortcut() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([
        node(DRV_C, OUT_C, &[]),
        node(DRV_A, OUT_A, &[(DRV_C, &["out"])]),
    ]);
    runner.local.insert(OUT_A.to_owned());

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
            out_link: None,
        })
        .expect("automatic graph manifest");

    assert_eq!(manifest.graph.len(), 1);
    assert!(runner.calls("derivation").is_empty());
    let events = progress.0.lock().expect("progress");
    let graph = events
        .iter()
        .position(|event| matches!(event, ProgressEvent::GraphDiscovered(_)))
        .expect("root graph event");
    assert!(matches!(
        events.get(graph + 1),
        Some(ProgressEvent::NodeFinished { .. })
    ));
}

#[test]
fn complete_graph_mode_preserves_required_multi_output_edges() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "root".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([
        (
            DRV_C.to_owned(),
            json!({
                "outputs": {
                    "dev": {"path": OUT_B},
                    "out": {"path": OUT_C}
                },
                "inputDrvs": {}
            }),
        ),
        node(DRV_A, OUT_A, &[(DRV_C, &["dev", "out"])]),
    ]);
    runner.local.extend([OUT_A.to_owned(), OUT_C.to_owned()]);

    let manifest = build_with_graph_mode(&runner, &["root"], limits(), GraphMode::Complete)
        .expect("complete graph manifest");
    let root = manifest
        .graph
        .iter()
        .find(|node| node.drv_path == DRV_A)
        .expect("root graph node");

    assert_eq!(
        root.dependencies[DRV_C],
        BTreeSet::from(["dev".to_owned(), "out".to_owned()])
    );
    let dependency = manifest
        .nodes
        .iter()
        .find(|node| node.drv_path == DRV_C)
        .expect("partially available dependency");
    assert_eq!(
        dependency.required_outputs,
        BTreeSet::from(["out".to_owned()])
    );
    assert_eq!(dependency.produced_paths, [OUT_C]);
}

#[test]
fn complete_graph_mode_clears_remote_metadata_after_dependency_becomes_local() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "root".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([
        node(DRV_C, OUT_C, &[]),
        node(DRV_A, OUT_A, &[(DRV_C, &["out"])]),
    ]);
    runner.remote.insert(
        "https://cache.example".to_owned(),
        BTreeSet::from([OUT_C.to_owned()]),
    );
    runner.local_after_build.insert(OUT_C.to_owned());

    let manifest = build_with_graph_mode(&runner, &["root"], limits(), GraphMode::Complete)
        .expect("complete graph manifest");
    let dependency = manifest
        .availability
        .iter()
        .find(|entry| entry.path == OUT_C)
        .expect("dependency availability");

    assert_eq!(dependency.state, AvailabilityState::Local);
    assert!(dependency.substituter.is_none());
    assert!(dependency.download_bytes.is_none());
    assert_eq!(
        manifest
            .nodes
            .iter()
            .find(|node| node.drv_path == DRV_C)
            .expect("dependency result")
            .state,
        NodeState::Realized
    );
}

#[test]
fn complete_graph_mode_does_not_fabricate_dependency_results_after_a_truncated_reprobe() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "root".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([
        node(DRV_C, OUT_C, &[]),
        node(DRV_A, OUT_A, &[(DRV_C, &["out"])]),
    ]);
    runner.local_after_build.insert(OUT_C.to_owned());
    runner.truncate_local_after_build = true;

    let manifest = build_with_graph_mode(&runner, &["root"], limits(), GraphMode::Complete)
        .expect("complete graph manifest");

    assert!(manifest.nodes.iter().all(|node| node.drv_path != DRV_C));
    assert!(
        manifest
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "local_cache_probe_failed")
    );
}

#[test]
fn cancellation_recovers_local_outputs_without_definitive_events() {
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
    runner.cancel_build = Some(DRV_B.to_owned());
    runner.local_after_build.insert(OUT_A.to_owned());
    runner.cleanup_probe = CleanupProbe::Partial;
    let manifest = build(&runner, &["a", "b"], limits());
    assert_eq!(manifest.outcome, ManifestOutcome::Cancelled);
    let fast = manifest
        .nodes
        .iter()
        .find(|node| node.drv_path == DRV_A)
        .expect("fast node");
    assert_eq!(fast.state, NodeState::Realized);
    assert_eq!(fast.produced_paths, [OUT_A]);
    assert_eq!(
        manifest
            .nodes
            .iter()
            .find(|node| node.drv_path == DRV_B)
            .expect("slow node")
            .state,
        NodeState::Cancelled
    );
    assert_eq!(runner.calls("path-info").len(), 2);
}

#[test]
fn cancellation_recovers_stopped_unknown_dependency_without_definitive_events() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.evaluations.insert(
        ("packages".to_owned(), "b".to_owned()),
        evaluation(DRV_B, OUT_B),
    );
    runner.graph = graph([
        node(DRV_A, OUT_A, &[(DRV_C, &["out"])]),
        node(DRV_B, OUT_B, &[(DRV_C, &["out"])]),
        node(DRV_C, OUT_C, &[]),
    ]);
    runner.cancel_build = Some(DRV_A.to_owned());
    runner.stopped_builds.push(DRV_C.to_owned());
    runner.local_after_build.insert(OUT_C.to_owned());
    let manifest = build(&runner, &["a", "b"], limits());
    let completed = manifest
        .nodes
        .iter()
        .find(|node| node.drv_path == DRV_C)
        .expect("stopped dependency recovered");
    assert_eq!(completed.state, NodeState::Realized);
    assert_eq!(completed.produced_paths, [OUT_C]);
    assert!(completed.dependencies.is_empty());
    assert!(
        manifest
            .roots
            .iter()
            .all(|root| root.state == NodeState::Cancelled)
    );
    let metadata = runner.calls("derivation");
    assert_eq!(metadata.len(), 1);
    assert!(!FakeRunner::args(&metadata[0]).contains(&"--recursive".to_owned()));
    assert_eq!(FakeRunner::stdin(&metadata[0]), format!("{DRV_C}\n"));
    assert_eq!(runner.calls("path-info").len(), 2);
}

#[test]
fn cancellation_cleanup_deadline_and_invalid_json_do_not_promote_nodes() {
    for quirk in [
        CleanupProbe::Stall,
        CleanupProbe::Malformed,
        CleanupProbe::Normal,
    ] {
        let mut runner = FakeRunner::default();
        runner.evaluations.insert(
            ("packages".to_owned(), "a".to_owned()),
            evaluation(DRV_A, OUT_A),
        );
        runner.graph = graph([node(DRV_A, OUT_A, &[])]);
        runner.cancel_build = Some(DRV_A.to_owned());
        runner.cleanup_probe = quirk;
        runner.truncate_local_after_build = quirk == CleanupProbe::Normal;
        runner.local_after_build.insert(OUT_A.to_owned());
        let started = std::time::Instant::now();
        let manifest = build(&runner, &["a"], limits());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "cleanup has one total deadline"
        );
        assert_eq!(manifest.outcome, ManifestOutcome::Cancelled);
        assert_eq!(manifest.nodes[0].state, NodeState::Cancelled);
        assert!(manifest.nodes[0].produced_paths.is_empty());
        let calls = runner.calls("path-info");
        let cleanup = calls.last().expect("cleanup probe");
        assert!(FakeRunner::args(cleanup).contains(&"--offline".to_owned()));
        assert_eq!(cleanup.cleanup_timeout, Duration::from_millis(100));
    }
}

#[test]
fn cancellation_preserves_unknown_dependency_with_multiple_automatic_roots() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.evaluations.insert(
        ("packages".to_owned(), "b".to_owned()),
        evaluation(DRV_B, OUT_B),
    );
    runner.graph = graph([
        node(DRV_A, OUT_A, &[(DRV_C, &["out"])]),
        node(DRV_B, OUT_B, &[]),
        node(DRV_C, OUT_C, &[]),
    ]);
    runner.cancel_build = Some(DRV_A.to_owned());
    runner.confirmed_results.push(json!({"success": true, "status": "Built", "path": {"drvPath": DRV_C, "outputs": ["out"]}, "builtOutputs": {"out": {"outPath": OUT_C}}}));
    let manifest = build(&runner, &["a", "b"], limits());
    assert_eq!(manifest.outcome, ManifestOutcome::Cancelled);
    let completed = manifest
        .nodes
        .iter()
        .find(|node| node.drv_path == DRV_C)
        .expect("authoritative unknown dependency retained");
    assert_eq!(completed.state, NodeState::Built);
    assert_eq!(completed.produced_paths, [OUT_C]);
    assert_eq!(
        completed.required_outputs,
        BTreeSet::from(["out".to_owned()])
    );
    assert!(completed.dependencies.is_empty());
    assert!(runner.calls("derivation").is_empty());
    assert_eq!(runner.calls("path-info").len(), 2);
}

#[test]
fn cancellation_preserves_confirmed_selected_and_transitive_results() {
    for (selected, mode) in [
        (false, GraphMode::Complete),
        (true, GraphMode::Complete),
        (false, GraphMode::Automatic),
        (true, GraphMode::Automatic),
    ] {
        let mut runner = FakeRunner::default();
        runner.evaluations.insert(
            ("packages".to_owned(), "root".to_owned()),
            evaluation(DRV_A, OUT_A),
        );
        if selected {
            runner.evaluations.insert(
                ("packages".to_owned(), "dependency".to_owned()),
                evaluation(DRV_B, OUT_B),
            );
        }
        runner.graph = graph([
            node(DRV_A, OUT_A, &[(DRV_B, &["out"])]),
            node(DRV_B, OUT_B, &[]),
        ]);
        runner.cancel_build = Some(DRV_A.to_owned());
        runner.confirmed_results.push(json!({"success": true, "status": "Substituted", "path": {"drvPath": DRV_B}, "builtOutputs": {"out": {"outPath": OUT_B}}}));
        let targets = if selected {
            vec!["root", "dependency"]
        } else {
            vec!["root"]
        };
        let manifest = build_with_graph_mode(&runner, &targets, limits(), mode).expect("manifest");
        assert_eq!(manifest.outcome, ManifestOutcome::Cancelled);
        let completed = manifest
            .nodes
            .iter()
            .find(|node| node.drv_path == DRV_B)
            .expect("confirmed dependency retained");
        assert_eq!(completed.state, NodeState::Substituted);
        assert_eq!(completed.produced_paths, [OUT_B]);
        assert_eq!(
            manifest
                .nodes
                .iter()
                .find(|node| node.drv_path == DRV_A)
                .expect("root")
                .state,
            NodeState::Cancelled
        );
        assert_eq!(
            runner.calls("path-info").len(),
            if selected && mode == GraphMode::Automatic {
                2
            } else {
                3
            }
        );
    }
}

#[test]
fn cancellation_requires_all_dependency_outputs_and_accepts_ca_paths() {
    for complete in [false, true] {
        let mut runner = FakeRunner::default();
        runner.evaluations.insert(
            ("packages".to_owned(), "root".to_owned()),
            evaluation(DRV_A, OUT_A),
        );
        runner.graph = graph([
            node(DRV_A, OUT_A, &[(DRV_B, &["out", "dev"])]),
            node(DRV_B, OUT_B, &[]),
        ]);
        runner.graph[DRV_B]["outputs"] = json!({"out": {}, "dev": {"path": OUT_C}});
        runner.cancel_build = Some(DRV_A.to_owned());
        let mut outputs =
            json!({"out": {"outPath": OUT_B.strip_prefix("/nix/store/").expect("basename")}});
        if complete {
            outputs["dev"] = json!({"outPath": OUT_C});
        }
        runner.confirmed_results.push(json!({"success": true, "status": "Built", "path": {"drvPath": DRV_B}, "builtOutputs": outputs}));
        let manifest = build_with_graph_mode(&runner, &["root"], limits(), GraphMode::Complete)
            .expect("manifest");
        assert_eq!(manifest.outcome, ManifestOutcome::Cancelled);
        let dependency = manifest.nodes.iter().find(|node| node.drv_path == DRV_B);
        if complete {
            let dependency = dependency.expect("complete CA dependency");
            assert_eq!(dependency.state, NodeState::Built);
            assert_eq!(dependency.produced_paths, [OUT_B, OUT_C]);
            assert_eq!(
                dependency.required_outputs,
                BTreeSet::from(["out".to_owned(), "dev".to_owned()])
            );
        } else {
            assert!(dependency.is_none());
        }
        assert_eq!(runner.calls("path-info").len(), 3);
    }
}

#[test]
fn cancellation_preserves_outputs_without_claiming_out_link_success() {
    for definitive in [true, false] {
        let mut runner = FakeRunner::default();
        runner.evaluations.insert(
            ("packages".to_owned(), "root".to_owned()),
            evaluation(DRV_A, OUT_A),
        );
        runner.graph = graph([node(DRV_A, OUT_A, &[])]);
        runner.cancel_build = Some(DRV_A.to_owned());
        runner.confirmed_results.push(json!({"success": true, "status": "Built", "path": {"drvPath": DRV_A}, "builtOutputs": {"out": {"outPath": OUT_A}}}));
        if !definitive {
            runner.confirmed_results.clear();
            runner.local_after_build.insert(OUT_A.to_owned());
        }
        let cancellation = Cancellation::default();
        let clock = FakeClock::default();
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
                targets: vec!["root".to_owned()],
                out_link: Some(PathBuf::from("/workspace/result")),
            })
            .expect("manifest");
        assert_eq!(manifest.outcome, ManifestOutcome::Cancelled);
        assert_eq!(manifest.roots[0].state, NodeState::Cancelled);
        assert_eq!(manifest.nodes[0].state, NodeState::Cancelled);
        assert_eq!(manifest.nodes[0].produced_paths, [OUT_A]);
    }
}

#[test]
fn complete_graph_mode_only_reconciles_requested_outputs_after_cancellation() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "root".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([
        node(DRV_B, OUT_B, &[]),
        node(DRV_C, OUT_C, &[]),
        node(DRV_A, OUT_A, &[(DRV_B, &["out"]), (DRV_C, &["out"])]),
    ]);
    runner.local.insert(OUT_C.to_owned());
    runner.cancel_build = Some(DRV_A.to_owned());
    let cancellation = Cancellation::default();
    let clock = FakeClock::with([100, 200]);
    let progress = FakeProgress::default();
    let mut engine_config = config(limits());
    engine_config.graph_mode = GraphMode::Complete;
    let engine = NixEngine::new(
        engine_config,
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
            targets: vec!["root".to_owned()],
            out_link: None,
        })
        .expect("cancelled manifest");

    assert_eq!(manifest.outcome, ManifestOutcome::Cancelled);
    let cached = manifest
        .nodes
        .iter()
        .find(|node| node.drv_path == DRV_C)
        .expect("cached dependency retained after cancellation");
    assert_eq!(cached.state, NodeState::Cached);
    assert_eq!(cached.produced_paths, [OUT_C]);
    assert!(manifest.nodes.iter().all(|node| node.drv_path != DRV_B));
    assert_eq!(
        runner
            .calls("path-info")
            .iter()
            .filter(|spec| !FakeRunner::args(spec).contains(&"--store".to_owned()))
            .count(),
        2
    );
    assert_eq!(
        progress
            .0
            .lock()
            .expect("progress")
            .iter()
            .filter(|event| matches!(event, ProgressEvent::Cancelled { .. }))
            .count(),
        1
    );
}

#[test]
fn complete_graph_mode_reports_graph_limits_as_failure() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "root".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([
        node(DRV_C, OUT_C, &[]),
        node(DRV_B, OUT_B, &[(DRV_C, &["out"])]),
        node(DRV_A, OUT_A, &[(DRV_B, &["out"])]),
    ]);
    let mut bounded = limits();
    bounded.max_graph_nodes = 2;

    let manifest = build_with_graph_mode(&runner, &["root"], bounded, GraphMode::Complete)
        .expect("settled graph failure");

    assert_eq!(manifest.outcome, ManifestOutcome::Failed);
    assert!(manifest.graph.is_empty());
    assert!(
        manifest
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "graph_node_limit_exceeded")
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
            out_link: None,
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
    for node in graphs[1] {
        let persisted = manifest
            .graph
            .iter()
            .find(|persisted| persisted.drv_path == node.drv_path)
            .expect("persisted graph node");
        assert!(
            std::sync::Arc::ptr_eq(node, persisted),
            "progress and manifest share graph payloads"
        );
    }
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
    let flake_directory = fs::canonicalize(&directory).expect("canonical flake directory");
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
            flake: FlakeRef::new(".", Some(flake_directory)),
            targets: vec!["fail".to_owned(), "succeed".to_owned()],
            out_link: None,
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
#[cfg(feature = "nix-integration")]
fn real_nix_complete_graph_includes_a_shared_build_input() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("nix-tools-complete-graph-{nonce}"));
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
  outputs = {{ self }}: let
    system = "{system}";
    dep = builtins.derivation {{
      name = "nix-tools-shared-{nonce}";
      inherit system;
      builder = builtins.storePath "{bash}";
      args = [ "-c" "echo shared > $out" ];
    }};
    root = name: let drv = builtins.derivation {{
        name = "nix-tools-${{name}}-{nonce}";
        inherit system;
        builder = builtins.storePath "{bash}";
        args = [ "-c" "echo ${{dep}} > $out" ];
      }};
    in drv // {{ outputs = [ "out" ]; out = drv; meta.outputsToInstall = [ "out" ]; }};
  in {{
    packages.${{system}} = {{ a = root "a"; b = root "b"; }};
  }};
}}"#,
            system = NixSystem::host().expect("host system"),
            bash = bash.display(),
        ),
    )
    .expect("flake");
    let flake_directory = fs::canonicalize(&directory).expect("canonical flake directory");
    let runner = StdProcessRunner::new(Duration::from_millis(10), Redactor::default());
    let cancellation = Cancellation::default();
    let clock = SystemClock;
    let progress = FakeProgress::default();
    let mut engine_config = EngineConfig::new("nix", NixSystem::host().expect("host system"));
    engine_config.graph_mode = GraphMode::Complete;
    let engine = NixEngine::new(
        engine_config,
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
            flake: FlakeRef::new(".", Some(flake_directory)),
            targets: vec!["a".to_owned(), "b".to_owned()],
            out_link: None,
        })
        .expect("complete graph manifest");

    fs::remove_dir_all(directory).expect("remove temporary flake");
    assert_eq!(
        manifest.outcome,
        ManifestOutcome::Success,
        "diagnostics: {:?}",
        manifest.diagnostics
    );
    assert_eq!(manifest.graph.len(), 3);
    let shared = manifest
        .graph
        .iter()
        .find(|node| node.dependencies.is_empty())
        .expect("shared build input");
    assert_eq!(
        manifest
            .graph
            .iter()
            .filter(|node| node.dependencies.contains_key(&shared.drv_path))
            .count(),
        2
    );
    let shared_result = manifest
        .nodes
        .iter()
        .find(|node| node.drv_path == shared.drv_path)
        .expect("shared build input result");
    assert_eq!(shared_result.state, NodeState::Realized);
    assert_eq!(shared_result.produced_paths.len(), 1);
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
            out_link: None,
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
            out_link: None,
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

#[test]
fn realization_streams_a_node_start_for_each_activity_nix_reports() {
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
    let clock = FakeClock::default();
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
            targets: vec!["a".to_owned(), "b".to_owned()],
            out_link: None,
        })
        .expect("build");

    let events = progress.0.lock().expect("progress");
    for drv_path in [DRV_A, DRV_B] {
        let started = events
            .iter()
            .position(|event| {
                event
                    == &ProgressEvent::NodeStarted {
                        drv_path: drv_path.to_owned(),
                    }
            })
            .expect("node started");
        let finished = events
            .iter()
            .position(|event| {
                matches!(event, ProgressEvent::NodeFinished { drv_path: path, .. } if path == drv_path)
            })
            .expect("node finished");
        assert!(started < finished);
    }
    assert!(
        FakeRunner::args(&runner.calls("build")[0]).contains(&"internal-json".to_owned()),
        "realization must request the streaming log format"
    );
}

#[test]
fn a_realization_diagnostic_reports_the_rebuilt_log_rather_than_the_json_envelope() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);
    runner.build_failures.insert(DRV_A.to_owned());
    runner.build_log_lines = 2;

    let manifest = build(&runner, &["a"], limits());

    let diagnostic = manifest
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.code == "realization_failed" && diagnostic.target.as_deref() == Some(DRV_A)
        })
        .expect("realization diagnostic");
    assert!(
        manifest
            .diagnostics
            .iter()
            .any(|entry| entry.target.is_none() && entry.stderr.contains(BUILD_ERROR)),
        "diagnostic must carry the terminating error: {}",
        diagnostic.stderr
    );
    assert!(
        !diagnostic.stderr.contains("unreconstructed envelope"),
        "the raw JSON capture must not reach a diagnostic: {}",
        diagnostic.stderr
    );
    assert!(
        diagnostic
            .stderr
            .contains("a> configure: checking chatter 0"),
        "build output must name the derivation that printed it: {}",
        diagnostic.stderr
    );
    assert!(!diagnostic.truncated);
}

#[test]
fn a_terminating_build_error_survives_a_log_far_past_the_diagnostic_bound() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);
    runner.build_failures.insert(DRV_A.to_owned());
    runner.build_log_lines = 400;

    let manifest = build(&runner, &["a"], limits());

    let diagnostic = manifest
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.code == "realization_failed" && diagnostic.target.as_deref() == Some(DRV_A)
        })
        .expect("realization diagnostic");
    assert!(
        manifest
            .diagnostics
            .iter()
            .any(|entry| entry.target.is_none() && entry.stderr.contains(BUILD_ERROR)),
        "a chatty build must not bury its own failure: {}",
        diagnostic.stderr
    );
    assert!(
        diagnostic
            .stderr
            .contains("a> configure: checking chatter 0"),
        "the opening of the log must survive too: {}",
        diagnostic.stderr
    );
    assert!(
        diagnostic.stderr.len() <= limits().max_diagnostic_bytes,
        "the diagnostic must stay bounded: {}",
        diagnostic.stderr.len()
    );
    assert!(
        diagnostic.stderr.contains("[log truncated]\n"),
        "a reader must see where lines went missing: {}",
        diagnostic.stderr
    );
    assert!(diagnostic.truncated);
}

#[test]
fn a_truncated_json_capture_does_not_mark_a_complete_rebuilt_log_truncated() {
    let mut runner = FakeRunner::default();
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);
    runner.build_failures.insert(DRV_A.to_owned());
    runner.build_quirk = BuildQuirk::TruncatedCapture;

    let manifest = build(&runner, &["a"], limits());

    let diagnostic = manifest
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "realization_failed")
        .expect("realization diagnostic");
    assert!(diagnostic.stderr.contains(BUILD_ERROR));
    assert!(
        !diagnostic.truncated,
        "the flag must describe the reported text, not the discarded envelope"
    );
}

#[test]
#[should_panic(expected = "fake realization run panicked")]
fn a_panicking_realization_run_unwinds_instead_of_blocking_on_its_forwarder() {
    let mut runner = FakeRunner {
        build_quirk: BuildQuirk::Panic,
        ..FakeRunner::default()
    };
    runner.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    runner.graph = graph([node(DRV_A, OUT_A, &[])]);

    drop(build(&runner, &["a"], limits()));
}

#[test]
fn failed_nodes_keep_their_own_log_excerpt_with_shared_context_once() {
    let mut runner = FakeRunner::default();
    for (name, drv, out) in [("a", DRV_A, OUT_A), ("b", DRV_B, OUT_B)] {
        runner.evaluations.insert(
            ("packages".to_owned(), name.to_owned()),
            evaluation(drv, out),
        );
        runner.build_failures.insert(drv.to_owned());
    }
    runner.graph = graph([node(DRV_A, OUT_A, &[]), node(DRV_B, OUT_B, &[])]);
    runner.build_log_lines = 2;
    let manifest = build(&runner, &["a", "b"], limits());
    for (drv, own, other) in [(DRV_A, "a>", "b>"), (DRV_B, "b>", "a>")] {
        let diagnostic = manifest
            .diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.target.as_deref() == Some(drv) && diagnostic.code == "realization_failed"
            })
            .expect("node failure");
        assert!(diagnostic.stderr.contains(own));
        assert!(!diagnostic.stderr.contains(other));
        assert!(!diagnostic.stderr.contains(BUILD_ERROR));
    }
    let contexts = manifest
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.target.is_none() && diagnostic.stderr.contains(BUILD_ERROR))
        .collect::<Vec<_>>();
    assert_eq!(contexts.len(), 1);
    assert!(!contexts[0].stderr.contains("a>"));
    assert!(!contexts[0].stderr.contains("b>"));
}

#[test]
fn raw_failed_build_diagnostics_are_normalized_and_redacted_before_bounding() {
    struct RawFailureRunner(FakeRunner, nix_tools_core::redaction::Redactor);
    impl ProcessRunner for RawFailureRunner {
        fn run(&self, spec: &ProcessSpec, cancellation: &Cancellation) -> Result<ProcessResult> {
            if FakeRunner::args(spec)
                .first()
                .is_some_and(|arg| arg == "build")
            {
                let mut result = process_with_code(1, b"\x1b[31mprivate-\x1b[0mvalue\n");
                result.stdout.bytes = b"private-value\n".to_vec();
                Ok(result)
            } else {
                self.0.run(spec, cancellation)
            }
        }
        fn redactor(&self) -> nix_tools_core::redaction::Redactor {
            self.1.clone()
        }
    }
    let mut fake = FakeRunner::default();
    fake.evaluations.insert(
        ("packages".to_owned(), "a".to_owned()),
        evaluation(DRV_A, OUT_A),
    );
    fake.graph = graph([node(DRV_A, OUT_A, &[])]);
    let redactor = nix_tools_core::redaction::Redactor::default();
    redactor.register(b"private-value");
    let runner = RawFailureRunner(fake, redactor);
    let cancellation = Cancellation::default();
    let clock = FakeClock::default();
    let progress = FakeProgress::default();
    let mut resource_limits = limits();
    resource_limits.max_diagnostic_bytes = 12;
    let engine = NixEngine::new(
        config(resource_limits),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .unwrap();
    let manifest = engine
        .build(BuildRequest {
            flake: flake(),
            targets: vec!["a".to_owned()],
            out_link: None,
        })
        .unwrap();
    let context = manifest
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.phase == Phase::Realization && diagnostic.target.is_none())
        .unwrap();
    assert_eq!(context.stdout, "[REDACTED]\n");
    assert_eq!(context.stderr, "[REDACTED]\n");
}

#[test]
fn cached_roots_and_proven_dependencies_settle_before_other_builds_start() {
    for mode in [GraphMode::Automatic, GraphMode::Complete] {
        let mut runner = FakeRunner::default();
        for (name, drv, out) in [("a", DRV_A, OUT_A), ("b", DRV_B, OUT_B)] {
            runner.evaluations.insert(
                ("packages".to_owned(), name.to_owned()),
                evaluation(drv, out),
            );
        }
        runner.graph = graph([
            node(DRV_A, OUT_A, &[(DRV_C, &["out"])]),
            node(DRV_B, OUT_B, &[]),
            node(DRV_C, OUT_C, &[]),
        ]);
        runner.local.extend([OUT_A.to_owned(), OUT_C.to_owned()]);
        let cancellation = Cancellation::default();
        let clock = FakeClock::default();
        let progress = FakeProgress::default();
        let mut configuration = config(limits());
        configuration.graph_mode = mode;
        let engine = NixEngine::new(
            configuration,
            EngineDependencies {
                runner: &runner,
                cancellation: &cancellation,
                clock: &clock,
                progress: &progress,
            },
        )
        .unwrap();
        engine
            .build(BuildRequest {
                flake: flake(),
                targets: vec!["a".to_owned(), "b".to_owned()],
                out_link: None,
            })
            .unwrap();
        let events = progress.0.lock().unwrap();
        let build_start = events
            .iter()
            .position(
                |event| matches!(event, ProgressEvent::NodeStarted {drv_path} if drv_path == DRV_B),
            )
            .unwrap();
        assert!(
            events[..build_start].contains(&ProgressEvent::NodeFinished {
                drv_path: DRV_A.to_owned(),
                state: NodeState::Cached
            })
        );
        assert_eq!(
            events[..build_start].contains(&ProgressEvent::NodeFinished {
                drv_path: DRV_C.to_owned(),
                state: NodeState::Cached
            }),
            mode == GraphMode::Complete
        );
    }
}

#[test]
fn warm_shortcuts_report_cached_but_forced_out_links_wait_for_realization() {
    for out_link in [None, Some(PathBuf::from("result"))] {
        let mut runner = FakeRunner::default();
        runner.evaluations.insert(
            ("packages".to_owned(), "a".to_owned()),
            evaluation(DRV_A, OUT_A),
        );
        runner.graph = graph([node(DRV_A, OUT_A, &[])]);
        runner.local.insert(OUT_A.to_owned());
        let cancellation = Cancellation::default();
        let clock = FakeClock::default();
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
        .unwrap();
        engine
            .build(BuildRequest {
                flake: flake(),
                targets: vec!["a".to_owned()],
                out_link: out_link.clone(),
            })
            .unwrap();
        let events = progress.0.lock().unwrap();
        let cached = events.iter().position(|event| matches!(event, ProgressEvent::NodeFinished {drv_path, state: NodeState::Cached} if drv_path == DRV_A)).unwrap();
        if out_link.is_some() {
            let started = events.iter().position(|event| matches!(event, ProgressEvent::NodeStarted {drv_path} if drv_path == DRV_A)).unwrap();
            assert!(started < cached);
        } else {
            assert!(
                events[..cached]
                    .iter()
                    .any(|event| matches!(event, ProgressEvent::GraphDiscovered(_)))
            );
            assert!(runner.calls("build").is_empty());
        }
    }
}
