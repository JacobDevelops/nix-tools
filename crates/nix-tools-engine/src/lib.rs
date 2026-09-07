#![forbid(unsafe_code)]

//! Policy-free Nix flake discovery, evaluation, cache probing, and realization.

mod activity;
mod engine;
mod graph;
mod model;

pub use engine::NixEngine;
pub use graph::DependencyGraph;
pub use model::{
    Availability, AvailabilityState, BuildRequest, CheckRequest, Clock, DependencyFailure,
    DerivationNode, Diagnostic, DiagnosticSeverity, DiscoverRequest, DiscoveredTargets,
    EngineConfig, EngineDependencies, EngineError, EngineRequest, EngineResponse, FlakeEngine,
    FlakeRef, GraphMode, Manifest, ManifestMetrics, ManifestOutcome, NoProgress, NodeMetrics,
    NodeResult, NodeState, Phase, PhaseMetrics, PreparedRun, ProgressEvent, ProgressSink,
    ResourceLimits, RootResult, RunRequest, SystemClock, TargetKind, TrustedSubstituter,
};

#[cfg(test)]
#[path = "activity_test.rs"]
mod activity_test;

#[cfg(test)]
#[path = "engine_test.rs"]
mod engine_test;

#[cfg(test)]
#[path = "graph_test.rs"]
mod graph_test;
