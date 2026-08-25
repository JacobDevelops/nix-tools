//! Bounded, cancellation-aware child process execution with redacted output relay.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::outcome::{Error, ExitCode, Result};
use crate::redaction::Redactor;
use crate::terminal::{TerminalOutputNormalizer, UnicodeFormatFilter};

/// A flat tick charges every child half the poll interval on average; the wait starts here and
/// doubles up to the configured interval, so a short child is reaped promptly and a long one
/// still costs one wakeup per interval.
const INITIAL_POLL_INTERVAL: Duration = Duration::from_micros(200);
const READER_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Receives each complete line of a child stream while the child is still running.
pub trait LineObserver: Send + Sync {
    /// Receives a complete line, including its trailing newline when present.
    fn line(&self, line: &[u8]);
}

/// Handling policy for one child output stream.
#[derive(Clone)]
pub enum StreamPolicy {
    /// Relay normalized, redacted output without retaining it.
    Inherit,
    /// Drain the stream while retaining at most `limit` leading bytes.
    Capture {
        /// Maximum retained byte count.
        limit: usize,
    },
    /// Relay both child streams in order and retain a bounded combined head and tail.
    ///
    /// This policy must be selected for both stdout and stderr with the same limit.
    RelayAndCapture {
        /// Maximum combined retained byte count.
        limit: usize,
    },
    /// `Capture`, plus every complete line is handed to the observer as it arrives. The capture
    /// keeps the same bound and truncation flag as `Capture`; the observer sees the whole stream
    /// because it is expected to keep only what it needs.
    Observe {
        /// Maximum retained byte count; observation itself remains unbounded by this value.
        limit: usize,
        /// Destination for complete lines as they arrive.
        observer: Arc<dyn LineObserver>,
    },
    /// Drain no bytes and connect the child stream to the null device.
    Discard,
}

impl std::fmt::Debug for StreamPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inherit => formatter.write_str("Inherit"),
            Self::Capture { limit } => formatter
                .debug_struct("Capture")
                .field("limit", limit)
                .finish(),
            Self::RelayAndCapture { limit } => formatter
                .debug_struct("RelayAndCapture")
                .field("limit", limit)
                .finish(),
            Self::Observe { limit, .. } => formatter
                .debug_struct("Observe")
                .field("limit", limit)
                .finish_non_exhaustive(),
            Self::Discard => formatter.write_str("Discard"),
        }
    }
}

impl PartialEq for StreamPolicy {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Inherit, Self::Inherit) | (Self::Discard, Self::Discard) => true,
            (Self::Capture { limit: left }, Self::Capture { limit: right })
            | (Self::RelayAndCapture { limit: left }, Self::RelayAndCapture { limit: right }) => {
                left == right
            }
            (
                Self::Observe {
                    limit: left,
                    observer: left_observer,
                },
                Self::Observe {
                    limit: right,
                    observer: right_observer,
                },
            ) => left == right && Arc::ptr_eq(left_observer, right_observer),
            _ => false,
        }
    }
}

impl Eq for StreamPolicy {}

/// Handling policy for child standard input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputPolicy {
    /// Inherit the parent process's standard input.
    Inherit,
    /// Connect standard input to the null device.
    Null,
    /// Write the supplied bytes through a pipe without exposing them in the argument list.
    Bytes(Vec<u8>),
}

/// Complete, shell-free child process specification.
///
/// The runner clears the inherited environment and passes only `env`, making the execution input
/// explicit and preventing accidental credential inheritance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSpec {
    /// Executable path or name.
    pub program: OsString,
    /// Argument vector, excluding the program.
    pub args: Vec<OsString>,
    /// Optional working directory.
    pub cwd: Option<PathBuf>,
    /// Complete child environment after the runner clears inherited variables.
    pub env: BTreeMap<OsString, OsString>,
    /// Child standard-input policy.
    pub stdin: InputPolicy,
    /// Child standard-output policy.
    pub stdout: StreamPolicy,
    /// Child standard-error policy.
    pub stderr: StreamPolicy,
    /// Time allowed for stream cleanup and graceful process-group termination.
    pub cleanup_timeout: Duration,
}

impl ProcessSpec {
    /// Creates a process spec with inherited streams, an empty environment, and a two-second cleanup.
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            stdin: InputPolicy::Inherit,
            stdout: StreamPolicy::Inherit,
            stderr: StreamPolicy::Inherit,
            cleanup_timeout: Duration::from_secs(2),
        }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, value: impl Into<OsString>) -> Self {
        self.args.push(value.into());
        self
    }

    /// Appends an argument sequence.
    #[must_use]
    pub fn args<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(values.into_iter().map(Into::into));
        self
    }

    /// Sets the child working directory.
    #[must_use]
    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }

    /// Adds or replaces one explicit child environment variable.
    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
}

/// Bounded capture of one output stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapturedStream {
    /// Retained leading bytes.
    pub bytes: Vec<u8>,
    /// Whether bytes beyond the configured limit were drained but not retained.
    pub truncated: bool,
}

/// Bounded capture retaining both the beginning and end of ordered combined output.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CombinedStream {
    /// Retained leading bytes.
    pub head: Vec<u8>,
    /// Retained trailing bytes.
    pub tail: Vec<u8>,
    /// Count of drained bytes omitted between `head` and `tail`.
    pub omitted_bytes: usize,
}

impl CombinedStream {
    /// Returns whether bytes were omitted.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.omitted_bytes > 0
    }

    /// Concatenates the retained head and tail without an omission marker.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        let mut bytes = self.head;
        bytes.extend(self.tail);
        bytes
    }
}

/// How the operating system reports that a child ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildTermination {
    /// Normal exit with the supplied status.
    Exited(i32),
    /// Termination by the supplied signal number.
    Signaled(i32),
    /// No portable status or signal was available.
    Unknown,
}

impl ChildTermination {
    /// Returns whether the child exited normally with status zero.
    #[must_use]
    pub const fn success(self) -> bool {
        matches!(self, Self::Exited(0))
    }

    /// Converts this termination to a portable process exit status.
    #[must_use]
    pub fn exit_code(self) -> ExitCode {
        match self {
            Self::Exited(code) => ExitCode::from_child_code(code),
            Self::Signaled(signal) => ExitCode::from_signal(signal),
            Self::Unknown => ExitCode::FAILURE,
        }
    }
}

/// Result of a completed child process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessResult {
    /// Child termination status.
    pub termination: ChildTermination,
    /// Independently captured stdout, empty when stdout was not captured separately.
    pub stdout: CapturedStream,
    /// Independently captured stderr, empty when stderr was not captured separately.
    pub stderr: CapturedStream,
    /// Ordered combined capture when both streams selected `RelayAndCapture`.
    pub combined: Option<CombinedStream>,
    /// Wall time from spawn preparation through process exit.
    pub duration: Duration,
}

impl ProcessResult {
    /// # Errors
    ///
    /// Returns an error when the process did not exit successfully.
    pub fn require_success(self, program: &OsStr) -> Result<Self> {
        if self.termination.success() {
            return Ok(self);
        }
        let program = program.to_string_lossy();
        let message = match self.termination {
            ChildTermination::Exited(code) => format!("{program} exited with status {code}"),
            ChildTermination::Signaled(signal) => {
                format!("{program} terminated by signal {signal}")
            }
            ChildTermination::Unknown => format!("{program} ended without an exit status"),
        };
        Err(Error::child(self.termination.exit_code(), message))
    }
}

#[derive(Debug, Default)]
struct CancellationGate {
    signal: Option<i32>,
    pending_signal: Option<i32>,
    committing: bool,
}

#[derive(Debug, Default)]
struct CancellationState {
    gate: Mutex<CancellationGate>,
    changed: Condvar,
}

struct CommitGuard<'a>(&'a CancellationState);

impl Drop for CommitGuard<'_> {
    fn drop(&mut self) {
        let mut gate = self
            .0
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gate.committing = false;
        if gate.signal.is_none() {
            gate.signal = gate.pending_signal;
        }
        gate.pending_signal = None;
        self.0.changed.notify_all();
    }
}

/// Cloneable cancellation token shared by process and atomic-publication operations.
///
/// Its commit gate gives cancellation and an irreversible commit point a total order: cancellation
/// wins before commit starts, while a request arriving during commit waits for visible success.
#[derive(Clone, Debug, Default)]
pub struct Cancellation {
    state: Arc<CancellationState>,
}

impl Cancellation {
    /// Requests cancellation with a signal number; the first request wins.
    pub fn request(&self, signal: i32) {
        self.request_with(signal, || {});
    }

    fn request_with(&self, signal: i32, entered: impl FnOnce()) {
        let mut gate = self
            .state
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if gate.signal.is_none() && gate.pending_signal.is_none() {
            gate.pending_signal = Some(signal);
        }
        entered();
        while gate.committing {
            gate = self
                .state
                .changed
                .wait(gate)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if gate.signal.is_none() {
            gate.signal = gate.pending_signal;
        }
        gate.pending_signal = None;
    }

    #[cfg(test)]
    pub(crate) fn request_after_entering_gate(&self, signal: i32, entered: impl FnOnce()) {
        self.request_with(signal, entered);
    }

    /// Returns the requested signal, including one waiting for an active commit to finish.
    #[must_use]
    pub fn signal(&self) -> Option<i32> {
        let gate = self
            .state
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gate.signal.or(gate.pending_signal)
    }

    /// Runs `commit` only when cancellation has not already won the commit gate.
    ///
    /// A concurrent cancellation request waits until `commit` returns. `None` means cancellation
    /// won before the closure began; `Some` means the closure completed as the authoritative action.
    pub fn commit_if_not_cancelled<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
        let mut gate = self
            .state
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while gate.committing {
            gate = self
                .state
                .changed
                .wait(gate)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if gate.signal.is_some() || gate.pending_signal.is_some() {
            return None;
        }
        gate.committing = true;
        drop(gate);
        let _guard = CommitGuard(&self.state);
        Some(commit())
    }
}

/// Injectable child-process runner boundary.
pub trait ProcessRunner: Send + Sync {
    /// # Errors
    ///
    /// Returns an error when the process cannot be spawned or is cancelled; a
    /// child that exits with a non-zero status is `Ok`.
    fn run(&self, spec: &ProcessSpec, cancellation: &Cancellation) -> Result<ProcessResult>;
}

/// Destination stream supplied to a process-output relay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessStream {
    /// Child standard output.
    Stdout,
    /// Child standard error.
    Stderr,
    /// Ordered stdout and stderr sharing one pipe.
    Combined,
}

/// Sink for normalized and redacted child output.
pub trait ProcessOutputRelay: Send + Sync {
    /// # Errors
    ///
    /// Returns an error when the bytes cannot all be handed to the destination;
    /// a relay that intentionally discards output reports success. The caller
    /// treats that error as a lost echo, never as a failure of the child.
    fn write(&self, stream: ProcessStream, bytes: &[u8]) -> io::Result<()>;
}

/// Relays child stdout to this process's stdout and diagnostics/combined output to stderr.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdProcessOutputRelay;

impl ProcessOutputRelay for StdProcessOutputRelay {
    fn write(&self, stream: ProcessStream, bytes: &[u8]) -> io::Result<()> {
        match stream {
            ProcessStream::Stdout => std::io::stdout().lock().write_all(bytes),
            ProcessStream::Stderr | ProcessStream::Combined => {
                std::io::stderr().lock().write_all(bytes)
            }
        }
    }
}

/// Relay that intentionally discards every byte.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiscardProcessOutputRelay;

impl ProcessOutputRelay for DiscardProcessOutputRelay {
    fn write(&self, _stream: ProcessStream, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

/// Keeps child diagnostics while leaving our own stdout free for structured records.
#[derive(Clone, Copy, Debug, Default)]
pub struct StderrProcessOutputRelay;

impl ProcessOutputRelay for StderrProcessOutputRelay {
    fn write(&self, stream: ProcessStream, bytes: &[u8]) -> io::Result<()> {
        match stream {
            ProcessStream::Stdout => Ok(()),
            ProcessStream::Stderr | ProcessStream::Combined => {
                std::io::stderr().lock().write_all(bytes)
            }
        }
    }
}

/// Standard process runner using a dedicated process group and bounded cleanup.
#[derive(Clone)]
pub struct StdProcessRunner {
    poll_interval: Duration,
    redactor: Redactor,
    relay: Arc<dyn ProcessOutputRelay>,
}

impl StdProcessRunner {
    /// Creates a runner that relays through [`StdProcessOutputRelay`].
    #[must_use]
    pub fn new(poll_interval: Duration, redactor: Redactor) -> Self {
        Self::with_output(poll_interval, redactor, Arc::new(StdProcessOutputRelay))
    }

    /// Creates a runner with an injectable output relay.
    #[must_use]
    pub fn with_output(
        poll_interval: Duration,
        redactor: Redactor,
        relay: Arc<dyn ProcessOutputRelay>,
    ) -> Self {
        Self {
            poll_interval,
            redactor,
            relay,
        }
    }

    /// Creates a runner that still drains and redacts output but does not echo it.
    #[must_use]
    pub fn without_output(poll_interval: Duration, redactor: Redactor) -> Self {
        Self::with_output(poll_interval, redactor, Arc::new(DiscardProcessOutputRelay))
    }
}

impl ProcessRunner for StdProcessRunner {
    fn run(&self, spec: &ProcessSpec, cancellation: &Cancellation) -> Result<ProcessResult> {
        check_before_spawn(spec, cancellation)?;
        self.redactor.register_sensitive_environment(&spec.env);
        let started = Instant::now();
        let SpawnedProcess {
            mut child,
            stdin_writer,
            stdout_reader,
            stderr_reader,
            combined_reader,
            combined_capture,
        } = spawn_process(spec, cancellation, &self.redactor, &self.relay)?;
        let mut cancellation_signal = None;
        let mut poll = INITIAL_POLL_INTERVAL.min(self.poll_interval);
        let status = loop {
            if let Some(signal) = cancellation.signal() {
                cancellation_signal = Some(signal);
                terminate_process_group(&mut child, spec.cleanup_timeout, self.poll_interval);
                break child.wait().map_err(|error| {
                    Error::io(format!(
                        "reap {} after cancellation: {error}",
                        spec.program.to_string_lossy()
                    ))
                })?;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    if let Some(signal) = cancellation.signal() {
                        cancellation_signal = Some(signal);
                        terminate_process_group(
                            &mut child,
                            spec.cleanup_timeout,
                            self.poll_interval,
                        );
                    }
                    break status;
                }
                Ok(None) => {}
                Err(error) => {
                    terminate_process_group(&mut child, spec.cleanup_timeout, self.poll_interval);
                    return Err(Error::io(format!(
                        "wait for {}: {error}",
                        spec.program.to_string_lossy()
                    )));
                }
            }
            thread::sleep(poll);
            poll = (poll * 2).min(self.poll_interval);
        };

        if cancellation_signal.is_none() {
            terminate_process_group(&mut child, spec.cleanup_timeout, self.poll_interval);
        }

        let writer_result = join_writer(stdin_writer, spec.cleanup_timeout);
        let stdout_result = join_reader(stdout_reader, spec.cleanup_timeout);
        let stderr_result = join_reader(stderr_reader, spec.cleanup_timeout);
        let combined_result = join_reader(combined_reader, spec.cleanup_timeout);

        if let Some(signal) = cancellation_signal {
            return Err(Error::cancelled(
                signal,
                format!(
                    "{} cancelled by signal {signal}",
                    spec.program.to_string_lossy()
                ),
            ));
        }

        let termination = child_termination(status);
        writer_result?;
        let stdout = stdout_result?;
        let stderr = stderr_result?;
        combined_result?;
        let combined = combined_capture.map(|capture| {
            capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .snapshot()
        });
        Ok(ProcessResult {
            termination,
            stdout,
            stderr,
            combined,
            duration: started.elapsed(),
        })
    }
}

struct SpawnedProcess {
    child: std::process::Child,
    stdin_writer: Option<mpsc::Receiver<io::Result<()>>>,
    stdout_reader: Option<ReaderHandle>,
    stderr_reader: Option<ReaderHandle>,
    combined_reader: Option<ReaderHandle>,
    combined_capture: Option<Arc<Mutex<CombinedCaptureBuffer>>>,
}

fn check_before_spawn(spec: &ProcessSpec, cancellation: &Cancellation) -> Result<()> {
    if let Some(signal) = cancellation.signal() {
        return Err(Error::cancelled(
            signal,
            format!(
                "{} cancelled by signal {signal} before start",
                spec.program.to_string_lossy()
            ),
        ));
    }
    Ok(())
}

fn spawn_process(
    spec: &ProcessSpec,
    cancellation: &Cancellation,
    redactor: &Redactor,
    relay: &Arc<dyn ProcessOutputRelay>,
) -> Result<SpawnedProcess> {
    spawn_process_with_hook(spec, cancellation, redactor, relay, || {})
}

fn spawn_process_with_hook(
    spec: &ProcessSpec,
    cancellation: &Cancellation,
    redactor: &Redactor,
    relay: &Arc<dyn ProcessOutputRelay>,
    before_spawn: impl FnOnce(),
) -> Result<SpawnedProcess> {
    let program = resolve_program(spec)?;
    let mut command = Command::new(&program);
    command.env_clear().args(&spec.args).envs(&spec.env);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    configure_input(&mut command, &spec.stdin);
    let combined_limit = combined_capture_limit(&spec.stdout, &spec.stderr)?;
    let (combined_stream, combined_capture) = if let Some(limit) = combined_limit {
        let reader = configure_combined_stream(&mut command)?;
        (
            Some(reader),
            Some(Arc::new(Mutex::new(CombinedCaptureBuffer::new(limit)))),
        )
    } else {
        command.stdout(stdio_for(&spec.stdout));
        command.stderr(stdio_for(&spec.stderr));
        (None, None)
    };
    #[cfg(unix)]
    command.process_group(0);
    check_before_spawn(spec, cancellation)?;
    before_spawn();

    let Some(spawn_result) = cancellation.commit_if_not_cancelled(|| command.spawn()) else {
        check_before_spawn(spec, cancellation)?;
        return Err(Error::internal(
            "spawn commit gate closed without cancellation",
        ));
    };
    let mut child = spawn_result
        .map_err(|error| Error::io(format!("start {}: {error}", spec.program.to_string_lossy())))?;
    drop(command);
    let stdin_writer = spawn_writer(&spec.stdin, child.stdin.take());
    let (stdout_reader, stderr_reader, combined_reader) =
        if let (Some(reader), Some(capture)) = (combined_stream, combined_capture.as_ref()) {
            (
                None,
                None,
                Some(spawn_combined_reader(
                    reader,
                    redactor,
                    relay,
                    Arc::clone(capture),
                )),
            )
        } else {
            (
                spawn_reader(
                    child.stdout.take(),
                    &spec.stdout,
                    ProcessStream::Stdout,
                    redactor,
                    relay,
                ),
                spawn_reader(
                    child.stderr.take(),
                    &spec.stderr,
                    ProcessStream::Stderr,
                    redactor,
                    relay,
                ),
                None,
            )
        };
    Ok(SpawnedProcess {
        child,
        stdin_writer,
        stdout_reader,
        stderr_reader,
        combined_reader,
        combined_capture,
    })
}

fn resolve_program(spec: &ProcessSpec) -> Result<OsString> {
    let requested = Path::new(&spec.program);
    if requested.components().count() != 1 {
        return Ok(spec.program.clone());
    }
    let search_path = spec
        .env
        .get(OsStr::new("PATH"))
        .cloned()
        .or_else(|| std::env::var_os("PATH"));
    let Some(search_path) = search_path else {
        return Ok(spec.program.clone());
    };
    let parent_cwd = std::env::current_dir()
        .map_err(|error| Error::io(format!("resolve current directory: {error}")))?;
    let child_cwd = spec.cwd.as_ref().map_or_else(
        || parent_cwd.clone(),
        |cwd| {
            if cwd.is_absolute() {
                cwd.clone()
            } else {
                parent_cwd.join(cwd)
            }
        },
    );
    for directory in std::env::split_paths(&search_path) {
        let directory = if directory.as_os_str().is_empty() {
            child_cwd.clone()
        } else if directory.is_absolute() {
            directory
        } else {
            child_cwd.join(directory)
        };
        let candidate = directory.join(requested);
        let Ok(metadata) = candidate.metadata() else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return Ok(candidate.into_os_string());
        }
    }
    Ok(spec.program.clone())
}

fn spawn_writer(
    policy: &InputPolicy,
    stdin: Option<std::process::ChildStdin>,
) -> Option<mpsc::Receiver<io::Result<()>>> {
    let (InputPolicy::Bytes(bytes), Some(mut stdin)) = (policy, stdin) else {
        return None;
    };
    let bytes = bytes.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(stdin.write_all(&bytes));
    });
    Some(receiver)
}

fn configure_input(command: &mut Command, policy: &InputPolicy) {
    command.stdin(match policy {
        InputPolicy::Inherit => Stdio::inherit(),
        InputPolicy::Null => Stdio::null(),
        InputPolicy::Bytes(_) => Stdio::piped(),
    });
}

fn combined_capture_limit(stdout: &StreamPolicy, stderr: &StreamPolicy) -> Result<Option<usize>> {
    match (stdout, stderr) {
        (
            StreamPolicy::RelayAndCapture {
                limit: stdout_limit,
            },
            StreamPolicy::RelayAndCapture {
                limit: stderr_limit,
            },
        ) if stdout_limit == stderr_limit => Ok(Some(*stdout_limit)),
        (StreamPolicy::RelayAndCapture { .. }, StreamPolicy::RelayAndCapture { .. }) => Err(
            Error::internal("combined stdout and stderr capture limits must match"),
        ),
        (StreamPolicy::RelayAndCapture { .. }, _) | (_, StreamPolicy::RelayAndCapture { .. }) => {
            Err(Error::internal(
                "combined capture must be enabled for both stdout and stderr",
            ))
        }
        _ => Ok(None),
    }
}

fn configure_combined_stream(command: &mut Command) -> Result<UnixStream> {
    let (reader, writer) = UnixStream::pair()
        .map_err(|error| Error::io(format!("create combined process output pipe: {error}")))?;
    let stderr_writer = writer
        .try_clone()
        .map_err(|error| Error::io(format!("duplicate combined process output pipe: {error}")))?;
    command.stdout(Stdio::from(OwnedFd::from(writer)));
    command.stderr(Stdio::from(OwnedFd::from(stderr_writer)));
    Ok(reader)
}

fn stdio_for(policy: &StreamPolicy) -> Stdio {
    match policy {
        StreamPolicy::Inherit | StreamPolicy::Capture { .. } | StreamPolicy::Observe { .. } => {
            Stdio::piped()
        }
        StreamPolicy::RelayAndCapture { .. } => {
            unreachable!("combined stream configured separately")
        }
        StreamPolicy::Discard => Stdio::null(),
    }
}

#[derive(Debug)]
struct CombinedCaptureBuffer {
    limit: usize,
    total_bytes: usize,
    stream: CombinedStream,
}

impl CombinedCaptureBuffer {
    const fn new(limit: usize) -> Self {
        Self {
            limit,
            total_bytes: 0,
            stream: CombinedStream {
                head: Vec::new(),
                tail: Vec::new(),
                omitted_bytes: 0,
            },
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        if self.limit == 0 {
            self.stream.omitted_bytes = self.total_bytes;
            return;
        }
        let head_limit = self.limit.div_ceil(2);
        let tail_limit = self.limit - head_limit;
        if self.stream.head.len() < head_limit {
            let retained = (head_limit - self.stream.head.len()).min(bytes.len());
            self.stream.head.extend_from_slice(&bytes[..retained]);
            self.push_tail(&bytes[retained..], tail_limit);
        } else {
            self.push_tail(bytes, tail_limit);
        }
        self.stream.omitted_bytes = self
            .total_bytes
            .saturating_sub(self.stream.head.len() + self.stream.tail.len());
    }

    fn push_tail(&mut self, bytes: &[u8], tail_limit: usize) {
        if tail_limit == 0 || bytes.is_empty() {
            return;
        }
        self.stream.tail.extend_from_slice(bytes);
        if self.stream.tail.len() > tail_limit {
            let excess = self.stream.tail.len() - tail_limit;
            self.stream.tail.drain(..excess);
        }
    }

    fn snapshot(&self) -> CombinedStream {
        self.stream.clone()
    }
}

struct ReaderHandle {
    receiver: mpsc::Receiver<io::Result<CapturedStream>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

fn spawn_reader<R: AsFd + Read + Send + 'static>(
    reader: Option<R>,
    policy: &StreamPolicy,
    stream: ProcessStream,
    redactor: &Redactor,
    relay: &Arc<dyn ProcessOutputRelay>,
) -> Option<ReaderHandle> {
    let reader = reader?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = match policy {
        StreamPolicy::Capture { limit } => {
            let limit = *limit;
            thread::spawn(move || {
                let reader = PollingReader::new(reader, Arc::clone(&worker_cancelled));
                let _ = sender.send(read_bounded(reader, limit, &worker_cancelled));
            })
        }
        StreamPolicy::Inherit => {
            let redactor = redactor.clone();
            let relay = Arc::clone(relay);
            thread::spawn(move || {
                let reader = PollingReader::new(reader, Arc::clone(&worker_cancelled));
                let result = relay_redacted(
                    reader,
                    stream,
                    &redactor,
                    relay.as_ref(),
                    None,
                    &worker_cancelled,
                )
                .map(|()| CapturedStream::default());
                let _ = sender.send(result);
            })
        }
        StreamPolicy::Observe { limit, observer } => {
            let limit = *limit;
            let observer = Arc::clone(observer);
            thread::spawn(move || {
                let reader = PollingReader::new(reader, Arc::clone(&worker_cancelled));
                let _ = sender.send(read_observed(
                    reader,
                    limit,
                    observer.as_ref(),
                    &worker_cancelled,
                ));
            })
        }
        StreamPolicy::RelayAndCapture { .. } => {
            unreachable!("combined stream reader configured separately");
        }
        StreamPolicy::Discard => return None,
    };
    Some(ReaderHandle {
        receiver,
        cancelled,
        worker,
    })
}

fn spawn_combined_reader(
    reader: UnixStream,
    redactor: &Redactor,
    relay: &Arc<dyn ProcessOutputRelay>,
    capture: Arc<Mutex<CombinedCaptureBuffer>>,
) -> ReaderHandle {
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let redactor = redactor.clone();
    let relay = Arc::clone(relay);
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let reader = PollingReader::new(reader, Arc::clone(&worker_cancelled));
        let result = relay_redacted(
            reader,
            ProcessStream::Combined,
            &redactor,
            relay.as_ref(),
            Some(&capture),
            &worker_cancelled,
        )
        .map(|()| CapturedStream::default());
        let _ = sender.send(result);
    });
    ReaderHandle {
        receiver,
        cancelled,
        worker,
    }
}

struct PollingReader<R> {
    reader: R,
    cancelled: Arc<AtomicBool>,
}

impl<R> PollingReader<R> {
    fn new(reader: R, cancelled: Arc<AtomicBool>) -> Self {
        Self { reader, cancelled }
    }
}

impl<R: AsFd + Read> Read for PollingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.cancelled.load(Ordering::Relaxed) {
                return Ok(0);
            }
            let mut descriptors = [PollFd::new(self.reader.as_fd(), PollFlags::POLLIN)];
            let timeout = PollTimeout::try_from(READER_POLL_INTERVAL).map_err(io::Error::other)?;
            match poll(&mut descriptors, timeout) {
                Ok(0) | Err(Errno::EINTR) => {}
                Ok(_) if self.cancelled.load(Ordering::Relaxed) => return Ok(0),
                Ok(_) => return self.reader.read(buffer),
                Err(error) => return Err(io::Error::other(error)),
            }
        }
    }
}

fn relay_redacted(
    mut reader: impl Read,
    stream: ProcessStream,
    redactor: &Redactor,
    relay: &dyn ProcessOutputRelay,
    capture: Option<&Arc<Mutex<CombinedCaptureBuffer>>>,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    const FRAME_LIMIT: usize = 8 * 1024;

    let mut pending = Vec::with_capacity(FRAME_LIMIT);
    let mut terminal_safe = Vec::with_capacity(FRAME_LIMIT);
    let mut buffer = [0_u8; 4096];
    let mut normalizer = TerminalOutputNormalizer::default();
    let mut format_filter = UnicodeFormatFilter::default();
    let mut suppress_sensitive_continuation = false;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        normalizer.push(&buffer[..read], &mut terminal_safe);
        format_filter.push(&terminal_safe, &mut pending);
        terminal_safe.clear();
        drain_suppressed(
            &mut pending,
            &mut suppress_sensitive_continuation,
            relay,
            stream,
            capture,
        );
        while !suppress_sensitive_continuation {
            let Some(frame_end) = next_frame_end(&pending, FRAME_LIMIT, redactor) else {
                break;
            };
            suppress_sensitive_continuation = !pending[..frame_end].contains(&b'\n')
                && (Redactor::contains_sensitive_assignment(&pending[..frame_end])
                    || Redactor::contains_sensitive_assignment_prefix(&pending[..frame_end]));
            relay_frame(
                relay,
                stream,
                redactor,
                &pending[..frame_end],
                FRAME_LIMIT,
                capture,
            );
            pending.drain(..frame_end);
            drain_suppressed(
                &mut pending,
                &mut suppress_sensitive_continuation,
                relay,
                stream,
                capture,
            );
        }
    }
    normalizer.finish(&mut terminal_safe);
    format_filter.push(&terminal_safe, &mut pending);
    format_filter.finish(&mut pending);
    drain_suppressed(
        &mut pending,
        &mut suppress_sensitive_continuation,
        relay,
        stream,
        capture,
    );
    while !pending.is_empty() && !suppress_sensitive_continuation {
        let candidate = newline_or_limit_end(&pending, FRAME_LIMIT).unwrap_or(pending.len());
        let frame_end = redactor
            .safe_frame_end(&pending, candidate, true)
            .unwrap_or(candidate);
        relay_frame(
            relay,
            stream,
            redactor,
            &pending[..frame_end],
            FRAME_LIMIT,
            capture,
        );
        pending.drain(..frame_end);
    }
    Ok(())
}

fn drain_suppressed(
    pending: &mut Vec<u8>,
    suppress: &mut bool,
    relay: &dyn ProcessOutputRelay,
    stream: ProcessStream,
    capture: Option<&Arc<Mutex<CombinedCaptureBuffer>>>,
) {
    if !*suppress {
        return;
    }
    if let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
        pending.drain(..=newline);
        relay_and_capture(relay, stream, b"\n", capture);
        *suppress = false;
    } else {
        pending.clear();
    }
}

fn next_frame_end(pending: &[u8], limit: usize, redactor: &Redactor) -> Option<usize> {
    newline_or_limit_end(pending, limit)
        .and_then(|candidate| redactor.safe_frame_end(pending, candidate, false))
}

fn newline_or_limit_end(pending: &[u8], limit: usize) -> Option<usize> {
    let search_end = pending.len().min(limit);
    pending[..search_end]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .or_else(|| (pending.len() >= limit).then_some(limit))
}

fn relay_frame(
    relay: &dyn ProcessOutputRelay,
    stream: ProcessStream,
    redactor: &Redactor,
    frame: &[u8],
    limit: usize,
    capture: Option<&Arc<Mutex<CombinedCaptureBuffer>>>,
) {
    let redacted = redactor.redact(&String::from_utf8_lossy(frame));
    for chunk in redacted.as_bytes().chunks(limit) {
        relay_and_capture(relay, stream, chunk, capture);
    }
}

fn relay_and_capture(
    relay: &dyn ProcessOutputRelay,
    stream: ProcessStream,
    bytes: &[u8],
    capture: Option<&Arc<Mutex<CombinedCaptureBuffer>>>,
) {
    // Our own stderr closing must not turn a child that succeeded into a failed command.
    let _ = relay.write(stream, bytes);
    if let Some(capture) = capture {
        capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(bytes);
    }
}

fn read_bounded(
    mut reader: impl Read,
    limit: usize,
    cancelled: &AtomicBool,
) -> io::Result<CapturedStream> {
    let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retained = read.min(remaining);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }
    Ok(CapturedStream { bytes, truncated })
}

/// A line that never ends would otherwise grow the frame forever, so an over-long one is handed
/// over in pieces.
const OBSERVED_LINE_LIMIT: usize = 1024 * 1024;

fn read_observed(
    mut reader: impl Read,
    limit: usize,
    observer: &dyn LineObserver,
    cancelled: &AtomicBool,
) -> io::Result<CapturedStream> {
    let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
    let mut buffer = [0_u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retained = read.min(remaining);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
        pending.extend_from_slice(&buffer[..read]);
        loop {
            let end = match pending.iter().position(|byte| *byte == b'\n') {
                Some(index) => index + 1,
                None if pending.len() >= OBSERVED_LINE_LIMIT => OBSERVED_LINE_LIMIT,
                None => break,
            };
            observer.line(&pending[..end]);
            pending.drain(..end);
        }
    }
    if !pending.is_empty() {
        observer.line(&pending);
    }
    Ok(CapturedStream { bytes, truncated })
}

fn join_reader(reader: Option<ReaderHandle>, timeout: Duration) -> Result<CapturedStream> {
    let Some(reader) = reader else {
        return Ok(CapturedStream::default());
    };
    match reader.receiver.recv_timeout(timeout) {
        Ok(result) => {
            join_reader_worker(reader.worker)?;
            result.map_err(|error| Error::io(format!("read process output: {error}")))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            reader.cancelled.store(true, Ordering::Relaxed);
            let wake_timeout = READER_POLL_INTERVAL.saturating_mul(4);
            let wake_result = reader.receiver.recv_timeout(wake_timeout);
            if !matches!(
                wake_result,
                Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected)
            ) {
                return Err(Error::io(
                    "process output reader did not stop after cancellation",
                ));
            }
            join_reader_worker(reader.worker)?;
            Err(Error::io("process output did not close during cleanup"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            join_reader_worker(reader.worker)?;
            Err(Error::internal(
                "process output reader stopped unexpectedly",
            ))
        }
    }
}

fn join_reader_worker(worker: thread::JoinHandle<()>) -> Result<()> {
    worker
        .join()
        .map_err(|_| Error::internal("process output reader panicked"))
}

fn join_writer(writer: Option<mpsc::Receiver<io::Result<()>>>, timeout: Duration) -> Result<()> {
    let Some(writer) = writer else {
        return Ok(());
    };
    match writer.recv_timeout(timeout) {
        Ok(result) => result.map_err(|error| Error::io(format!("write process input: {error}"))),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err(Error::io("process input did not close during cleanup"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(Error::internal("process input writer stopped unexpectedly"))
        }
    }
}

fn terminate_process_group(
    child: &mut std::process::Child,
    timeout: Duration,
    poll_interval: Duration,
) {
    let Ok(pid) = i32::try_from(child.id()) else {
        let _ = child.kill();
        return;
    };
    let group = Pid::from_raw(-pid);
    if kill(group, None).is_err() {
        return;
    }
    let _ = kill(group, Signal::SIGTERM);
    if wait_for_group_exit(child, group, timeout, poll_interval) {
        return;
    }
    let _ = kill(group, Signal::SIGKILL);
    wait_for_group_exit(child, group, timeout, poll_interval);
}

fn wait_for_group_exit(
    child: &mut std::process::Child,
    group: Pid,
    timeout: Duration,
    poll_interval: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let _ = child.try_wait();
        if kill(group, None).is_err() {
            return true;
        }
        thread::sleep(poll_interval);
    }
    false
}

fn child_termination(status: std::process::ExitStatus) -> ChildTermination {
    if let Some(code) = status.code() {
        ChildTermination::Exited(code)
    } else {
        #[cfg(unix)]
        {
            status
                .signal()
                .map_or(ChildTermination::Unknown, ChildTermination::Signaled)
        }
        #[cfg(not(unix))]
        {
            ChildTermination::Unknown
        }
    }
}

#[cfg(test)]
#[path = "process_test.rs"]
mod process_test;
