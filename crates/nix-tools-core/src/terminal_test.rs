use super::{
    TerminalOutputNormalizer, UnicodeFormatFilter, is_visible_output_character,
    normalize_terminal_output,
};

#[test]
fn removes_raw_c1_terminal_sequences_and_unicode_format_controls() {
    let normalized = normalize_terminal_output(
        b"before\x90hidden\x9cafter\x85next\x9b31mred\x9b0m token=se\xe2\x80\xaecret \xf0\x91\x82\xbe",
    );

    assert_eq!(
        normalized,
        b"beforeafter\nnextred token=secret \xf0\x91\x82\xbe"
    );
}

#[test]
fn streaming_normalization_handles_sequences_split_across_chunks() {
    let mut normalizer = TerminalOutputNormalizer::default();
    let mut terminal_safe = Vec::new();
    normalizer.push(b"se\x1b[", &mut terminal_safe);
    normalizer.push(b"31mcr\xe2\x80", &mut terminal_safe);
    normalizer.push(b"\x8det", &mut terminal_safe);
    normalizer.finish(&mut terminal_safe);

    let mut filter = UnicodeFormatFilter::default();
    let mut output = Vec::new();
    for chunk in terminal_safe.chunks(2) {
        filter.push(chunk, &mut output);
    }
    filter.finish(&mut output);

    assert_eq!(output, b"secret");
}

#[test]
fn visible_kaithi_punctuation_is_not_treated_as_formatting() {
    assert!(is_visible_output_character('\u{110be}'));
    assert!(!is_visible_output_character('\u{110bd}'));
    assert!(!is_visible_output_character('\u{110cd}'));
}

#[test]
fn default_ignorable_interlinear_controls_are_removed() {
    assert_eq!(
        normalize_terminal_output("se\u{fff0}cret".as_bytes()),
        b"secret"
    );
    assert_eq!(
        normalize_terminal_output("se\u{fff8}cret".as_bytes()),
        b"secret"
    );
}
