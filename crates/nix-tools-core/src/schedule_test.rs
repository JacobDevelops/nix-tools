use std::collections::{BTreeMap, BTreeSet};

use super::{
    PlanTarget, PredictionSource, ScheduleConfig, ScheduleError, Strategy, observe, schedule,
};
use crate::history::History;

const NOW: u64 = 1_800_000_000_000;

fn target(name: &str, dependencies: &[&str], transfer_bytes: u64, roots: &[&str]) -> PlanTarget {
    PlanTarget {
        id: name.to_owned(),
        history_key: roots
            .first()
            .map_or_else(|| format!("derivation.{name}"), |root| (*root).to_owned()),
        dependencies: dependencies.iter().map(|name| (*name).to_owned()).collect(),
        needs_work: true,
        transfer_bytes,
        roots: roots.iter().map(|root| (*root).to_owned()).collect(),
    }
}

fn history(entries: &[(&str, u64)]) -> History {
    let mut history = History::default();
    for (key, duration_ms) in entries {
        history.record(key, None, *duration_ms, None, NOW);
    }
    history
}

fn timed(targets: &[PlanTarget], duration_ms: u64) -> History {
    history(
        &targets
            .iter()
            .map(|target| (target.history_key.as_str(), duration_ms))
            .collect::<Vec<_>>(),
    )
}

fn roots(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn config(max_workers: usize) -> ScheduleConfig {
    ScheduleConfig {
        default_duration_ms: 120_000,
        worker_startup_ms: 240_000,
        max_workers,
        max_history_age_ms: 30 * 24 * 60 * 60 * 1000,
    }
}

#[test]
fn diamond_plan_reports_critical_path_cost_and_transfer() {
    let targets = [
        target("base", &[], 4_000, &[]),
        target("left", &["base"], 8_000, &[]),
        target("right", &["base"], 1_000, &[]),
        target("top", &["left", "right"], 500, &["packages.top"]),
    ];
    let history = history(&[
        ("derivation.base", 10_000),
        ("derivation.left", 30_000),
        ("derivation.right", 5_000),
        ("packages.top", 1_000),
    ]);

    let decision = schedule(&targets, &roots(&[]), Some(&history), NOW, config(8)).expect("plan");

    assert_eq!(decision.plan.target_count, 4);
    assert_eq!(decision.plan.work_target_count, 4);
    assert_eq!(decision.plan.estimated_work_ms, 46_000);
    assert_eq!(decision.plan.estimated_transfer_bytes, 13_500);
    assert_eq!(decision.plan.critical_path, ["base", "left", "top"]);
    assert_eq!(decision.plan.critical_path_ms, 41_000);
    assert_eq!(decision.plan.unpredicted_targets, 0);
}

#[test]
fn completed_target_costs_nothing_but_keeps_its_prediction() {
    let mut targets = [
        target("base", &[], 0, &[]),
        target("top", &["base"], 0, &["packages.top"]),
    ];
    targets[0].needs_work = false;
    let history = timed(&targets, 30_000);

    let decision = schedule(&targets, &roots(&[]), Some(&history), NOW, config(8)).expect("plan");

    assert_eq!(decision.plan.work_target_count, 1);
    assert_eq!(decision.plan.estimated_work_ms, 30_000);
    assert_eq!(decision.plan.critical_path, ["top"]);
    assert_eq!(decision.targets[0].predicted_duration_ms, 30_000);
    assert!(!decision.targets[0].needs_work);
}

#[test]
fn missing_history_uses_the_configured_default_and_one_worker() {
    let targets = [
        target("a", &[], 0, &["packages.a"]),
        target("b", &[], 0, &["checks.b"]),
    ];
    let mut options = config(8);
    options.default_duration_ms = 77_000;

    let decision = schedule(&targets, &roots(&["checks.b"]), None, NOW, options).expect("plan");

    assert_eq!(decision.strategy, Strategy::SingleWorker);
    assert_eq!(decision.workers, 1);
    assert_eq!(
        decision.fallback_reason.as_deref(),
        Some("2 work target(s) have no recorded timing")
    );
    assert!(decision.targets.iter().all(|prediction| {
        prediction.predicted_from == PredictionSource::Default
            && prediction.predicted_duration_ms == 77_000
    }));
}

#[test]
fn independent_expensive_roots_fan_out_with_unique_ownership() {
    let targets = [
        target("a3", &[], 0, &[]),
        target("a2", &["a3"], 0, &[]),
        target("a1", &["a2"], 0, &["packages.a"]),
        target("b3", &[], 0, &[]),
        target("b2", &["b3"], 0, &[]),
        target("b1", &["b2"], 0, &["checks.b"]),
    ];
    let history = timed(&targets, 200_000);

    let decision = schedule(
        &targets,
        &roots(&["checks.b"]),
        Some(&history),
        NOW,
        config(8),
    )
    .expect("plan");

    assert_eq!(decision.strategy, Strategy::FanOut);
    assert_eq!(decision.workers, 2);
    assert_eq!(decision.predicted_single_worker_ms, 1_200_000);
    assert_eq!(decision.predicted_wall_ms, 840_000);
    let owners = decision
        .units
        .iter()
        .flat_map(|unit| unit.targets.iter().map(move |target| (target, unit.index)))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(owners.len(), targets.len());
}

#[test]
fn shared_expensive_base_keeps_work_on_one_worker() {
    let targets = [
        target("base", &[], 0, &[]),
        target("a", &["base"], 0, &["packages.a"]),
        target("b", &["base"], 0, &["checks.b"]),
    ];
    let history = history(&[
        ("derivation.base", 900_000),
        ("packages.a", 60_000),
        ("checks.b", 60_000),
    ]);

    let decision = schedule(
        &targets,
        &roots(&["checks.b"]),
        Some(&history),
        NOW,
        config(8),
    )
    .expect("plan");

    assert_eq!(decision.strategy, Strategy::SingleWorker);
    assert_eq!(decision.predicted_wall_ms, 1_020_000);
    assert!(decision.fallback_reason.is_none());
}

#[test]
fn shared_cheap_target_has_one_owner_and_is_reported_by_other_units() {
    let targets = [
        target("base", &[], 0, &[]),
        target("a", &["base"], 0, &["packages.a"]),
        target("b", &["base"], 0, &["packages.b"]),
    ];
    let history = history(&[
        ("derivation.base", 1_000),
        ("packages.a", 900_000),
        ("packages.b", 900_000),
    ]);

    let decision = schedule(&targets, &roots(&[]), Some(&history), NOW, config(2)).expect("plan");

    assert_eq!(decision.strategy, Strategy::FanOut);
    assert_eq!(
        decision
            .units
            .iter()
            .filter(|unit| unit.targets.contains(&"base".to_owned()))
            .count(),
        1
    );
    assert_eq!(
        decision
            .units
            .iter()
            .map(|unit| unit.shared_target_count)
            .sum::<usize>(),
        1
    );
}

#[test]
fn required_roots_and_graph_shape_are_validated() {
    let targets = [
        target("a", &[], 0, &["packages.a"]),
        target("a", &[], 0, &[]),
    ];

    assert_eq!(
        schedule(&targets, &roots(&[]), None, NOW, config(1)),
        Err(ScheduleError::DuplicateTarget("a".to_owned()))
    );

    let targets = [target("a", &[], 0, &["packages.a"])];
    assert_eq!(
        schedule(&targets, &roots(&["checks.b"]), None, NOW, config(1)),
        Err(ScheduleError::UncoveredRoots(vec!["checks.b".to_owned()]))
    );
    assert_eq!(
        schedule(&targets, &roots(&[]), None, NOW, config(0)),
        Err(ScheduleError::ZeroWorkers)
    );
}

#[test]
fn cycles_are_rejected_instead_of_silently_dropping_targets() {
    let targets = [
        target("a", &["b"], 0, &["packages.a"]),
        target("b", &["a"], 0, &["packages.b"]),
    ];

    assert_eq!(
        schedule(&targets, &roots(&[]), None, NOW, config(2)),
        Err(ScheduleError::DependencyCycle(vec![
            "a".to_owned(),
            "b".to_owned()
        ]))
    );
}

#[test]
fn observations_are_attached_without_owning_persistence_or_rendering() {
    let targets = [target("a", &[], 0, &["packages.a"])];
    let history = timed(&targets, 30_000);
    let mut decision =
        schedule(&targets, &roots(&[]), Some(&history), NOW, config(8)).expect("plan");

    observe(
        &mut decision,
        &BTreeMap::from([("a".to_owned(), (44_000, "built".to_owned()))]),
    );

    assert_eq!(decision.targets[0].observed_duration_ms, Some(44_000));
    assert_eq!(decision.targets[0].observed_state.as_deref(), Some("built"));
}
