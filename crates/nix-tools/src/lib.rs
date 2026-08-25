#![forbid(unsafe_code)]

//! Provider-neutral flake operations built on [`nix_tools_core`].
//!
//! Repository CLIs should compose [`StandardCommands`] with their own argument parsing, output,
//! and selector policy. The included binary is a deliberately thin reference client, not a
//! required command hierarchy.

mod command;
mod flake;
mod plan;

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

#[cfg(test)]
#[path = "command_test.rs"]
mod command_test;

#[cfg(test)]
#[path = "flake_test.rs"]
mod flake_test;

#[cfg(test)]
#[path = "plan_test.rs"]
mod plan_test;
