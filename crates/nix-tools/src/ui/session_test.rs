use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nix_tools_core::process::Cancellation;

use super::{
    model::Model,
    session::{DisplayContext, DisplayMode, handle_key},
};

#[test]
fn tui_is_automatic_only_for_an_interactive_terminal() {
    assert_eq!(
        DisplayMode::select(DisplayContext {
            interactive_io: true,
            term: Some("xterm-256color"),
            automated: false,
            disabled: false,
        }),
        DisplayMode::Tui
    );

    for context in [
        DisplayContext {
            interactive_io: false,
            term: Some("xterm-256color"),
            automated: false,
            disabled: false,
        },
        DisplayContext {
            interactive_io: true,
            term: Some("dumb"),
            automated: false,
            disabled: false,
        },
        DisplayContext {
            interactive_io: true,
            term: Some("xterm-256color"),
            automated: true,
            disabled: false,
        },
        DisplayContext {
            interactive_io: true,
            term: Some("xterm-256color"),
            automated: false,
            disabled: true,
        },
    ] {
        assert_eq!(DisplayMode::select(context), DisplayMode::Stream);
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
