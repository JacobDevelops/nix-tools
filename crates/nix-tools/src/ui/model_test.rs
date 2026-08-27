use std::collections::{BTreeMap, BTreeSet};

use nix_tools_engine::{
    DerivationNode, Manifest, ManifestMetrics, ManifestOutcome, NodeResult, NodeState, Phase,
    ProgressEvent, RootResult, TargetKind,
};

use super::model::{JobStatus, Model, PhaseStatus};

fn node(path: &str, dependencies: &[&str]) -> DerivationNode {
    DerivationNode {
        drv_path: path.to_owned(),
        dependencies: dependencies
            .iter()
            .map(|dependency| ((*dependency).to_owned(), BTreeSet::from(["out".to_owned()])))
            .collect::<BTreeMap<_, _>>(),
        outputs: BTreeMap::from([("out".to_owned(), None)]),
    }
}

#[test]
fn graph_events_build_a_dependency_map_with_readable_labels() {
    let mut model = Model::new("check");

    model.apply(ProgressEvent::GraphDiscovered(vec![
        node("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv", &[]),
        node(
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-cli.drv",
            &["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv"],
        ),
    ]));

    assert_eq!(model.jobs().len(), 2);
    assert_eq!(model.jobs()[0].label, "core");
    assert_eq!(model.jobs()[1].label, "cli");
    assert_eq!(model.jobs()[1].dependencies, vec![0]);
    assert_eq!(model.jobs()[1].status, JobStatus::Queued);
}

#[test]
fn phase_and_job_transitions_are_reduced_without_terminal_state() {
    let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv";
    let mut model = Model::new("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node(path, &[])]));

    model.apply(ProgressEvent::PhaseStarted(Phase::Realization));
    model.apply(ProgressEvent::NodeStarted {
        drv_path: path.to_owned(),
    });
    assert_eq!(model.phase(Phase::Realization), PhaseStatus::Active);
    assert_eq!(model.jobs()[0].status, JobStatus::Running);

    model.apply(ProgressEvent::NodeFinished {
        drv_path: path.to_owned(),
        state: NodeState::Built,
    });
    model.apply(ProgressEvent::PhaseFinished(Phase::Realization));
    assert_eq!(model.phase(Phase::Realization), PhaseStatus::Complete);
    assert_eq!(model.jobs()[0].status, JobStatus::Settled(NodeState::Built));
}

#[test]
fn selection_wraps_and_dependency_focus_is_stable() {
    let mut model = Model::new("check");
    model.apply(ProgressEvent::GraphDiscovered(vec![
        node("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv", &[]),
        node(
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-cli.drv",
            &["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv"],
        ),
        node("/nix/store/cccccccccccccccccccccccccccccccc-web.drv", &[]),
    ]));

    model.select_previous();
    assert_eq!(model.selected(), Some(2));
    assert!(model.focused_dependencies().is_empty());
    model.select_next();
    assert_eq!(model.selected(), Some(0));
}

#[test]
fn final_manifest_populates_fast_cached_runs_that_emitted_no_graph() {
    let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv";
    let mut model = Model::new("check");
    let manifest = Manifest {
        schema: "nix-tools.manifest/v1",
        system: "x86_64-linux".to_owned(),
        roots: vec![RootResult {
            kind: TargetKind::Check,
            name: "framework-eval".to_owned(),
            drv_path: Some(path.to_owned()),
            outputs: BTreeMap::new(),
            state: NodeState::Cached,
        }],
        graph: vec![node(path, &[])],
        availability: Vec::new(),
        nodes: vec![NodeResult {
            drv_path: path.to_owned(),
            dependencies: Vec::new(),
            required_outputs: BTreeSet::from(["out".to_owned()]),
            produced_paths: Vec::new(),
            state: NodeState::Cached,
            dependency_failure: None,
        }],
        diagnostics: Vec::new(),
        metrics: ManifestMetrics::default(),
        outcome: ManifestOutcome::Success,
    };

    model.finish(&manifest);

    assert_eq!(model.jobs().len(), 1);
    assert_eq!(model.jobs()[0].label, "framework-eval");
    assert_eq!(
        model.jobs()[0].status,
        JobStatus::Settled(NodeState::Cached)
    );
    assert!(model.finished());
}

#[test]
fn help_is_an_explicit_toggle_in_the_ui_model() {
    let mut model = Model::new("check");

    assert!(!model.help_visible());
    model.toggle_help();
    assert!(model.help_visible());
    model.toggle_help();
    assert!(!model.help_visible());
}
