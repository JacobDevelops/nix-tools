//! Profiles a cached 10,000-root realization, including the captured path-info response.

use std::error::Error;
use std::ffi::OsStr;
use std::time::{Duration, Instant};

use nix_tools_core::process::{
    Cancellation, CapturedStream, ChildTermination, ProcessResult, ProcessRunner, ProcessSpec,
};
use nix_tools_core::system::NixSystem;
use nix_tools_engine::{
    BuildRequest, EngineConfig, EngineDependencies, FlakeRef, GraphMode, ManifestOutcome,
    NixEngine, NoProgress, ResourceLimits, SystemClock,
};
use serde_json::json;

struct FixtureRunner {
    evaluation: Vec<u8>,
    probe: Vec<u8>,
}

impl ProcessRunner for FixtureRunner {
    fn run(
        &self,
        spec: &ProcessSpec,
        _cancellation: &Cancellation,
    ) -> nix_tools_core::outcome::Result<ProcessResult> {
        let payload = if spec.args.iter().any(|arg| arg == OsStr::new("eval")) {
            &self.evaluation
        } else {
            assert!(spec.args.iter().any(|arg| arg == OsStr::new("path-info")));
            &self.probe
        };
        Ok(ProcessResult {
            termination: ChildTermination::Exited(0),
            stdout: CapturedStream {
                bytes: payload.clone(),
                truncated: false,
            },
            stderr: CapturedStream::default(),
            combined: None,
            duration: Duration::from_millis(5),
        })
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let count = 10_000;
    let targets = (0..count)
        .map(|index| format!("pkg-{index}"))
        .collect::<Vec<_>>();
    let output = |name: &str| format!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-{name}");
    let evaluation = targets.iter().map(|name| json!({
        "success": true,
        "value": {"drvPath": format!("{}.drv", output(name)), "outputs": {"out": output(name)}, "outputsToInstall": ["out"]}
    })).collect::<Vec<_>>();
    let probe = targets
        .iter()
        .map(|name| (output(name), json!({"narSize": 10})))
        .collect::<serde_json::Map<_, _>>();
    let runner = FixtureRunner {
        evaluation: serde_json::to_vec(&evaluation)?,
        probe: serde_json::to_vec(&probe)?,
    };
    let cancellation = Cancellation::default();
    let engine = NixEngine::new(
        EngineConfig {
            nix_executable: "fixture-nix".into(),
            system: NixSystem::X86_64Linux,
            trusted_substituters: Vec::new(),
            graph_mode: GraphMode::Automatic,
            limits: ResourceLimits {
                max_roots: count,
                evaluation_batch_size: count,
                ..ResourceLimits::default()
            },
        },
        EngineDependencies {
            runner: &runner,
            cancellation: &cancellation,
            clock: &SystemClock,
            progress: &NoProgress,
        },
    )?;
    let started = Instant::now();
    for _ in 0..10 {
        let manifest = engine.build(BuildRequest {
            flake: FlakeRef::new(".", None),
            targets: targets.clone(),
            out_link: None,
        })?;
        assert_eq!(manifest.outcome, ManifestOutcome::Success);
        assert_eq!(manifest.metrics.probe.processes, 1);
        std::hint::black_box(manifest);
    }
    println!(
        "{{\"roots\":{count},\"iterations\":10,\"probe_bytes\":{},\"wall_ns\":{}}}",
        runner.probe.len(),
        started.elapsed().as_nanos()
    );
    Ok(())
}
