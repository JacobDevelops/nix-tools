use std::ffi::OsString;
use std::sync::Mutex;

use nix_tools_core::outcome::Result;
use nix_tools_core::process::{
    Cancellation, CapturedStream, ChildTermination, ProcessResult, ProcessRunner, ProcessSpec,
};
use nix_tools_engine::{
    BuildRequest, CheckRequest, DiscoveredTargets, EngineError, EngineRequest, EngineResponse,
    FlakeEngine, FlakeRef, Manifest, ManifestMetrics, ManifestOutcome, PreparedRun, RunRequest,
};

use super::{AppExecutionPolicy, AppOutputPolicy, CheckSelector, Flake, StandardCommands};

struct Engine {
    requests: Mutex<Vec<EngineRequest>>,
    outcome: ManifestOutcome,
}

impl Engine {
    fn successful() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            outcome: ManifestOutcome::Success,
        }
    }

    fn with_outcome(outcome: ManifestOutcome) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            outcome,
        }
    }
}

impl FlakeEngine for Engine {
    fn execute(&self, request: EngineRequest) -> std::result::Result<EngineResponse, EngineError> {
        self.requests.lock().unwrap().push(request.clone());
        match request {
            EngineRequest::Discover(_) => Ok(EngineResponse::Discovery(DiscoveredTargets {
                packages: vec!["web".into(), "worker".into()],
                checks: vec!["api-test".into(), "ui-test".into()],
                apps: vec!["serve".into()],
            })),
            EngineRequest::Build(_) | EngineRequest::Check(_) => {
                Ok(EngineResponse::Realization(manifest(self.outcome)))
            }
            EngineRequest::Run(request) => Ok(EngineResponse::PreparedRun(PreparedRun {
                program: "realized-app".into(),
                arguments: request.arguments,
                manifest: manifest(self.outcome),
            })),
        }
    }
}

fn manifest(outcome: ManifestOutcome) -> Manifest {
    Manifest {
        schema: "test",
        system: "test-system".into(),
        roots: Vec::new(),
        graph: Vec::new(),
        availability: Vec::new(),
        nodes: Vec::new(),
        diagnostics: Vec::new(),
        metrics: ManifestMetrics::default(),
        outcome,
    }
}

#[derive(Default)]
struct Runner(Mutex<Vec<ProcessSpec>>);

impl ProcessRunner for Runner {
    fn run(&self, spec: &ProcessSpec, _cancellation: &Cancellation) -> Result<ProcessResult> {
        self.0.lock().unwrap().push(spec.clone());
        Ok(ProcessResult {
            termination: ChildTermination::Exited(0),
            stdout: CapturedStream::default(),
            stderr: CapturedStream::default(),
            combined: None,
            duration: std::time::Duration::ZERO,
        })
    }
}

struct OnlyApi;

impl CheckSelector for OnlyApi {
    fn select(&self, _scope: &str, checks: &[String]) -> Result<Vec<String>> {
        Ok(checks
            .iter()
            .filter(|check| check.starts_with("api"))
            .cloned()
            .collect())
    }
}

struct CaptureOutput;

impl AppOutputPolicy for CaptureOutput {
    fn stdout(&self) -> nix_tools_core::process::StreamPolicy {
        nix_tools_core::process::StreamPolicy::Capture { limit: 7 }
    }

    fn stderr(&self) -> nix_tools_core::process::StreamPolicy {
        nix_tools_core::process::StreamPolicy::Capture { limit: 11 }
    }
}

#[test]
fn standard_commands_dispatches_engine_requests_and_executes_only_prepared_apps() {
    let engine = Engine::successful();
    let runner = Runner::default();
    let cancellation = Cancellation::default();
    let execution = AppExecutionPolicy::minimal()
        .with_cwd("/work")
        .with_environment("LANG", "C.UTF-8");
    let commands =
        StandardCommands::new(&engine, &runner, &cancellation, &CaptureOutput, &execution);
    let flake = Flake::new(".");

    commands.build(&flake, "web").unwrap();
    commands.check_selected(&flake, "api", &OnlyApi).unwrap();
    commands
        .run(
            &flake,
            "serve",
            &[OsString::from("--port"), OsString::from("3000")],
        )
        .unwrap();

    let requests = engine.requests.lock().unwrap().clone();
    assert_eq!(
        requests,
        vec![
            EngineRequest::Build(BuildRequest {
                flake: FlakeRef::new(".", None),
                targets: vec!["web".into()],
            }),
            EngineRequest::Discover(nix_tools_engine::DiscoverRequest {
                flake: FlakeRef::new(".", None),
            }),
            EngineRequest::Check(CheckRequest {
                flake: FlakeRef::new(".", None),
                targets: vec!["api-test".into()],
            }),
            EngineRequest::Run(RunRequest {
                flake: FlakeRef::new(".", None),
                app: "serve".into(),
                arguments: vec![OsString::from("--port"), OsString::from("3000")],
            }),
        ]
    );
    let specs = runner.0.lock().unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].program, OsString::from("realized-app"));
    assert_eq!(specs[0].args, vec!["--port", "3000"]);
    assert_eq!(specs[0].cwd.as_deref(), Some(std::path::Path::new("/work")));
    assert_eq!(
        specs[0].env.get(std::ffi::OsStr::new("LANG")),
        Some(&OsString::from("C.UTF-8"))
    );
    assert_eq!(
        specs[0].stdout,
        nix_tools_core::process::StreamPolicy::Capture { limit: 7 }
    );
    assert_eq!(
        specs[0].stderr,
        nix_tools_core::process::StreamPolicy::Capture { limit: 11 }
    );
}

#[test]
fn bulk_roots_remain_one_engine_request() {
    let engine = Engine::successful();
    let runner = Runner::default();
    let cancellation = Cancellation::default();
    let execution = AppExecutionPolicy::minimal();
    let commands =
        StandardCommands::new(&engine, &runner, &cancellation, &CaptureOutput, &execution);
    let flake = Flake::new(".");

    commands.build_all(&flake).unwrap();
    commands.check_all(&flake).unwrap();

    let requests = engine.requests.lock().unwrap().clone();
    assert!(matches!(
        &requests[1],
        EngineRequest::Build(BuildRequest { targets, .. }) if targets == &["web", "worker"]
    ));
    assert!(matches!(
        &requests[3],
        EngineRequest::Check(CheckRequest { targets, .. }) if targets == &["api-test", "ui-test"]
    ));
}

#[test]
fn failed_or_cancelled_manifests_are_structured_errors_not_successful_dispatches() {
    let runner = Runner::default();
    let cancellation = Cancellation::default();
    let failed = Engine::with_outcome(ManifestOutcome::Failed);
    let execution = AppExecutionPolicy::minimal();
    let failed_commands =
        StandardCommands::new(&failed, &runner, &cancellation, &CaptureOutput, &execution);
    assert_eq!(
        failed_commands
            .build(&Flake::new("."), "web")
            .unwrap_err()
            .kind,
        nix_tools_core::outcome::ErrorKind::Child
    );

    let cancelled = Engine::with_outcome(ManifestOutcome::Cancelled);
    cancellation.request(2);
    let cancelled_commands = StandardCommands::new(
        &cancelled,
        &runner,
        &cancellation,
        &CaptureOutput,
        &execution,
    );
    assert_eq!(
        cancelled_commands
            .build(&Flake::new("."), "web")
            .unwrap_err()
            .kind,
        nix_tools_core::outcome::ErrorKind::Cancelled
    );
}

#[test]
fn standard_commands_preserve_nonempty_nix_attribute_names() {
    let engine = Engine::successful();
    let runner = Runner::default();
    let cancellation = Cancellation::default();
    let execution = AppExecutionPolicy::minimal();
    let commands =
        StandardCommands::new(&engine, &runner, &cancellation, &CaptureOutput, &execution);

    commands.build(&Flake::new("."), "name with dot.π").unwrap();

    assert!(matches!(
        &engine.requests.lock().unwrap()[0],
        EngineRequest::Build(BuildRequest { targets, .. }) if targets == &["name with dot.π"]
    ));
}

#[test]
fn a_failed_prepared_run_does_not_execute_the_app() {
    let engine = Engine::with_outcome(ManifestOutcome::Failed);
    let runner = Runner::default();
    let cancellation = Cancellation::default();
    let execution = AppExecutionPolicy::minimal();
    let commands =
        StandardCommands::new(&engine, &runner, &cancellation, &CaptureOutput, &execution);

    let error = commands.run(&Flake::new("."), "serve", &[]).unwrap_err();

    assert_eq!(error.kind, nix_tools_core::outcome::ErrorKind::Child);
    assert!(runner.0.lock().unwrap().is_empty());
}
