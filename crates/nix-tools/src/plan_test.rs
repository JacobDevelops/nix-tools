use std::collections::BTreeSet;

use super::{PlanInput, plan_json};

#[test]
fn plan_json_is_deterministic_and_uses_core_scheduler() {
    let input = PlanInput {
        targets: vec![
            super::PlanTargetInput {
                id: "test".into(),
                history_key: "test".into(),
                dependencies: BTreeSet::from(["build".into()]),
                needs_work: true,
                transfer_bytes: 0,
                roots: BTreeSet::from(["test".into()]),
            },
            super::PlanTargetInput {
                id: "build".into(),
                history_key: "build".into(),
                dependencies: BTreeSet::new(),
                needs_work: true,
                transfer_bytes: 12,
                roots: BTreeSet::from(["test".into()]),
            },
        ],
        required_roots: BTreeSet::from(["test".into()]),
        history: Some(vec![
            super::HistoryInput {
                key: "build".into(),
                duration_ms: 20,
                size_bytes: Some(5),
                recorded_at_ms: 100,
                source: None,
            },
            super::HistoryInput {
                key: "test".into(),
                duration_ms: 10,
                size_bytes: None,
                recorded_at_ms: 100,
                source: None,
            },
        ]),
        now_ms: 120,
        config: super::PlanConfigInput {
            default_duration_ms: 100,
            worker_startup_ms: 1,
            max_workers: 2,
            max_history_age_ms: 100,
        },
    };

    let encoded = serde_json::to_vec(&input).unwrap();
    let output = plan_json(&encoded).unwrap();

    assert_eq!(output, plan_json(&encoded).unwrap());
    assert!(output.ends_with(b"\n"));
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["plan"]["estimated_work_ms"], 30);
    assert_eq!(value["schedule"]["plan"]["critical_path_ms"], 30);
}

#[test]
fn plan_json_rejects_unknown_fields_at_the_boundary() {
    let error = plan_json(br#"{"targets":[],"required_roots":[],"history":null,"now_ms":0,"config":{"default_duration_ms":1,"worker_startup_ms":0,"max_workers":1,"max_history_age_ms":1},"unknown":true}"#).unwrap_err();
    assert_eq!(error.kind, nix_tools_core::outcome::ErrorKind::Usage);
}
