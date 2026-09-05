use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nix_tools_core::process::Cancellation;

use super::{
    model::Model,
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
