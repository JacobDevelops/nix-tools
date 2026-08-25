//! JSON boundary for deterministic, provider-neutral scheduling.

use std::collections::BTreeSet;

use nix_tools_core::history::History;
use nix_tools_core::outcome::{Error, Result};
use nix_tools_core::schedule::{self, PlanTarget, Schedule, ScheduleConfig};
use serde::{Deserialize, Serialize};

/// JSON input for [`plan`] and [`plan_json`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanInput {
    /// Graph targets to schedule.
    pub targets: Vec<PlanTargetInput>,
    /// Roots that must be covered by the resulting schedule.
    pub required_roots: BTreeSet<String>,
    /// Optional caller-supplied timing history.
    pub history: Option<Vec<HistoryInput>>,
    /// Caller-supplied current Unix epoch milliseconds.
    pub now_ms: u64,
    /// Scheduling policy.
    pub config: PlanConfigInput,
}

/// Serializable target representation at the provider-neutral JSON boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTargetInput {
    /// Stable target identity.
    pub id: String,
    /// Stable history identity.
    pub history_key: String,
    /// Target dependencies.
    pub dependencies: BTreeSet<String>,
    /// Whether the target needs work this run.
    pub needs_work: bool,
    /// Predicted transfer volume.
    pub transfer_bytes: u64,
    /// Requested roots that include this target.
    pub roots: BTreeSet<String>,
}

/// Serializable timing history record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryInput {
    /// History key.
    pub key: String,
    /// Observed duration in milliseconds.
    pub duration_ms: u64,
    /// Optional observed size in bytes.
    pub size_bytes: Option<u64>,
    /// Time the record was captured in Unix epoch milliseconds.
    pub recorded_at_ms: u64,
    /// Optional concrete source identity.
    pub source: Option<String>,
}

/// Serializable scheduling policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanConfigInput {
    /// Default duration for targets lacking fresh history.
    pub default_duration_ms: u64,
    /// Per-fan-out startup cost.
    pub worker_startup_ms: u64,
    /// Maximum parallel workers.
    pub max_workers: usize,
    /// Maximum usable history age.
    pub max_history_age_ms: u64,
}

/// Deterministic provider-neutral planner result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlanOutput {
    /// Aggregate plan data for simple adapters.
    pub plan: nix_tools_core::schedule::Plan,
    /// Full core scheduling decision for adapters that need worker assignment.
    pub schedule: Schedule,
}

/// Computes a schedule using the core planner.
///
/// # Errors
///
/// Returns a structured usage error for invalid JSON input fields or a structured preflight error
/// when the graph cannot be scheduled.
pub fn plan(input: PlanInput) -> Result<PlanOutput> {
    let targets = input
        .targets
        .into_iter()
        .map(|target| PlanTarget {
            id: target.id,
            history_key: target.history_key,
            dependencies: target.dependencies,
            needs_work: target.needs_work,
            transfer_bytes: target.transfer_bytes,
            roots: target.roots,
        })
        .collect::<Vec<_>>();
    let history = input.history.map(|records| {
        let mut history = History::default();
        for record in records {
            history.record(
                &record.key,
                record.source.as_deref(),
                record.duration_ms,
                record.size_bytes,
                record.recorded_at_ms,
            );
        }
        history
    });
    let schedule = schedule::schedule(
        &targets,
        &input.required_roots,
        history.as_ref(),
        input.now_ms,
        ScheduleConfig {
            default_duration_ms: input.config.default_duration_ms,
            worker_startup_ms: input.config.worker_startup_ms,
            max_workers: input.config.max_workers,
            max_history_age_ms: input.config.max_history_age_ms,
        },
    )
    .map_err(|error| Error::preflight(error.to_string()))?;
    Ok(PlanOutput {
        plan: schedule.plan.clone(),
        schedule,
    })
}

/// Parses plan input and returns deterministic, newline-terminated JSON.
///
/// # Errors
///
/// Returns a structured usage error for invalid JSON and an internal error if serialization fails.
pub fn plan_json(input: &[u8]) -> Result<Vec<u8>> {
    let input = serde_json::from_slice(input)
        .map_err(|error| Error::usage(format!("invalid plan JSON: {error}")))?;
    let mut output = serde_json::to_vec(&plan(input)?)
        .map_err(|error| Error::internal(format!("serialize plan JSON: {error}")))?;
    output.push(b'\n');
    Ok(output)
}
