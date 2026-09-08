use std::ffi::OsString;
use std::time::Duration;

use nix_tools_core::process::{
    Cancellation, CapturedStream, ChildTermination, ProcessResult, ProcessRunner, ProcessSpec,
    StreamPolicy,
};
use nix_tools_engine::{Clock, FlakeRef, ManifestOutcome};

use crate::{
    CheckSelector, DisplayContext, OutputMode, Runtime, RuntimeCommand, RuntimeConfig,
    RuntimeDependencies, SelectedCheckCommand,
};

struct NeverRunner;

struct FailingEvaluationRunner;

struct DiscoveryOnlyRunner;

impl ProcessRunner for DiscoveryOnlyRunner {
    fn run(
        &self,
        spec: &ProcessSpec,
        _cancellation: &Cancellation,
    ) -> nix_tools_core::outcome::Result<ProcessResult> {
        assert!(
            !spec
                .env
                .contains_key(std::ffi::OsStr::new("NIX_TOOLS_ENGINE_KIND")),
            "an empty selection must not evaluate every check"
        );
        Ok(ProcessResult {
            termination: ChildTermination::Exited(0),
            stdout: CapturedStream {
                bytes: br#"{"packages":[],"checks":["app:test"],"apps":[]}"#.to_vec(),
                truncated: false,
            },
            stderr: CapturedStream::default(),
            combined: None,
            duration: Duration::ZERO,
        })
    }
}

struct EmptySelector;

impl CheckSelector for EmptySelector {
    fn select(
        &self,
        _scope: &str,
        _available: &[String],
    ) -> nix_tools_core::outcome::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl ProcessRunner for NeverRunner {
    fn run(
        &self,
        _spec: &ProcessSpec,
        _cancellation: &Cancellation,
    ) -> nix_tools_core::outcome::Result<ProcessResult> {
        panic!("a pre-cancelled runtime must not start nix")
    }
}

impl ProcessRunner for FailingEvaluationRunner {
    fn run(
        &self,
        _spec: &ProcessSpec,
        _cancellation: &Cancellation,
    ) -> nix_tools_core::outcome::Result<ProcessResult> {
        Ok(ProcessResult {
            termination: ChildTermination::Exited(23),
            stdout: CapturedStream::default(),
            stderr: CapturedStream {
                bytes: b"evaluation failed".to_vec(),
                truncated: false,
            },
            combined: None,
            duration: Duration::from_millis(1),
        })
    }
}

struct FixedClock;

struct CachedBuildGraphRunner;

const BUILD_INPUT: &str = "/nix/store/00000000000000000000000000000000-build-input.drv";
const BUILD_INPUT_OUT: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-build-input";
const ROOT_ONE: &str = "/nix/store/11111111111111111111111111111111-one.drv";
const ROOT_ONE_OUT: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-one";
const ROOT_TWO: &str = "/nix/store/22222222222222222222222222222222-two.drv";
const ROOT_TWO_OUT: &str = "/nix/store/cccccccccccccccccccccccccccccccc-two";

impl ProcessRunner for CachedBuildGraphRunner {
    fn run(
        &self,
        spec: &ProcessSpec,
        _cancellation: &Cancellation,
    ) -> nix_tools_core::outcome::Result<ProcessResult> {
        use serde_json::json;

        let value = match spec.args.first().and_then(|arg| arg.to_str()) {
            Some("eval") => json!([
                {"success": true, "value": {"drvPath": ROOT_ONE,
                    "outputs": {"out": ROOT_ONE_OUT}, "outputsToInstall": ["out"]}},
                {"success": true, "value": {"drvPath": ROOT_TWO,
                    "outputs": {"out": ROOT_TWO_OUT}, "outputsToInstall": ["out"]}}
            ]),
            Some("derivation") => json!({
                BUILD_INPUT: {"outputs": {"out": {"path": BUILD_INPUT_OUT}}, "inputDrvs": {}},
                ROOT_ONE: {"outputs": {"out": {"path": ROOT_ONE_OUT}},
                    "inputDrvs": {BUILD_INPUT: {"outputs": ["out"]}}},
                ROOT_TWO: {"outputs": {"out": {"path": ROOT_TWO_OUT}},
                    "inputDrvs": {BUILD_INPUT: {"outputs": ["out"]}}}
            }),
            Some("path-info") => {
                let nix_tools_core::process::InputPolicy::Bytes(input) = &spec.stdin else {
                    panic!("path-info must receive requested paths on stdin");
                };
                let entries = std::str::from_utf8(input)
                    .expect("UTF-8 paths")
                    .lines()
                    .map(|path| (path.to_owned(), json!({"narSize": 10})))
                    .collect::<serde_json::Map<_, _>>();
                serde_json::Value::Object(entries)
            }
            other => panic!("cached fixture must not build: {other:?}"),
        };
        let bytes = serde_json::to_vec(&value).expect("fixture JSON");
        // The graph is streamed rather than captured, so it reaches the engine through the
        // consumer the spec carries.
        let stdout = if let StreamPolicy::Consume { consumer, .. } = &spec.stdout {
            consumer
                .consume(&mut bytes.as_slice())
                .expect("consume fixture");
            CapturedStream::default()
        } else {
            CapturedStream {
                bytes,
                truncated: false,
            }
        };
        Ok(ProcessResult {
            termination: ChildTermination::Exited(0),
            stdout,
            stderr: CapturedStream::default(),
            combined: None,
            duration: Duration::ZERO,
        })
    }
}

#[test]
fn runtime_complete_graph_retains_shared_build_inputs_for_cached_roots() {
    let mut engine =
        nix_tools_engine::EngineConfig::new("nix", nix_tools_core::system::NixSystem::X86_64Linux);
    engine.graph_mode = crate::GraphMode::Complete;
    let cancellation = Cancellation::default();
    let runtime = Runtime::new(
        RuntimeConfig::new(engine, crate::AppExecutionPolicy::minimal()),
        RuntimeDependencies {
            runner: &CachedBuildGraphRunner,
            cancellation: &cancellation,
            clock: &FixedClock,
        },
    );
    for command in [
        RuntimeCommand::Build {
            title: "build".to_owned(),
            flake: FlakeRef::new(".", None),
            targets: vec!["one".to_owned(), "two".to_owned()],
            out_link: None,
            output: OutputMode::Stream,
        },
        RuntimeCommand::Check {
            title: "check".to_owned(),
            flake: FlakeRef::new(".", None),
            targets: vec!["one".to_owned(), "two".to_owned()],
            output: OutputMode::Stream,
        },
    ] {
        let manifest = runtime.execute(command).expect("cached realization");
        assert_eq!(manifest.outcome, ManifestOutcome::Success);
        assert_eq!(manifest.graph.len(), 3);
        let input = manifest
            .nodes
            .iter()
            .find(|node| node.drv_path == BUILD_INPUT)
            .expect("manifest must retain the shared build dependency");
        assert_eq!(input.produced_paths, [BUILD_INPUT_OUT]);
        assert!(
            manifest
                .graph
                .iter()
                .filter(|node| node.drv_path != BUILD_INPUT)
                .all(|node| node.dependencies.contains_key(BUILD_INPUT))
        );
    }
}

#[test]
fn tui_keeps_large_transitive_graphs_on_the_root_only_path() {
    let mut engine =
        nix_tools_engine::EngineConfig::new("nix", nix_tools_core::system::NixSystem::X86_64Linux);
    engine.limits.max_graph_nodes = 1;
    let cancellation = Cancellation::default();
    let runtime = Runtime::new(
        RuntimeConfig::new(engine, crate::AppExecutionPolicy::minimal()),
        RuntimeDependencies {
            runner: &CachedBuildGraphRunner,
            cancellation: &cancellation,
            clock: &FixedClock,
        },
    );

    let manifest = runtime
        .execute(RuntimeCommand::Check {
            title: "check".to_owned(),
            flake: FlakeRef::new(".", None),
            targets: vec!["one".to_owned(), "two".to_owned()],
            output: OutputMode::Tui,
        })
        .expect("automatic TUI realization");

    assert_eq!(manifest.outcome, ManifestOutcome::Success);
    assert_eq!(manifest.graph.len(), 2);
}

struct NeverSelector;

impl CheckSelector for NeverSelector {
    fn select(
        &self,
        _scope: &str,
        _available: &[String],
    ) -> nix_tools_core::outcome::Result<Vec<String>> {
        panic!("a pre-cancelled runtime must not select checks")
    }
}

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        0
    }
}

#[test]
fn public_runtime_owns_display_selection_and_preserves_cancellation() {
    assert_eq!(
        OutputMode::select(
            OutputMode::Tui,
            DisplayContext {
                interactive_io: false,
                term: Some("xterm-256color"),
            },
        ),
        OutputMode::Stream
    );

    let runner = NeverRunner;
    let clock = FixedClock;
    let cancellation = Cancellation::default();
    cancellation.request(15);
    let runtime = Runtime::new(
        RuntimeConfig::new(
            nix_tools_engine::EngineConfig::new(
                "nix",
                nix_tools_core::system::NixSystem::X86_64Linux,
            ),
            crate::AppExecutionPolicy::minimal(),
        ),
        RuntimeDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
        },
    );
    let error = runtime
        .execute(RuntimeCommand::Run {
            title: "lt run app".to_owned(),
            flake: FlakeRef::new(".", None),
            app: "app".to_owned(),
            arguments: vec![OsString::from("--flag")],
            output: OutputMode::Stream,
        })
        .unwrap_err();

    assert_eq!(error.kind, nix_tools_core::outcome::ErrorKind::Cancelled);
    assert_eq!(error.exit_code.get(), 143);
}

#[test]
fn selected_checks_enter_through_the_interactive_runtime_seam() {
    let runner = NeverRunner;
    let clock = FixedClock;
    let cancellation = Cancellation::default();
    cancellation.request(2);
    let runtime = Runtime::new(
        RuntimeConfig::new(
            nix_tools_engine::EngineConfig::new(
                "nix",
                nix_tools_core::system::NixSystem::X86_64Linux,
            ),
            crate::AppExecutionPolicy::minimal(),
        ),
        RuntimeDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
        },
    );

    let error = runtime
        .check_selected(SelectedCheckCommand {
            title: "lt check app".to_owned(),
            flake: FlakeRef::new(".", None),
            scope: "app".to_owned(),
            selector: &NeverSelector,
            output: OutputMode::Stream,
        })
        .unwrap_err();

    assert_eq!(error.kind, nix_tools_core::outcome::ErrorKind::Cancelled);
}

#[test]
fn settled_execution_returns_failed_manifest_without_validation() {
    let runner = FailingEvaluationRunner;
    let clock = FixedClock;
    let cancellation = Cancellation::default();
    let runtime = Runtime::new(
        RuntimeConfig::new(
            nix_tools_engine::EngineConfig::new(
                "nix",
                nix_tools_core::system::NixSystem::X86_64Linux,
            ),
            crate::AppExecutionPolicy::minimal(),
        ),
        RuntimeDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
        },
    );
    let command = RuntimeCommand::Build {
        title: "lt build".to_owned(),
        flake: FlakeRef::new(".", None),
        targets: Vec::new(),
        out_link: None,
        output: OutputMode::Stream,
    };

    let manifest = runtime.execute_settled(command.clone()).unwrap();
    let validated = runtime.execute(command).unwrap_err();

    assert_eq!(manifest.outcome, ManifestOutcome::Failed);
    assert_eq!(validated.kind, nix_tools_core::outcome::ErrorKind::Child);
}

#[test]
fn settled_execution_rejects_run_before_starting_a_process() {
    let runner = NeverRunner;
    let clock = FixedClock;
    let cancellation = Cancellation::default();
    let runtime = Runtime::new(
        RuntimeConfig::new(
            nix_tools_engine::EngineConfig::new(
                "nix",
                nix_tools_core::system::NixSystem::X86_64Linux,
            ),
            crate::AppExecutionPolicy::minimal(),
        ),
        RuntimeDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
        },
    );

    let error = runtime
        .execute_settled(RuntimeCommand::Run {
            title: "lt run app".to_owned(),
            flake: FlakeRef::new(".", None),
            app: "app".to_owned(),
            arguments: Vec::new(),
            output: OutputMode::Stream,
        })
        .unwrap_err();

    assert_eq!(error.kind, nix_tools_core::outcome::ErrorKind::Usage);
}

#[test]
fn empty_check_selection_is_rejected_without_running_all_checks() {
    let runner = DiscoveryOnlyRunner;
    let clock = FixedClock;
    let cancellation = Cancellation::default();
    let runtime = Runtime::new(
        RuntimeConfig::new(
            nix_tools_engine::EngineConfig::new(
                "nix",
                nix_tools_core::system::NixSystem::X86_64Linux,
            ),
            crate::AppExecutionPolicy::minimal(),
        ),
        RuntimeDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
        },
    );
    let error = runtime
        .check_selected(SelectedCheckCommand {
            title: "check empty scope".to_owned(),
            flake: FlakeRef::new(".", None),
            scope: "empty".to_owned(),
            selector: &EmptySelector,
            output: OutputMode::Stream,
        })
        .unwrap_err();
    assert_eq!(error.kind, nix_tools_core::outcome::ErrorKind::Usage);
    assert!(error.message.contains("no checks"));
}
