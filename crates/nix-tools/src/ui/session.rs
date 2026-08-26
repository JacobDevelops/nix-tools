use std::io::{self, IsTerminal};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use nix_tools_core::process::Cancellation;
use nix_tools_engine::{Manifest, NodeState, ProgressEvent, ProgressSink};
use ratatui::{Terminal, backend::CrosstermBackend};

use super::{model::Model, view::render};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayMode {
    Tui,
    Stream,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayContext<'a> {
    pub interactive_io: bool,
    pub term: Option<&'a str>,
    pub automated: bool,
    pub disabled: bool,
}

impl DisplayMode {
    pub fn select(context: DisplayContext<'_>) -> Self {
        if context.interactive_io
            && context
                .term
                .is_some_and(|term| !term.is_empty() && term != "dumb")
            && !context.automated
            && !context.disabled
        {
            Self::Tui
        } else {
            Self::Stream
        }
    }
}

enum Message {
    Progress(ProgressEvent),
    Finished(Option<Box<Manifest>>),
}

enum UiProgress {
    Tui(Sender<Message>),
    Stream,
}

impl ProgressSink for UiProgress {
    fn emit(&self, event: ProgressEvent) {
        match self {
            Self::Tui(sender) => drop(sender.send(Message::Progress(event))),
            Self::Stream => render_stream_event(event),
        }
    }
}

pub struct UiSession {
    progress: UiProgress,
    thread: Option<JoinHandle<()>>,
    mode: DisplayMode,
    title: String,
}

impl UiSession {
    pub fn detect(title: impl Into<String>, cancellation: Cancellation, disabled: bool) -> Self {
        let term = std::env::var("TERM").ok();
        let mode = DisplayMode::select(DisplayContext {
            interactive_io: io::stdin().is_terminal() && io::stderr().is_terminal(),
            term: term.as_deref(),
            automated: std::env::var_os("CI").is_some(),
            disabled,
        });
        Self::new(title.into(), cancellation, mode)
    }

    pub const fn progress(&self) -> &dyn ProgressSink {
        &self.progress
    }

    pub fn finish(&mut self, manifest: Option<&Manifest>) {
        if let UiProgress::Tui(sender) = &self.progress {
            drop(sender.send(Message::Finished(manifest.cloned().map(Box::new))));
        }
        self.join();
        if self.mode == DisplayMode::Tui
            && let Some(manifest) = manifest
        {
            eprintln!("{}", completion_summary(&self.title, manifest));
        }
    }

    fn new(title: String, cancellation: Cancellation, mode: DisplayMode) -> Self {
        match mode {
            DisplayMode::Tui => {
                let (sender, receiver) = mpsc::channel();
                let (startup_sender, startup_receiver) = mpsc::sync_channel(0);
                let thread_title = title.clone();
                let thread = thread::spawn(move || {
                    run_tui(thread_title, &cancellation, &receiver, &startup_sender);
                });
                if startup_receiver.recv() == Ok(true) {
                    Self {
                        progress: UiProgress::Tui(sender),
                        thread: Some(thread),
                        mode,
                        title,
                    }
                } else {
                    drop(thread.join());
                    Self {
                        progress: UiProgress::Stream,
                        thread: None,
                        mode: DisplayMode::Stream,
                        title,
                    }
                }
            }
            DisplayMode::Stream => Self {
                progress: UiProgress::Stream,
                thread: None,
                mode,
                title,
            },
        }
    }

    fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

impl Drop for UiSession {
    fn drop(&mut self) {
        if self.thread.is_some() {
            if let UiProgress::Tui(sender) = &self.progress {
                drop(sender.send(Message::Finished(None)));
            }
            self.join();
        }
    }
}

fn run_tui(
    title: String,
    cancellation: &Cancellation,
    receiver: &Receiver<Message>,
    startup: &SyncSender<bool>,
) {
    let Ok(_guard) = TerminalGuard::enter() else {
        let _ = startup.send(false);
        return;
    };
    let backend = CrosstermBackend::new(io::stderr());
    let Ok(mut terminal) = Terminal::new(backend) else {
        let _ = startup.send(false);
        return;
    };
    if startup.send(true).is_err() {
        return;
    }
    let mut model = Model::new(title);
    loop {
        let mut disconnected = false;
        loop {
            match receiver.try_recv() {
                Ok(Message::Progress(event)) => model.apply(event),
                Ok(Message::Finished(manifest)) => {
                    if let Some(manifest) = manifest {
                        model.finish(&manifest);
                    } else {
                        model.complete();
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if terminal.draw(|frame| render(frame, &model)).is_err() {
            break;
        }
        if model.finished() || disconnected {
            break;
        }
        if event::poll(Duration::from_millis(80)).unwrap_or(false)
            && let Ok(Event::Key(key)) = event::read()
            && key.kind == KeyEventKind::Press
        {
            handle_key(&mut model, key, cancellation);
        }
    }
}

pub(super) fn handle_key(model: &mut Model, key: KeyEvent, cancellation: &Cancellation) {
    match key.code {
        KeyCode::Char('q') => cancellation.request(2),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            cancellation.request(2);
        }
        KeyCode::Up | KeyCode::Char('k') => model.select_previous(),
        KeyCode::Down | KeyCode::Char('j') => model.select_next(),
        KeyCode::Char('?') => model.toggle_help(),
        KeyCode::Esc if model.help_visible() => model.toggle_help(),
        _ => {}
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stderr(), EnterAlternateScreen) {
            drop(disable_raw_mode());
            return Err(error);
        }
        let guard = Self;
        execute!(io::stderr(), Hide)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        drop(disable_raw_mode());
        drop(execute!(io::stderr(), Show, LeaveAlternateScreen));
    }
}

fn completion_summary(title: &str, manifest: &Manifest) -> String {
    let mut cached = 0;
    let mut substituted = 0;
    let mut built = 0;
    let mut failed = 0;
    let mut skipped = 0;
    let mut cancelled = 0;
    for node in &manifest.nodes {
        match node.state {
            NodeState::Cached => cached += 1,
            NodeState::Substituted => substituted += 1,
            NodeState::Built => built += 1,
            NodeState::Failed => failed += 1,
            NodeState::Skipped => skipped += 1,
            NodeState::Cancelled => cancelled += 1,
        }
    }
    format!(
        "{title}: {:?} · {} jobs · {cached} cached · {substituted} downloaded · {built} built · {failed} failed · {skipped} skipped · {cancelled} cancelled",
        manifest.outcome,
        manifest.nodes.len(),
    )
}

fn render_stream_event(event: ProgressEvent) {
    match event {
        ProgressEvent::PhaseStarted(phase) => eprintln!("nix-tools: {phase:?} started"),
        ProgressEvent::PhaseFinished(phase) => eprintln!("nix-tools: {phase:?} finished"),
        ProgressEvent::GraphDiscovered(nodes) => {
            eprintln!("nix-tools: discovered {} derivations", nodes.len());
        }
        ProgressEvent::NodeStarted { drv_path } => eprintln!("nix-tools: realizing {drv_path}"),
        ProgressEvent::NodeFinished { drv_path, state } => {
            eprintln!("nix-tools: {drv_path} {state:?}");
        }
        ProgressEvent::Cancelled { signal } => {
            eprintln!("nix-tools: cancelled by signal {signal}");
        }
    }
}
