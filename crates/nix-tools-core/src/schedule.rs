//! Deterministic, pure scheduling over a weighted dependency graph.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Serialize;

use crate::history::History;

/// Caller-owned policy for one scheduling decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduleConfig {
    /// Duration assigned to a target without usable history.
    pub default_duration_ms: u64,
    /// Fixed wall-time cost paid when more than one worker is selected.
    pub worker_startup_ms: u64,
    /// Maximum number of workers the caller can provision; must be at least one.
    pub max_workers: usize,
    /// Maximum age of a history record used for prediction.
    pub max_history_age_ms: u64,
}

/// One weighted node in the scheduling graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanTarget {
    /// Unique stable identifier within this graph.
    pub id: String,
    /// Stable key used to look up timing history.
    pub history_key: String,
    /// IDs of targets that must be available first.
    pub dependencies: BTreeSet<String>,
    /// Whether this target still needs work during this run.
    pub needs_work: bool,
    /// Bytes expected to cross the network for this target.
    pub transfer_bytes: u64,
    /// Requested roots whose dependency closures include this target.
    pub roots: BTreeSet<String>,
}

/// Source of a target duration prediction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictionSource {
    /// A fresh history record.
    History,
    /// The caller's configured unknown-target duration.
    Default,
}

/// Execution shape selected by the planner.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// All work stays on one worker.
    SingleWorker,
    /// Independent roots are assigned to multiple workers.
    FanOut,
}

/// Predicted and, optionally, observed cost of one target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TargetPrediction {
    /// Target ID.
    pub id: String,
    /// History lookup key.
    pub history_key: String,
    /// Whether this target needs work.
    pub needs_work: bool,
    /// Predicted work duration.
    pub predicted_duration_ms: u64,
    /// Predicted bytes transferred.
    pub predicted_transfer_bytes: u64,
    /// Source of the duration prediction.
    pub predicted_from: PredictionSource,
    /// Realized duration attached after execution.
    pub observed_duration_ms: Option<u64>,
    /// Caller-defined realized state attached after execution.
    pub observed_state: Option<String>,
}

/// Aggregate cost and critical-path information for the graph.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Plan {
    /// Number of graph targets.
    pub target_count: usize,
    /// Number of targets that still need work.
    pub work_target_count: usize,
    /// Sum of predicted work durations, without parallelism.
    pub estimated_work_ms: u64,
    /// Sum of predicted transfer bytes.
    pub estimated_transfer_bytes: u64,
    /// Longest weighted dependency chain.
    pub critical_path: Vec<String>,
    /// Cost of the critical path.
    pub critical_path_ms: u64,
    /// Number of targets predicted from history.
    pub predicted_targets: usize,
    /// Number of work targets using the configured default.
    pub unpredicted_targets: usize,
}

/// One worker's roots and exclusive target ownership.
///
/// Every target appears in exactly one unit's `targets`. `shared_target_count` describes targets in
/// this unit's root closure that another unit owns, allowing a caller to quantify duplicated work.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Unit {
    /// Stable zero-based position in the schedule.
    pub index: usize,
    /// Predicted duration of the union of this unit's root closures.
    pub predicted_ms: u64,
    /// Requested roots assigned to this unit.
    pub roots: Vec<String>,
    /// Target IDs this unit exclusively owns.
    pub targets: Vec<String>,
    /// Targets needed by this unit but owned by another unit.
    pub shared_target_count: usize,
}

/// Pure scheduling result; persistence and CI rendering belong to the caller.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Schedule {
    /// Selected execution shape.
    pub strategy: Strategy,
    /// Number of worker units.
    pub workers: usize,
    /// Predicted duration without fan-out.
    pub predicted_single_worker_ms: u64,
    /// Predicted wall duration after fan-out and startup cost.
    pub predicted_wall_ms: u64,
    /// Startup cost used for this decision.
    pub worker_startup_ms: u64,
    /// Why fan-out was conservatively disabled, if applicable.
    pub fallback_reason: Option<String>,
    /// Per-worker assignment.
    pub units: Vec<Unit>,
    /// Aggregate graph plan.
    pub plan: Plan,
    /// Per-target predictions.
    pub targets: Vec<TargetPrediction>,
}

/// Invalid graph or caller configuration rejected by the planner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScheduleError {
    /// At least one required root was absent from the graph.
    UncoveredRoots(Vec<String>),
    /// Two graph nodes used the same target ID.
    DuplicateTarget(String),
    /// A node named a dependency absent from the graph.
    UnknownDependency {
        /// Target containing the invalid edge.
        target: String,
        /// Missing dependency ID.
        dependency: String,
    },
    /// The graph contains a dependency cycle; the value lists nodes left after topological sort.
    DependencyCycle(Vec<String>),
    /// `ScheduleConfig::max_workers` was zero.
    ZeroWorkers,
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UncoveredRoots(roots) => {
                write!(
                    formatter,
                    "schedule does not cover required root(s): {}",
                    roots.join(", ")
                )
            }
            Self::DuplicateTarget(target) => write!(formatter, "duplicate target ID: {target}"),
            Self::UnknownDependency { target, dependency } => {
                write!(
                    formatter,
                    "target {target} depends on unknown target {dependency}"
                )
            }
            Self::DependencyCycle(targets) => {
                write!(
                    formatter,
                    "dependency cycle contains: {}",
                    targets.join(", ")
                )
            }
            Self::ZeroWorkers => formatter.write_str("maximum worker count must be at least one"),
        }
    }
}

impl std::error::Error for ScheduleError {}

/// Builds a weighted plan and selects the cheapest worker count.
///
/// Unknown work conservatively stays on one worker. Fan-out is selected only when every work target
/// has fresh history, at least two independent roots exist, and startup-adjusted wall time improves.
///
/// # Errors
///
/// Returns an error for invalid configuration, duplicate IDs, missing dependencies, cycles, or
/// required roots not covered by any unit.
pub fn schedule(
    targets: &[PlanTarget],
    required_roots: &BTreeSet<String>,
    history: Option<&History>,
    now_ms: u64,
    config: ScheduleConfig,
) -> Result<Schedule, ScheduleError> {
    let order = validate_graph(targets, config.max_workers)?;
    let predictions = predict(targets, history, now_ms, config);
    let durations = predictions
        .iter()
        .map(|prediction| {
            (
                prediction.id.clone(),
                if prediction.needs_work {
                    prediction.predicted_duration_ms
                } else {
                    0
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let plan = build_plan(targets, &predictions, &durations, &order);
    let closures = root_closures(targets);

    let mut fallback_reason = (plan.unpredicted_targets > 0).then(|| {
        format!(
            "{} work target(s) have no recorded timing",
            plan.unpredicted_targets
        )
    });
    if fallback_reason.is_none() && closures.len() < 2 {
        fallback_reason = Some("plan has fewer than two independent roots".to_owned());
    }

    let workers = if fallback_reason.is_some() {
        1
    } else {
        best_worker_count(
            &closures,
            &durations,
            plan.estimated_work_ms,
            config.max_workers,
            config.worker_startup_ms,
        )
    };
    let groups = group_roots(&closures, &durations, workers);
    let units = build_units(&groups, &closures, targets, &durations);

    let covered = units
        .iter()
        .flat_map(|unit| unit.roots.iter().cloned())
        .collect::<BTreeSet<_>>();
    let uncovered = required_roots
        .iter()
        .filter(|root| !covered.contains(*root))
        .cloned()
        .collect::<Vec<_>>();
    if !uncovered.is_empty() {
        return Err(ScheduleError::UncoveredRoots(uncovered));
    }

    let actual_workers = units.len().max(1);
    let slowest_unit_ms = units
        .iter()
        .map(|unit| unit.predicted_ms)
        .max()
        .unwrap_or(0);
    let predicted_wall_ms = if actual_workers > 1 {
        slowest_unit_ms.saturating_add(config.worker_startup_ms)
    } else {
        slowest_unit_ms
    };
    Ok(Schedule {
        strategy: if actual_workers > 1 {
            Strategy::FanOut
        } else {
            Strategy::SingleWorker
        },
        workers: actual_workers,
        predicted_single_worker_ms: plan.estimated_work_ms,
        predicted_wall_ms,
        worker_startup_ms: config.worker_startup_ms,
        fallback_reason,
        units,
        plan,
        targets: predictions,
    })
}

/// Attaches realized durations and caller-defined states to a schedule.
pub fn observe(schedule: &mut Schedule, observations: &BTreeMap<String, (u64, String)>) {
    for prediction in &mut schedule.targets {
        if let Some((duration_ms, state)) = observations.get(&prediction.id) {
            prediction.observed_duration_ms = Some(*duration_ms);
            prediction.observed_state = Some(state.clone());
        }
    }
}

fn validate_graph(
    targets: &[PlanTarget],
    max_workers: usize,
) -> Result<Vec<String>, ScheduleError> {
    if max_workers == 0 {
        return Err(ScheduleError::ZeroWorkers);
    }
    let mut ids = BTreeSet::new();
    for target in targets {
        if !ids.insert(target.id.as_str()) {
            return Err(ScheduleError::DuplicateTarget(target.id.clone()));
        }
    }
    for target in targets {
        if let Some(dependency) = target
            .dependencies
            .iter()
            .find(|dependency| !ids.contains(dependency.as_str()))
        {
            return Err(ScheduleError::UnknownDependency {
                target: target.id.clone(),
                dependency: dependency.clone(),
            });
        }
    }
    topological_order(targets)
}

fn predict(
    targets: &[PlanTarget],
    history: Option<&History>,
    now_ms: u64,
    config: ScheduleConfig,
) -> Vec<TargetPrediction> {
    targets
        .iter()
        .map(|target| {
            let record = history.and_then(|history| {
                history.lookup(&target.history_key, now_ms, config.max_history_age_ms)
            });
            TargetPrediction {
                id: target.id.clone(),
                history_key: target.history_key.clone(),
                needs_work: target.needs_work,
                predicted_duration_ms: record
                    .map_or(config.default_duration_ms, |record| record.duration_ms),
                predicted_transfer_bytes: target.transfer_bytes,
                predicted_from: if record.is_some() {
                    PredictionSource::History
                } else {
                    PredictionSource::Default
                },
                observed_duration_ms: None,
                observed_state: None,
            }
        })
        .collect()
}

fn build_plan(
    targets: &[PlanTarget],
    predictions: &[TargetPrediction],
    durations: &BTreeMap<String, u64>,
    order: &[String],
) -> Plan {
    let (critical_path, critical_path_ms) = critical_path(targets, durations, order);
    Plan {
        target_count: targets.len(),
        work_target_count: targets.iter().filter(|target| target.needs_work).count(),
        estimated_work_ms: durations
            .values()
            .fold(0_u64, |total, duration| total.saturating_add(*duration)),
        estimated_transfer_bytes: targets.iter().fold(0_u64, |total, target| {
            total.saturating_add(target.transfer_bytes)
        }),
        critical_path,
        critical_path_ms,
        predicted_targets: predictions
            .iter()
            .filter(|prediction| prediction.predicted_from == PredictionSource::History)
            .count(),
        unpredicted_targets: predictions
            .iter()
            .filter(|prediction| {
                prediction.needs_work && prediction.predicted_from == PredictionSource::Default
            })
            .count(),
    }
}

fn critical_path(
    targets: &[PlanTarget],
    durations: &BTreeMap<String, u64>,
    order: &[String],
) -> (Vec<String>, u64) {
    let by_id = index(targets);
    let mut best: BTreeMap<&str, (u64, Option<&str>)> = BTreeMap::new();
    for id in order {
        let target = by_id[id.as_str()];
        let mut predecessor = None;
        let mut inherited = 0;
        for dependency in &target.dependencies {
            let (cost, _) = best[dependency.as_str()];
            if cost > inherited {
                inherited = cost;
                predecessor = best.get_key_value(dependency.as_str()).map(|(key, _)| *key);
            }
        }
        let own = durations.get(&target.id).copied().unwrap_or_default();
        best.insert(
            target.id.as_str(),
            (inherited.saturating_add(own), predecessor),
        );
    }
    let Some((mut cursor, total)) = best
        .iter()
        .max_by(|left, right| left.1.0.cmp(&right.1.0).then(right.0.cmp(left.0)))
        .map(|(id, (cost, _))| (*id, *cost))
        .filter(|(_, cost)| *cost > 0)
    else {
        return (Vec::new(), 0);
    };
    let mut path = vec![cursor.to_owned()];
    while let Some((_, Some(previous))) = best.get(cursor) {
        cursor = previous;
        path.push(cursor.to_owned());
    }
    path.reverse();
    (path, total)
}

fn index(targets: &[PlanTarget]) -> BTreeMap<&str, &PlanTarget> {
    targets
        .iter()
        .map(|target| (target.id.as_str(), target))
        .collect()
}

fn topological_order(targets: &[PlanTarget]) -> Result<Vec<String>, ScheduleError> {
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut pending = BTreeMap::new();
    for target in targets {
        pending.insert(target.id.as_str(), target.dependencies.len());
        for dependency in &target.dependencies {
            dependents
                .entry(dependency.as_str())
                .or_default()
                .push(target.id.as_str());
        }
    }
    let mut ready = pending
        .iter()
        .filter(|(_, remaining)| **remaining == 0)
        .map(|(id, _)| *id)
        .collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(targets.len());
    while let Some(next) = ready.pop_first() {
        pending.remove(next);
        order.push(next.to_owned());
        for dependent in dependents.get(next).into_iter().flatten() {
            if let Some(remaining) = pending.get_mut(dependent) {
                *remaining -= 1;
                if *remaining == 0 {
                    ready.insert(dependent);
                }
            }
        }
    }
    if pending.is_empty() {
        Ok(order)
    } else {
        Err(ScheduleError::DependencyCycle(
            pending.keys().map(|id| (*id).to_owned()).collect(),
        ))
    }
}

fn root_closures(targets: &[PlanTarget]) -> BTreeMap<String, BTreeSet<String>> {
    let by_id = index(targets);
    let mut closures = BTreeMap::new();
    for target in targets {
        for root in &target.roots {
            let closure: &mut BTreeSet<String> = closures.entry(root.clone()).or_default();
            let mut pending = vec![target.id.clone()];
            while let Some(id) = pending.pop() {
                if !closure.insert(id.clone()) {
                    continue;
                }
                pending.extend(by_id[id.as_str()].dependencies.iter().cloned());
            }
        }
    }
    closures
}

fn closure_cost(closure: &BTreeSet<String>, durations: &BTreeMap<String, u64>) -> u64 {
    closure.iter().fold(0_u64, |total, id| {
        total.saturating_add(durations.get(id).copied().unwrap_or_default())
    })
}

fn group_roots(
    closures: &BTreeMap<String, BTreeSet<String>>,
    durations: &BTreeMap<String, u64>,
    workers: usize,
) -> Vec<Vec<String>> {
    if workers <= 1 {
        return vec![closures.keys().cloned().collect()];
    }
    let mut ordered = closures.keys().cloned().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        closure_cost(&closures[right], durations)
            .cmp(&closure_cost(&closures[left], durations))
            .then(left.cmp(right))
    });
    let mut groups = vec![Vec::new(); workers];
    let mut unions = vec![BTreeSet::new(); workers];
    for root in ordered {
        let closure = &closures[&root];
        let chosen = (0..workers)
            .min_by_key(|index| {
                let merged = unions[*index].union(closure).cloned().collect();
                (closure_cost(&merged, durations), *index)
            })
            .unwrap_or_default();
        unions[chosen].extend(closure.iter().cloned());
        groups[chosen].push(root);
    }
    groups.retain(|group| !group.is_empty());
    for group in &mut groups {
        group.sort();
    }
    groups
}

fn best_worker_count(
    closures: &BTreeMap<String, BTreeSet<String>>,
    durations: &BTreeMap<String, u64>,
    single_ms: u64,
    max_workers: usize,
    startup_ms: u64,
) -> usize {
    let ceiling = max_workers.min(closures.len());
    let mut best = (single_ms, 1);
    for workers in 2..=ceiling {
        let groups = group_roots(closures, durations, workers);
        if groups.len() < workers {
            continue;
        }
        let wall = groups
            .iter()
            .map(|group| {
                let union = group
                    .iter()
                    .flat_map(|root| closures[root].iter().cloned())
                    .collect::<BTreeSet<_>>();
                closure_cost(&union, durations)
            })
            .max()
            .unwrap_or(0)
            .saturating_add(startup_ms);
        if wall < best.0 {
            best = (wall, workers);
        }
    }
    best.1
}

fn build_units(
    groups: &[Vec<String>],
    closures: &BTreeMap<String, BTreeSet<String>>,
    targets: &[PlanTarget],
    durations: &BTreeMap<String, u64>,
) -> Vec<Unit> {
    let mut owned = BTreeSet::new();
    let mut units = Vec::with_capacity(groups.len().max(1));
    for (unit_index, group) in groups.iter().enumerate() {
        let union = group
            .iter()
            .flat_map(|root| closures[root].iter().cloned())
            .collect::<BTreeSet<_>>();
        let mine = union
            .iter()
            .filter(|id| !owned.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        let shared_target_count = union.len() - mine.len();
        owned.extend(mine.iter().cloned());
        units.push(Unit {
            index: unit_index,
            predicted_ms: closure_cost(&union, durations),
            roots: group.clone(),
            targets: mine,
            shared_target_count,
        });
    }
    if units.is_empty() {
        units.push(Unit {
            index: 0,
            predicted_ms: 0,
            roots: Vec::new(),
            targets: Vec::new(),
            shared_target_count: 0,
        });
    }
    let orphans = targets
        .iter()
        .filter(|target| !owned.contains(&target.id))
        .map(|target| target.id.clone())
        .collect::<Vec<_>>();
    if let Some(first) = units.first_mut() {
        first.predicted_ms =
            first
                .predicted_ms
                .saturating_add(orphans.iter().fold(0_u64, |total, id| {
                    total.saturating_add(durations.get(id).copied().unwrap_or_default())
                }));
        first.targets.extend(orphans);
        first.targets.sort();
        first.targets.dedup();
    }
    units
}

#[cfg(test)]
#[path = "schedule_test.rs"]
mod schedule_test;
