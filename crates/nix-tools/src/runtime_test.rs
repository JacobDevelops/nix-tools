use std::ffi::OsString;
use std::time::Duration;

use nix_tools_core::process::{
    Cancellation, CapturedStream, ChildTermination, ProcessResult, ProcessRunner, ProcessSpec,
};
use nix_tools_engine::{Clock, FlakeRef, ManifestOutcome};

use crate::{
    CheckSelector, DisplayContext, OutputMode, Runtime, RuntimeCommand, RuntimeConfig,
    RuntimeDependencies, SelectedCheckCommand,
};

struct NeverRunner;

struct FailingEvaluationRunner;

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
