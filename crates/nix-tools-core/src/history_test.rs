use super::{History, derivation_key, target_key};

const NOW: u64 = 1_800_000_000_000;
const MAX_AGE: u64 = 30 * 24 * 60 * 60 * 1000;

fn document(schema: &str, recorded_at_ms: u64) -> Vec<u8> {
    let mut history = History::default();
    history.record(
        "checks.test",
        Some("/nix/store/00000000000000000000000000000000-test.drv"),
        376_000,
        Some(4_096),
        recorded_at_ms,
    );
    history.to_bytes(schema).expect("history bytes")
}

#[test]
fn caller_selected_schema_round_trips_without_a_built_in_name() {
    let restored = History::from_slice(&document("example.history/v2", NOW), "example.history/v2")
        .expect("history");

    let record = restored
        .lookup("checks.test", NOW, MAX_AGE)
        .expect("record");
    assert_eq!(record.duration_ms, 376_000);
    assert_eq!(record.size_bytes, Some(4_096));
    assert_eq!(restored.len(), 1);
}

#[test]
fn foreign_corrupt_and_misfiled_documents_are_unusable() {
    assert!(History::from_slice(b"not json", "example/v1").is_none());
    assert!(History::from_slice(&document("other/v1", NOW), "example/v1").is_none());
    assert!(
        History::from_slice(
            br#"{"schema":"example/v1","records":{"a":{"key":"b","source":null,"duration_ms":1,"size_bytes":null,"recorded_at_ms":1}}}"#,
            "example/v1"
        )
        .is_none()
    );
}

#[test]
fn age_policy_is_supplied_by_the_caller() {
    let mut history =
        History::from_slice(&document("example/v1", NOW), "example/v1").expect("history");

    assert!(
        history
            .lookup("checks.test", NOW + MAX_AGE, MAX_AGE)
            .is_some()
    );
    assert!(
        history
            .lookup("checks.test", NOW + MAX_AGE + 1, MAX_AGE)
            .is_none()
    );
    history.forget_stale(NOW + MAX_AGE + 1, MAX_AGE);
    assert!(history.is_empty());
}

#[test]
fn stable_keys_survive_content_address_changes() {
    assert_eq!(target_key("checks", "test"), "checks.test");
    assert_eq!(
        derivation_key("/nix/store/00000000000000000000000000000000-bun-deps.drv"),
        derivation_key("/nix/store/11111111111111111111111111111111-bun-deps.drv")
    );
}
