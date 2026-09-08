use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nix_tools_core::process::Cancellation;

use super::{
    model::{JobFilter, Model},
    session::{DisplayContext, OutputMode, handle_key},
};

#[test]
fn explicit_tui_requires_an_interactive_terminal() {
    assert_eq!(
        OutputMode::select(
            OutputMode::Tui,
            DisplayContext {
                interactive_io: true,
                term: Some("xterm-256color"),
            },
        ),
        OutputMode::Tui
    );

    assert_eq!(
        OutputMode::select(
            OutputMode::Stream,
            DisplayContext {
                interactive_io: true,
                term: Some("xterm-256color"),
            },
        ),
        OutputMode::Stream
    );

    for context in [
        DisplayContext {
            interactive_io: false,
            term: Some("xterm-256color"),
        },
        DisplayContext {
            interactive_io: true,
            term: Some("dumb"),
        },
    ] {
        assert_eq!(
            OutputMode::select(OutputMode::Tui, context),
            OutputMode::Stream
        );
    }
}

#[test]
fn control_c_requests_cancellation() {
    let cancellation = Cancellation::default();
    let mut model = Model::new("check");

    handle_key(
        &mut model,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &cancellation,
    );
    assert_eq!(cancellation.signal(), Some(2));
}

#[test]
fn slash_edits_a_filter_without_triggering_normal_mode_keys() {
    let cancellation = Cancellation::default();
    let mut model = Model::new("check");

    for code in [
        KeyCode::Char('/'),
        KeyCode::Char('j'),
        KeyCode::Char('o'),
        KeyCode::Char('b'),
    ] {
        handle_key(
            &mut model,
            KeyEvent::new(code, KeyModifiers::NONE),
            &cancellation,
        );
    }
    assert_eq!(model.filter_query(), "job");
    assert!(model.filter_input_active());

    handle_key(
        &mut model,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &cancellation,
    );
    assert!(!model.filter_input_active());
}

#[test]
fn normal_mode_cycles_status_filters_and_escape_clears_them() {
    let cancellation = Cancellation::default();
    let mut model = Model::new("check");

    handle_key(
        &mut model,
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
        &cancellation,
    );
    assert_eq!(model.job_filter(), JobFilter::Active);
    handle_key(
        &mut model,
        KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT),
        &cancellation,
    );
    assert_eq!(model.job_filter(), JobFilter::All);

    handle_key(
        &mut model,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &cancellation,
    );
    assert_eq!(model.job_filter(), JobFilter::All);
    assert_eq!(model.filter_query(), "");
}
