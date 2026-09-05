#![forbid(unsafe_code)]

//! Provider-neutral flake operations built on [`nix_tools_core`].
//!
//! Repository CLIs should compose [`Runtime`] with their own argument parsing and target-selection
//! policy. The runtime owns engine setup, interactive progress, cancellation, realization, and app
//! execution. [`StandardCommands`] is the lower-level noninteractive seam for callers that already
//! own an engine and output adapters. The included binary is a deliberately thin reference client.

mod command;
mod flake;
mod plan;
mod runtime;
mod ui;

#[cfg(test)]
#[path = "runtime_test.rs"]
mod runtime_test;

pub use command::{
    AppExecutionPolicy, AppOutputPolicy, BoundedAppOutput, StandardCommands, manifest_result,
};
pub use flake::{
    CheckSelector, Flake, FlakeContents, FlakeOperations, NixTools, StandardFlake,
    StandardFlakeKind,
};
pub use nix_tools_engine::{
    BuildRequest, CheckRequest, EngineRequest, EngineResponse, FlakeEngine, FlakeRef, Manifest,
    ManifestOutcome, PreparedRun, RunRequest,
};
pub use plan::{
    HistoryInput, PlanConfigInput, PlanInput, PlanOutput, PlanTargetInput, plan, plan_json,
};
pub use runtime::{
    Runtime, RuntimeCommand, RuntimeConfig, RuntimeDependencies, SelectedCheckCommand,
    forward_termination_signals,
};
pub use ui::{DisplayContext, OutputMode};

#[cfg(test)]
#[path = "command_test.rs"]
mod command_test;

#[cfg(test)]
#[path = "flake_test.rs"]
mod flake_test;

#[cfg(test)]
#[path = "plan_test.rs"]
mod plan_test;
