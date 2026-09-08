use nix_tools_engine::{
    Diagnostic, DiagnosticSeverity, Manifest, ManifestMetrics, ManifestOutcome, Phase,
};

use nix_tools::manifest_result;
use nix_tools_core::process::Cancellation;

use clap::Parser;

use super::{Cli, CliOutputMode, trusted_substituters};
use nix_tools::{CheckSelector, ServiceCheckSelector};

#[test]
fn output_defaults_to_tui_and_accepts_only_stream_or_tui() {
    let default = Cli::try_parse_from(["nix-tools", "check"]).unwrap();
    assert_eq!(default.command.output(), Some(CliOutputMode::Tui));

    let tui = Cli::try_parse_from(["nix-tools", "check", "--output=tui"]).unwrap();
    assert_eq!(tui.command.output(), Some(CliOutputMode::Tui));

    assert!(Cli::try_parse_from(["nix-tools", "check", "--output=json"]).is_err());
    assert!(Cli::try_parse_from(["nix-tools", "check", "--no-tui"]).is_err());
}

#[test]
fn plan_rejects_tui_because_its_output_is_always_json() {
    assert!(Cli::try_parse_from(["nix-tools", "plan", "missing.json", "--output=tui"]).is_err());
}

#[test]
fn selector_supports_scope_and_scope_job_without_repository_policy() {
    let checks = vec![
        "api:unit".into(),
        "api:integration".into(),
        "web:unit".into(),
    ];

    assert_eq!(
        ServiceCheckSelector.select("api", &checks).unwrap(),
        vec!["api:integration", "api:unit"]
    );
    assert_eq!(
        ServiceCheckSelector.select("api:unit", &checks).unwrap(),
        vec!["api:unit"]
    );
    assert_eq!(
        ServiceCheckSelector
            .select("missing", &checks)
            .unwrap_err()
            .kind,
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

#[test]
fn run_defaults_to_exec_and_supervision_is_explicit() {
    let parsed = Cli::try_parse_from(["nix-tools", "run", "app:dev"]).unwrap();
    assert!(matches!(
        parsed.command,
        super::Command::Run {
            supervise: false,
            ..
        }
    ));
    let parsed =
        Cli::try_parse_from(["nix-tools", "run", "--supervise", "app:dev", "--", "--raw"]).unwrap();
    assert!(
        matches!(parsed.command, super::Command::Run { supervise: true, args, .. } if args == [std::ffi::OsString::from("--raw")])
    );
}

#[cfg(unix)]
#[test]
fn run_accepts_non_utf8_app_arguments() {
    use std::os::unix::ffi::OsStringExt;
    let argument = std::ffi::OsString::from_vec(vec![0xff]);
    let mut arguments: Vec<std::ffi::OsString> = ["nix-tools", "run", "app:dev", "--"]
        .into_iter()
        .map(Into::into)
        .collect();
    arguments.push(argument.clone());
    let parsed = Cli::try_parse_from(arguments).unwrap();
    assert!(matches!(parsed.command, super::Command::Run { args, .. } if args == [argument]));
}

#[test]
fn run_rejects_missing_and_malformed_targets_before_nix() {
    for target in ["web", "", ":dev", "web:", "web:dev:extra"] {
        assert!(Cli::try_parse_from(["nix-tools", "run", target]).is_err());
    }
}
