use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use nix_tools_engine::{
    DerivationNode, Manifest, ManifestMetrics, ManifestOutcome, NodeResult, NodeState, Phase,
    ProgressEvent, RootResult, TargetKind,
};

use super::model::{JobFilter, JobStatus, Model, PhaseStatus};

fn node(path: &str, dependencies: &[&str]) -> Arc<DerivationNode> {
    Arc::new(DerivationNode {
        drv_path: path.to_owned(),
        dependencies: dependencies
            .iter()
            .map(|dependency| ((*dependency).to_owned(), BTreeSet::from(["out".to_owned()])))
            .collect::<BTreeMap<_, _>>(),
        outputs: BTreeMap::from([("out".to_owned(), None)]),
    })
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
    assert!(model.jobs()[0].relationships_known);
    assert!(model.jobs()[1].relationships_known);
}

#[test]
fn incomplete_graphs_reveal_live_transitive_jobs_without_inventing_dependencies() {
    let root = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-root.drv";
    let dependency = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-shared.drv";
    let mut model = Model::new("check");
    model.apply(ProgressEvent::GraphDiscovered(vec![node(root, &[])]));

    model.apply(ProgressEvent::NodeStarted {
        drv_path: dependency.to_owned(),
    });
    model.apply(ProgressEvent::NodeLogLine {
        drv_path: dependency.to_owned(),
        line: "compiling shared crate".to_owned(),
    });

    assert_eq!(model.jobs().len(), 2);
    assert!(model.jobs()[0].relationships_known);
    assert!(!model.jobs()[1].relationships_known);
    assert_eq!(model.jobs()[1].status, JobStatus::Running);
    assert_eq!(
        model.jobs()[1].logs.back().map(String::as_str),
        Some("compiling shared crate")
    );
}

#[test]
fn root_only_completion_settles_provisional_transitive_builds() {
    let root = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-root.drv";
    let dependency = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-shared.drv";
    let mut model = Model::new("check");
    model.apply(ProgressEvent::GraphDiscovered(vec![node(root, &[])]));
    model.apply(ProgressEvent::NodeStarted {
        drv_path: dependency.to_owned(),
    });
    model.apply(ProgressEvent::NodeActivityStopped {
        drv_path: dependency.to_owned(),
    });
    model.apply(ProgressEvent::NodeProvisionalFinished {
        drv_path: dependency.to_owned(),
        state: NodeState::Built,
    });
    let manifest = Manifest {
        schema: "nix-tools.manifest/v1",
        system: "x86_64-linux".to_owned(),
        roots: vec![RootResult {
            kind: TargetKind::Check,
            name: "root".to_owned(),
            drv_path: Some(root.to_owned()),
            outputs: BTreeMap::new(),
            state: NodeState::Cached,
        }],
        graph: vec![node(root, &[])],
        availability: Vec::new(),
        nodes: vec![NodeResult {
            drv_path: root.to_owned(),
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
    model.set_job_filter(JobFilter::Completed);

    assert_eq!(model.jobs()[1].status, JobStatus::Settled(NodeState::Built));
    assert_eq!(model.settled(), 2);
    assert_eq!(model.visible_job_indices(), [0, 1]);
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
fn text_filter_matches_labels_case_insensitively_and_navigation_stays_visible() {
    let mut model = Model::new("check");
    model.apply(ProgressEvent::GraphDiscovered(vec![
        node(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-api-unit.drv",
            &[],
        ),
        node(
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-web-unit.drv",
            &[],
        ),
        node(
            "/nix/store/cccccccccccccccccccccccccccccccc-api-lint.drv",
            &[],
        ),
    ]));

    model.start_filter_input();
    for character in "API".chars() {
        model.push_filter_character(character);
    }

    assert_eq!(model.visible_job_indices(), vec![0, 2]);
    assert_eq!(model.selected(), Some(0));
    model.select_last();
    assert_eq!(model.selected(), Some(2));
    assert_eq!(model.selected_visible(), Some(1));
    model.select_first();
    assert_eq!(model.selected(), Some(0));
    model.select_previous();
    assert_eq!(model.selected(), Some(2));
    model.select_next();
    assert_eq!(model.selected(), Some(0));
}

#[test]
fn status_filters_distinguish_active_waiting_queued_completed_and_failed_jobs() {
    let paths = [
        "queued",
        "running",
        "waiting",
        "provisional",
        "built",
        "failed",
    ];
    let mut model = Model::new("check");
    model.apply(ProgressEvent::GraphDiscovered(
        paths.iter().map(|path| node(path, &[])).collect(),
    ));
    model.apply(ProgressEvent::NodeStarted {
        drv_path: "running".to_owned(),
    });
    model.apply(ProgressEvent::NodeStarted {
        drv_path: "waiting".to_owned(),
    });
    model.apply(ProgressEvent::NodeActivityStopped {
        drv_path: "waiting".to_owned(),
    });
    model.apply(ProgressEvent::NodeProvisionalFinished {
        drv_path: "provisional".to_owned(),
        state: NodeState::Built,
    });
    model.apply(ProgressEvent::NodeFinished {
        drv_path: "built".to_owned(),
        state: NodeState::Built,
    });
    model.apply(ProgressEvent::NodeFinished {
        drv_path: "failed".to_owned(),
        state: NodeState::Failed,
    });

    model.set_job_filter(JobFilter::Active);
    assert_eq!(model.visible_job_indices(), vec![1]);
    model.set_job_filter(JobFilter::Waiting);
    assert_eq!(model.visible_job_indices(), vec![2, 3]);
    model.set_job_filter(JobFilter::Queued);
    assert_eq!(model.visible_job_indices(), vec![0]);
    model.set_job_filter(JobFilter::Completed);
    assert_eq!(model.visible_job_indices(), vec![4, 5]);
    model.set_job_filter(JobFilter::Failed);
    assert_eq!(model.visible_job_indices(), vec![5]);
}

#[test]
fn active_filter_updates_incrementally_as_jobs_transition() {
    let mut model = Model::new("check");
    model.apply(ProgressEvent::GraphDiscovered(vec![
        node("first", &[]),
        node("second", &[]),
    ]));
    model.set_job_filter(JobFilter::Active);
    assert!(model.visible_job_indices().is_empty());

    model.apply(ProgressEvent::NodeStarted {
        drv_path: "second".to_owned(),
    });
    assert_eq!(model.visible_job_indices(), [1]);
    assert_eq!(model.selected(), Some(1));
    model.apply(ProgressEvent::NodeStarted {
        drv_path: "first".to_owned(),
    });
    assert_eq!(model.visible_job_indices(), [0, 1]);
    assert_eq!(model.selected(), Some(1));

    model.apply(ProgressEvent::NodeFinished {
        drv_path: "second".to_owned(),
        state: NodeState::Built,
    });
    assert_eq!(model.visible_job_indices(), [0]);
    assert_eq!(model.selected(), Some(0));
}

#[test]
fn clearing_filters_restores_every_job_and_a_selection() {
    let mut model = Model::new("check");
    model.apply(ProgressEvent::GraphDiscovered(vec![
        node("api", &[]),
        node("web", &[]),
    ]));
    model.start_filter_input();
    for character in "missing".chars() {
        model.push_filter_character(character);
    }
    model.set_job_filter(JobFilter::Failed);
    assert!(model.visible_job_indices().is_empty());
    assert_eq!(model.selected(), None);

    model.clear_filters();

    assert_eq!(model.visible_job_indices(), vec![0, 1]);
    assert_eq!(model.selected(), Some(0));
    assert_eq!(model.job_filter(), JobFilter::All);
    assert_eq!(model.filter_query(), "");
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
fn activity_timing_excludes_waiting_for_other_jobs_and_resumes_on_retry() {
    let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv";
    let mut model = Model::fixed("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node(path, &[])]));
    model.apply(ProgressEvent::NodeStarted {
        drv_path: path.to_owned(),
    });
    model.advance(Duration::from_secs(1));
    model.apply(ProgressEvent::NodeActivityStopped {
        drv_path: path.to_owned(),
    });
    model.advance(Duration::from_mins(10));
    assert_eq!(
        model.jobs()[0].elapsed(model.now()),
        Some(Duration::from_secs(1))
    );
    assert_eq!(model.jobs()[0].status, JobStatus::AwaitingResult);
    assert_eq!(model.settled(), 0);
    model.apply(ProgressEvent::NodeStarted {
        drv_path: path.to_owned(),
    });
    model.advance(Duration::from_secs(2));
    assert_eq!(
        model.jobs()[0].elapsed(model.now()),
        Some(Duration::from_secs(3))
    );
    model.apply(ProgressEvent::NodeActivityStopped {
        drv_path: path.to_owned(),
    });
    model.advance(Duration::from_mins(10));
    model.apply(ProgressEvent::NodeFinished {
        drv_path: path.to_owned(),
        state: NodeState::Built,
    });
    assert_eq!(
        model.jobs()[0].elapsed(model.now()),
        Some(Duration::from_secs(3))
    );
}

#[test]
fn running_jobs_are_timed_and_settled_jobs_keep_their_duration() {
    let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv";
    let mut model = Model::fixed("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node(path, &[])]));

    assert!(model.jobs()[0].elapsed(model.now()).is_none());

    model.apply(ProgressEvent::NodeStarted {
        drv_path: path.to_owned(),
    });
    model.advance(Duration::from_millis(1_500));
    assert_eq!(
        model.jobs()[0].elapsed(model.now()),
        Some(Duration::from_millis(1_500))
    );

    model.apply(ProgressEvent::NodeFinished {
        drv_path: path.to_owned(),
        state: NodeState::Built,
    });
    model.advance(Duration::from_millis(500));
    assert_eq!(
        model.jobs()[0].elapsed(model.now()),
        Some(Duration::from_millis(1_500)),
        "a settled job keeps the duration it took"
    );
    assert_eq!(model.settled(), 1);
}

#[test]
fn a_node_nix_never_reported_settles_without_a_duration() {
    let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv";
    let mut model = Model::fixed("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node(path, &[])]));

    model.apply(ProgressEvent::NodeFinished {
        drv_path: path.to_owned(),
        state: NodeState::Cached,
    });

    assert!(model.jobs()[0].elapsed(model.now()).is_none());
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

#[test]
fn live_logs_keep_last_lines_and_preserve_scrolled_position() {
    let mut model = Model::fixed("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node("a", &[])]));
    for index in 0..1_010 {
        model.apply(ProgressEvent::NodeLogLine {
            drv_path: "a".to_owned(),
            line: index.to_string(),
        });
    }
    assert_eq!(model.jobs()[0].logs.len(), 1_000);
    assert_eq!(model.jobs()[0].logs.front().map(String::as_str), Some("10"));
    model.scroll_logs(10);
    model.apply(ProgressEvent::NodeLogLine {
        drv_path: "a".to_owned(),
        line: "new".to_owned(),
    });
    assert_eq!(model.jobs()[0].log_scroll, 11);
    model.follow_logs();
    assert_eq!(model.jobs()[0].log_scroll, 0);
}

#[test]
fn provisional_outcome_can_resume_and_final_result_corrects_it() {
    let mut model = Model::fixed("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node("a", &[])]));
    model.apply(ProgressEvent::NodeStarted {
        drv_path: "a".to_owned(),
    });
    model.advance(Duration::from_secs(2));
    model.apply(ProgressEvent::NodeProvisionalFinished {
        drv_path: "a".to_owned(),
        state: NodeState::Built,
    });
    assert_eq!(
        model.jobs()[0].status,
        JobStatus::Provisional(NodeState::Built)
    );
    model.advance(Duration::from_secs(5));
    model.apply(ProgressEvent::NodeStarted {
        drv_path: "a".to_owned(),
    });
    model.advance(Duration::from_secs(1));
    model.apply(ProgressEvent::NodeFinished {
        drv_path: "a".to_owned(),
        state: NodeState::Failed,
    });
    assert_eq!(
        model.jobs()[0].elapsed(model.now()),
        Some(Duration::from_secs(3))
    );
    assert_eq!(
        model.jobs()[0].status,
        JobStatus::Settled(NodeState::Failed)
    );
}

#[test]
fn expanded_graph_preserves_cached_and_running_jobs_and_selection() {
    let mut model = Model::fixed("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![
        node("b", &[]),
        node("c", &[]),
    ]));
    model.apply(ProgressEvent::NodeFinished {
        drv_path: "b".to_owned(),
        state: NodeState::Cached,
    });
    model.apply(ProgressEvent::NodeStarted {
        drv_path: "c".to_owned(),
    });
    model.apply(ProgressEvent::NodeLogLine {
        drv_path: "c".to_owned(),
        line: "compile".to_owned(),
    });
    model.select_next();
    model.advance(Duration::from_secs(2));
    model.apply(ProgressEvent::GraphDiscovered(vec![
        node("a", &[]),
        node("b", &["a"]),
        node("c", &["b"]),
    ]));
    assert_eq!(
        model.jobs()[1].status,
        JobStatus::Settled(NodeState::Cached)
    );
    assert_eq!(model.jobs()[2].status, JobStatus::Running);
    assert_eq!(
        model.jobs()[2].elapsed(model.now()),
        Some(Duration::from_secs(2))
    );
    assert_eq!(
        model.jobs()[2].logs.front().map(String::as_str),
        Some("compile")
    );
    assert_eq!(model.selected(), Some(2));
    assert_eq!(model.jobs()[1].dependencies, vec![0]);
}
