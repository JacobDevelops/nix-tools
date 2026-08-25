use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

use super::Redactor;

#[test]
fn registered_values_are_redacted_longest_first() {
    let redactor = Redactor::default();
    redactor.register("token");
    redactor.register("token-long");
    assert_eq!(redactor.redact("token-long token"), "[REDACTED] [REDACTED]");
}

#[test]
fn registered_values_mask_quoted_url_encoded_and_base64_variants() {
    let redactor = Redactor::default();
    redactor.register("p@ss word+/=x");
    redactor.register("???");
    let redacted = redactor.redact(
        "'p@ss word+/=x' p%40ss%20word%2B%2F%3Dx p%40ss%20word%2b%2f%3dx cEBzcyB3b3JkKy89eA cEBzcyB3b3JkKy89eA== Pz8/ Pz8_",
    );
    assert_eq!(
        redacted,
        "'[REDACTED]' [REDACTED] [REDACTED] [REDACTED] [REDACTED] [REDACTED] [REDACTED]"
    );
}

#[test]
fn registered_values_include_terminal_and_unicode_normalized_variants() {
    let redactor = Redactor::default();
    redactor.register("se\u{200d}cret");
    redactor.register("ansi\u{1b}[31m-secret");

    assert_eq!(
        redactor.redact("secret ansi-secret"),
        "[REDACTED] [REDACTED]"
    );
}

#[test]
fn normalized_variant_cannot_hide_a_sensitive_assignment_key() {
    let redactor = Redactor::default();
    redactor.register("to\u{200d}ken");

    let redacted = redactor.redact("token=other-sensitive-value");

    assert_eq!(redacted, "[REDACTED]=[REDACTED]");
}

#[test]
fn unquoted_assignment_redacts_a_registered_secret_containing_spaces() {
    let redactor = Redactor::default();
    redactor.register("correct horse battery");

    let redacted = redactor.redact("TOKEN=correct horse battery\n");

    assert_eq!(redacted, "TOKEN=[REDACTED]\n");
}

#[test]
fn debug_output_never_contains_registered_values() {
    let redactor = Redactor::default();
    redactor.register("debug-canary-secret");
    let rendered = format!("{redactor:?}");
    assert_eq!(rendered, "Redactor { secrets: \"[REDACTED]\" }");
}

#[test]
fn common_sensitive_assignments_are_redacted_case_insensitively() {
    let redactor = Redactor::default();
    assert_eq!(
        redactor.redact(
            "token=abc password: 'alpha beta' Authorization: Bearer credential safe=value\n{\"token\":\"alpha\\\"beta\"}"
        ),
        "token=[REDACTED] password: '[REDACTED]' Authorization: [REDACTED]\n{\"token\":\"[REDACTED]\"}"
    );
}

#[test]
fn key_mentions_without_values_are_preserved() {
    let redactor = Redactor::default();
    assert_eq!(
        redactor.redact("token refresh failed"),
        "token refresh failed"
    );
}

#[test]
fn assignment_prefixes_are_detected_in_invalid_utf8_without_panicking() {
    assert!(Redactor::contains_sensitive_assignment_prefix(
        b"\xffTOKEN "
    ));
    assert!(Redactor::contains_sensitive_assignment_prefix(
        b"\xff\xff\xffAUTHORIZATION\t"
    ));
}

#[test]
fn assignments_after_an_invalid_byte_are_still_detected() {
    assert!(Redactor::contains_sensitive_assignment(b"\xffTOKEN=abc"));
    assert!(Redactor::contains_sensitive_assignment(
        b"\xff\xffPASSWORD: hunter2"
    ));
}

#[test]
fn frame_end_indexes_the_original_bytes_when_input_is_not_utf8() {
    let redactor = Redactor::default();
    let input = b"\xffTOKEN=abcdef";

    let frame_end = redactor
        .safe_frame_end(input, input.len(), true)
        .expect("frame end");

    assert_eq!(frame_end, 1);
    assert_eq!(&input[..frame_end], b"\xff");
}

#[test]
fn secrets_containing_invalid_utf8_are_redacted() {
    let redactor = Redactor::default();
    redactor.register(b"ab\xffcd");

    assert_eq!(
        redactor.redact_bytes(b"leaked ab\xffcd here"),
        b"leaked [REDACTED] here"
    );
}

#[test]
fn invalid_utf8_secrets_are_redacted_after_a_lossy_render() {
    let redactor = Redactor::default();
    redactor.register(b"ab\xffcd");

    let rendered = String::from_utf8_lossy(b"leaked ab\xffcd here").into_owned();

    assert_eq!(redactor.redact(&rendered), "leaked [REDACTED] here");
}

#[test]
fn non_utf8_environment_values_are_redacted() {
    let redactor = Redactor::default();
    let environment = BTreeMap::from([(
        OsString::from("APP_TOKEN"),
        OsStr::from_bytes(b"se\xffcret").to_owned(),
    )]);

    redactor.register_sensitive_environment(&environment);

    assert_eq!(
        redactor.redact_bytes(b"printed se\xffcret"),
        b"printed [REDACTED]"
    );
}

#[test]
fn encoded_forms_of_a_binary_secret_are_masked() {
    let redactor = Redactor::default();
    redactor.register(b"ab\xffcd");

    assert_eq!(
        redactor.redact("YWL/Y2Q= YWL/Y2Q YWL_Y2Q ab%FFcd ab%ffcd"),
        "[REDACTED] [REDACTED] [REDACTED] [REDACTED] [REDACTED]"
    );
}

#[test]
fn a_binary_secret_is_never_split_across_two_frames() {
    let redactor = Redactor::default();
    let secret = b"pre\xffmid\xfe\xfdsuf";
    redactor.register(secret);
    let pending = b"log: pre\xffmid\xfe\xfdsuf tail\n";

    let frame_end = redactor
        .safe_frame_end(pending, 10, true)
        .expect("frame end");

    assert_eq!(frame_end, 5);
    let mut emitted = redactor.redact_bytes(&pending[..frame_end]);
    emitted.extend_from_slice(&redactor.redact_bytes(&pending[frame_end..]));
    assert_eq!(emitted, b"log: [REDACTED] tail\n");
}
