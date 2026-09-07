use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

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

#[test]
fn running_jobs_are_timed_and_settled_jobs_keep_their_duration() {
    let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv";
    let mut model = Model::new("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node(path, &[])]));
    let now = Instant::now();

    assert!(model.jobs()[0].elapsed(now).is_none());

    model.apply(ProgressEvent::NodeStarted {
        drv_path: path.to_owned(),
    });
    assert!(model.jobs()[0].elapsed(Instant::now()).is_some());

    model.apply(ProgressEvent::NodeFinished {
        drv_path: path.to_owned(),
        state: NodeState::Built,
    });
    let settled = model.jobs()[0].settled.expect("settled duration");
    assert_eq!(model.jobs()[0].elapsed(Instant::now()), Some(settled));
    assert_eq!(model.settled(), 1);
}

#[test]
fn a_node_nix_never_reported_settles_without_a_duration() {
    let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv";
    let mut model = Model::new("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node(path, &[])]));

    model.apply(ProgressEvent::NodeFinished {
        drv_path: path.to_owned(),
        state: NodeState::Cached,
    });

    assert!(model.jobs()[0].elapsed(Instant::now()).is_none());
}

#[test]
fn dependents_and_transfer_progress_are_recorded_for_the_detail_pane() {
    let core = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv";
    let cli = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-cli.drv";
    let mut model = Model::new("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![
        node(core, &[]),
        node(cli, &[core]),
    ]));

    model.apply(ProgressEvent::NodeProgress {
        drv_path: core.to_owned(),
        done: 512,
        expected: 2048,
    });

    assert_eq!(model.jobs()[0].dependents, vec![1]);
    assert!(model.jobs()[1].dependents.is_empty());
    assert_eq!(model.jobs()[0].progress, Some((512, 2048)));
}
