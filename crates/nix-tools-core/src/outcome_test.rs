use super::{Error, ErrorKind, ExitCode, Outcome};

#[test]
fn child_exit_codes_are_preserved_when_portable() {
    assert_eq!(ExitCode::from_child_code(42).get(), 42);
    assert_eq!(ExitCode::from_child_code(126).get(), 126);
    assert_eq!(ExitCode::from_child_code(127).get(), 127);
    assert_eq!(ExitCode::from_child_code(130).get(), 130);
    assert_eq!(ExitCode::from_child_code(255).get(), 255);
}

#[test]
fn out_of_range_child_exit_codes_become_failure() {
    assert_eq!(ExitCode::from_child_code(0), ExitCode::FAILURE);
    assert_eq!(ExitCode::from_child_code(-1), ExitCode::FAILURE);
    assert_eq!(ExitCode::from_child_code(256), ExitCode::FAILURE);
}

#[test]
fn signal_exit_codes_follow_shell_convention() {
    assert_eq!(ExitCode::from_signal(2).get(), 130);
    assert_eq!(ExitCode::from_signal(15).get(), 143);
}

#[test]
fn outcome_carries_the_child_exit_code() {
    let outcome = Outcome::success("failed").with_exit_code(ExitCode::from_child_code(42));
    assert_eq!(outcome.exit_code().get(), 42);
}

#[test]
fn default_outcome_exits_successfully() {
    assert_eq!(Outcome::default().exit_code(), ExitCode::SUCCESS);
}

#[test]
fn outcome_data_rejects_reserved_result_fields() {
    let error = Outcome::success("invalid")
        .with_data(&serde_json::json!({ "exit_code": 1 }))
        .expect_err("reserved key");

    assert!(error.message.contains("collides with common result fields"));
}

#[test]
fn generic_error_replaces_the_source_specific_error_name() {
    let error = Error::new(ErrorKind::Internal, ExitCode::INTERNAL, "sensitive detail");

    assert_eq!(error.to_string(), "sensitive detail");
    assert_eq!(
        format!("{error:?}"),
        "Error { kind: Internal, message: \"[REDACTED]\", exit_code: ExitCode(70) }"
    );
}

#[test]
fn outcome_accepts_repository_specific_object_data() {
    let outcome = Outcome::success("done")
        .with_data(&serde_json::json!({ "repository_field": 42 }))
        .expect("object data");

    assert_eq!(
        outcome.data(),
        Some(&serde_json::json!({ "repository_field": 42 }))
    );
}
