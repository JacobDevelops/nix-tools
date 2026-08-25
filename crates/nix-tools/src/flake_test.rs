use std::ffi::OsString;
use std::sync::Mutex;

use nix_tools_core::outcome::{Error, Result};
use nix_tools_core::process::{
    Cancellation, CapturedStream, ChildTermination, ProcessResult, ProcessRunner, ProcessSpec,
};
use nix_tools_core::system::NixSystem;

use super::{Flake, FlakeOperations, NixTools, StandardFlake};

#[derive(Default)]
struct Runner {
    specs: Mutex<Vec<ProcessSpec>>,
    responses: Mutex<Vec<std::result::Result<ProcessResult, Error>>>,
}

impl Runner {
    fn with_json(values: &[&str]) -> Self {
        Self {
            specs: Mutex::new(Vec::new()),
            responses: Mutex::new(values.iter().map(|value| Ok(result(value))).collect()),
        }
    }

    fn specs(&self) -> Vec<ProcessSpec> {
        self.specs.lock().unwrap().clone()
    }
}

impl ProcessRunner for Runner {
    fn run(&self, spec: &ProcessSpec, _cancellation: &Cancellation) -> Result<ProcessResult> {
        self.specs.lock().unwrap().push(spec.clone());
        self.responses.lock().unwrap().remove(0)
    }
}

fn result(stdout: &str) -> ProcessResult {
    ProcessResult {
        termination: ChildTermination::Exited(0),
        stdout: CapturedStream {
            bytes: stdout.as_bytes().to_vec(),
            truncated: false,
        },
        stderr: CapturedStream::default(),
        combined: None,
        duration: std::time::Duration::ZERO,
    }
}

fn tools(runner: &Runner) -> NixTools<'_> {
    NixTools::new(
        runner,
        OsString::from("nix"),
        NixSystem::Aarch64Darwin,
        4096,
    )
}

#[test]
fn discovery_uses_bounded_attr_name_evaluations_for_the_selected_system() {
    let runner = Runner::with_json(&[
        "[\"web\",\"api\",\"name with dot.π\"]",
        "[\"lint\"]",
        "[\"serve\"]",
    ]);
    let flake = Flake::new(".");

    let discovered = tools(&runner)
        .discover(&flake, &Cancellation::default())
        .unwrap();

    assert_eq!(discovered.packages, vec!["api", "name with dot.π", "web"]);
    assert_eq!(discovered.checks, vec!["lint"]);
    assert_eq!(discovered.apps, vec!["serve"]);
    let specs = runner.specs();
    assert_eq!(specs.len(), 3);
    assert!(specs.iter().all(|spec| spec.program == "nix"));
    assert!(
        specs
            .iter()
            .all(|spec| spec.args.iter().any(|argument| argument == "--json"))
    );
    assert!(
        specs
            .iter()
            .all(|spec| spec.args.iter().any(|argument| argument == "--apply"))
    );
    assert_eq!(specs[0].args.last().unwrap(), "builtins.attrNames");
    assert!(
        specs[0]
            .args
            .iter()
            .any(|argument| argument == ".#packages.aarch64-darwin")
    );
}

#[test]
fn malformed_evaluation_is_a_structured_external_error() {
    let runner = Runner::with_json(&["not json"]);

    let error = tools(&runner)
        .discover_packages(&Flake::new("."), &Cancellation::default())
        .unwrap_err();

    assert_eq!(error.kind, nix_tools_core::outcome::ErrorKind::External);
}

#[test]
fn standard_flake_names_are_composable_typed_requests() {
    let request = StandardFlake::package("hello").for_system(NixSystem::X86_64Linux);
    assert_eq!(request.attribute_path(), "packages.x86_64-linux.\"hello\"");
    assert_eq!(
        request.target_for(&Flake::new(".")),
        ".#packages.x86_64-linux.\"hello\""
    );
}

#[test]
fn standard_flake_quotes_dynamic_attribute_names() {
    let system = NixSystem::X86_64Linux;
    let cases = [
        ("name.with.dot", "packages.x86_64-linux.\"name.with.dot\""),
        (
            "name with space",
            "packages.x86_64-linux.\"name with space\"",
        ),
        (
            "say \"hello\"",
            "packages.x86_64-linux.\"say \\\"hello\\\"\"",
        ),
        (
            "value${interpolation}",
            "packages.x86_64-linux.\"value\\${interpolation}\"",
        ),
        ("π", "packages.x86_64-linux.\"π\""),
    ];

    for (name, expected) in cases {
        assert_eq!(
            StandardFlake::package(name)
                .for_system(system)
                .attribute_path(),
            expected
        );
    }
}
