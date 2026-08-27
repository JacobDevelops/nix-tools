use nix_tools_core::process::{Cancellation, ProcessResult, ProcessRunner, ProcessSpec};
use nix_tools_core::system::NixSystem;
use nix_tools_engine::{
    Diagnostic, DiagnosticSeverity, DiscoverRequest, EngineConfig, EngineDependencies, FlakeRef,
    Manifest, ManifestMetrics, ManifestOutcome, NixEngine, NoProgress, Phase, SystemClock,
};

use nix_tools::manifest_result;

use clap::Parser;

use super::{Cli, OutputMode, engine_error, select_checks, trusted_substituters};

struct NeverRunner;

#[test]
fn output_defaults_to_tui_and_accepts_only_stream_or_tui() {
    let default = Cli::try_parse_from(["nix-tools", "check"]).unwrap();
    assert_eq!(default.command.output(), Some(OutputMode::Tui));

    let tui = Cli::try_parse_from(["nix-tools", "check", "--output=tui"]).unwrap();
    assert_eq!(tui.command.output(), Some(OutputMode::Tui));

    assert!(Cli::try_parse_from(["nix-tools", "check", "--output=json"]).is_err());
    assert!(Cli::try_parse_from(["nix-tools", "check", "--no-tui"]).is_err());
}

#[test]
fn plan_rejects_tui_because_its_output_is_always_json() {
    assert!(Cli::try_parse_from(["nix-tools", "plan", "missing.json", "--output=tui"]).is_err());
}

impl ProcessRunner for NeverRunner {
    fn run(
        &self,
        _spec: &ProcessSpec,
        _cancellation: &Cancellation,
    ) -> nix_tools_core::outcome::Result<ProcessResult> {
        panic!("pre-cancelled engine must not run a process")
    }
}

#[test]
fn selector_supports_scope_and_scope_job_without_repository_policy() {
    let checks = vec![
        "api-unit".into(),
        "api-integration".into(),
        "web-unit".into(),
    ];

    assert_eq!(
        select_checks(checks.clone(), Some("api")).unwrap(),
        vec!["api-unit", "api-integration"]
    );
    assert_eq!(
        select_checks(checks.clone(), Some("api:unit")).unwrap(),
        vec!["api-unit"]
    );
    assert_eq!(
        select_checks(checks, Some("missing")).unwrap_err().kind,
        nix_tools_core::outcome::ErrorKind::NotFound
    );
}

#[test]
fn reference_cache_is_explicit_and_extra_caches_need_matching_keys() {
    let caches = trusted_substituters(
        vec!["https://cache.example".into()],
        vec!["example-1:key".into()],
    )
    .unwrap();

    assert_eq!(caches[0].url, "https://cache.nixos.org");
    assert_eq!(caches[1].url, "https://cache.example");
    assert!(trusted_substituters(vec!["https://cache.example".into()], Vec::new()).is_err());
}

#[test]
fn a_failed_manifest_cannot_be_reported_as_cli_success() {
    let mut manifest = manifest(ManifestOutcome::Failed);
    manifest.diagnostics.push(Diagnostic {
        phase: Phase::Realization,
        code: "build_failed".to_owned(),
        severity: DiagnosticSeverity::Error,
        target: None,
        message: "the selected build failed".to_owned(),
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
    });

    let error = manifest_result(&manifest, "build", &Cancellation::default()).unwrap_err();

    assert_eq!(error.kind, nix_tools_core::outcome::ErrorKind::Child);
    assert_eq!(error.message, "the selected build failed");
}

#[test]
fn fatal_engine_cancellation_preserves_the_requested_signal() {
    let cancellation = Cancellation::default();
    cancellation.request(15);
    let runner = NeverRunner;
    let clock = SystemClock;
    let progress = NoProgress;
    let engine = NixEngine::new(
        EngineConfig::new("nix", NixSystem::X86_64Linux),
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &clock,
            progress: &progress,
        },
    )
    .unwrap();

    let source_error = engine
        .discover(&DiscoverRequest {
            flake: FlakeRef::new(".", None),
        })
        .unwrap_err();
    let error = engine_error(&source_error, &cancellation);

    assert_eq!(error.kind, nix_tools_core::outcome::ErrorKind::Cancelled);
    assert_eq!(error.exit_code.get(), 143);
}

fn manifest(outcome: ManifestOutcome) -> Manifest {
    Manifest {
        schema: "nix-tools.manifest/v1",
        system: "x86_64-linux".to_owned(),
        roots: Vec::new(),
        graph: Vec::new(),
        availability: Vec::new(),
        nodes: Vec::new(),
        diagnostics: Vec::new(),
        metrics: ManifestMetrics::default(),
        outcome,
    }
}
