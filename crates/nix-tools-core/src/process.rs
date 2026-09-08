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

const WORKER_STOP_TIMEOUT: Duration = Duration::from_millis(100);

/// Receives each complete line of a child stream while the child is still running.
pub trait LineObserver: Send + Sync {
    /// Receives a complete line, including its trailing newline when present.
    ///
    /// Implementations must return promptly; a blocked callback can outlive bounded cleanup.
    fn line(&self, line: &[u8]);
}

/// Reads one child stream directly while the child is still running.
pub trait StreamConsumer: Send + Sync {
    /// Reads until the consumer has what it needs.
    ///
    /// The bytes are raw: they are neither normalized nor redacted, so a consumer must extract
    /// what it needs rather than relay them. Anything left unread is drained and discarded, so
    /// returning early cannot block the child on a full pipe.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream cannot be read. A consumer that rejects well-read bytes
    /// reports that verdict through its own state instead.
    fn consume(&self, reader: &mut dyn Read) -> io::Result<()>;
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
    /// Hand the stream to a consumer as it arrives and retain nothing.
    ///
    /// Peak memory is whatever the consumer keeps rather than however much the child writes. The
    /// limit is not that memory: it is the ceiling on bytes read from the child at all, because a
    /// parser can allocate from a single value before the consumer ever sees it. It is required so
    /// that no call site can inherit an absent ceiling by omission, and zero admits nothing.
    Consume {
        /// Destination for the stream.
        consumer: Arc<dyn StreamConsumer>,
        /// Maximum bytes read from the child before the read fails.
        limit: usize,
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
            Self::Consume { limit, .. } => formatter
                .debug_struct("Consume")
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
            (
                Self::Consume {
                    consumer: left_consumer,
                    limit: left,
                },
                Self::Consume {
                    consumer: right_consumer,
                    limit: right,
                },
            ) => left == right && Arc::ptr_eq(left_consumer, right_consumer),
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
    wakeups: Vec<std::sync::Weak<UnixStream>>,
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
        gate.wakeups.retain(|wake| {
            if let Some(wake) = wake.upgrade() {
                let _ = wake.shutdown(std::net::Shutdown::Write);
                true
            } else {
                false
            }
        });
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

    fn subscribe(&self) -> io::Result<(UnixStream, Arc<UnixStream>)> {
        let (reader, writer) = UnixStream::pair()?;
        let writer = Arc::new(writer);
        let mut gate = self
            .state
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gate.wakeups.retain(|wake| wake.strong_count() > 0);
        gate.wakeups.push(Arc::downgrade(&writer));
        if gate.signal.or(gate.pending_signal).is_some() {
            let _ = writer.shutdown(std::net::Shutdown::Write);
        }
        Ok((reader, writer))
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

    /// Returns the shared secret registry for redacting decoded structured output.
    fn redactor(&self) -> Redactor {
        Redactor::default()
    }
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
    /// Implementations must return promptly; a blocked callback can outlive bounded cleanup.
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

/// Event-driven process runner using a dedicated process group and bounded cleanup.
///
/// Linux uses pidfds, with one blocking exit-notifier thread when the kernel or sandbox denies
/// them; macOS uses kqueue. Capture and discard need no stream threads. Relay and observation
/// share one bounded callback worker, and each blocking [`StreamConsumer`] has a parser worker.
#[derive(Clone)]
pub struct StdProcessRunner {
    discard_output: bool,
    redactor: Redactor,
    relay: Arc<dyn ProcessOutputRelay>,
}

impl StdProcessRunner {
    /// Creates an event-driven runner relaying through [`StdProcessOutputRelay`].
    ///
    /// The interval argument is retained for source compatibility and no longer controls wakeups.
    #[must_use]
    pub fn new(poll_interval: Duration, redactor: Redactor) -> Self {
        Self::with_output(poll_interval, redactor, Arc::new(StdProcessOutputRelay))
    }

    /// Creates a runner with an injectable output relay; the interval is ignored.
    #[must_use]
    pub fn with_output(
        _poll_interval: Duration,
        redactor: Redactor,
        relay: Arc<dyn ProcessOutputRelay>,
    ) -> Self {
        Self {
            discard_output: false,
            redactor,
            relay,
        }
    }

    /// Creates a runner without echoing; uncaptured inherited output goes directly to the null device.
    /// The interval is ignored.
    #[must_use]
    pub fn without_output(poll_interval: Duration, redactor: Redactor) -> Self {
        Self {
            discard_output: true,
            ..Self::with_output(poll_interval, redactor, Arc::new(DiscardProcessOutputRelay))
        }
    }
}

impl ProcessRunner for StdProcessRunner {
    fn redactor(&self) -> Redactor {
        self.redactor.clone()
    }

    fn run(&self, spec: &ProcessSpec, cancellation: &Cancellation) -> Result<ProcessResult> {
        event::run(self, spec, cancellation)
    }
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
        StreamPolicy::Inherit
        | StreamPolicy::Capture { .. }
        | StreamPolicy::Observe { .. }
        | StreamPolicy::Consume { .. } => Stdio::piped(),
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
}

struct ReaderHandle {
    receiver: mpsc::Receiver<io::Result<CapturedStream>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
    wake: UnixStream,
    finished: UnixStream,
}

fn spawn_reader<R: AsFd + Read + Send + 'static>(
    reader: R,
    consumer: Arc<dyn StreamConsumer>,
    limit: usize,
) -> io::Result<ReaderHandle> {
    let (wake_reader, wake) = UnixStream::pair()?;
    let (finished, completed) = UnixStream::pair()?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("process-consumer".into())
        .spawn(move || {
            let _completed = completed;
            let reader = EventReader::new(reader, worker_cancelled, wake_reader);
            let _ = sender.send(read_consumed(reader, consumer.as_ref(), limit));
        })?;
    Ok(ReaderHandle {
        receiver,
        cancelled,
        worker,
        wake,
        finished,
    })
}

struct EventReader<R> {
    reader: R,
    wake: UnixStream,
    cancelled: Arc<AtomicBool>,
}

impl<R> EventReader<R> {
    fn new(reader: R, cancelled: Arc<AtomicBool>, wake: UnixStream) -> Self {
        Self {
            reader,
            wake,
            cancelled,
        }
    }
}

impl<R: AsFd + Read> Read for EventReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.cancelled.load(Ordering::Relaxed) {
                return Ok(0);
            }
            let mut descriptors = [
                PollFd::new(self.reader.as_fd(), PollFlags::POLLIN),
                PollFd::new(self.wake.as_fd(), PollFlags::POLLIN),
            ];
            match poll(&mut descriptors, PollTimeout::NONE) {
                Ok(0) | Err(Errno::EINTR) => {}
                Ok(_) if self.cancelled.load(Ordering::Relaxed) => return Ok(0),
                Ok(_) => return self.reader.read(buffer),
                Err(error) => return Err(io::Error::other(error)),
            }
        }
    }
}

const FRAME_LIMIT: usize = 8 * 1024;

#[derive(Default)]
struct RelayState {
    pending: Vec<u8>,
    terminal_safe: Vec<u8>,
    normalizer: TerminalOutputNormalizer,
    format_filter: UnicodeFormatFilter,
    suppress_sensitive_continuation: bool,
}

impl RelayState {
    fn push(
        &mut self,
        bytes: &[u8],
        stream: ProcessStream,
        redactor: &Redactor,
        relay: &dyn ProcessOutputRelay,
        capture: Option<&Arc<Mutex<CombinedCaptureBuffer>>>,
    ) {
        self.normalizer.push(bytes, &mut self.terminal_safe);
        self.format_filter
            .push(&self.terminal_safe, &mut self.pending);
        self.terminal_safe.clear();
        drain_suppressed(
            &mut self.pending,
            &mut self.suppress_sensitive_continuation,
            relay,
            stream,
            capture,
        );
        while !self.suppress_sensitive_continuation {
            let Some(frame_end) = next_frame_end(&self.pending, FRAME_LIMIT, redactor) else {
                break;
            };
            self.suppress_sensitive_continuation = !self.pending[..frame_end].contains(&b'\n')
                && (Redactor::contains_sensitive_assignment(&self.pending[..frame_end])
                    || Redactor::contains_sensitive_assignment_prefix(&self.pending[..frame_end]));
            relay_frame(
                relay,
                stream,
                redactor,
                &self.pending[..frame_end],
                FRAME_LIMIT,
                capture,
            );
            self.pending.drain(..frame_end);
            drain_suppressed(
                &mut self.pending,
                &mut self.suppress_sensitive_continuation,
                relay,
                stream,
                capture,
            );
        }
    }
    fn finish(
        &mut self,
        stream: ProcessStream,
        redactor: &Redactor,
        relay: &dyn ProcessOutputRelay,
        capture: Option<&Arc<Mutex<CombinedCaptureBuffer>>>,
    ) {
        self.normalizer.finish(&mut self.terminal_safe);
        self.format_filter
            .push(&self.terminal_safe, &mut self.pending);
        self.format_filter.finish(&mut self.pending);
        drain_suppressed(
            &mut self.pending,
            &mut self.suppress_sensitive_continuation,
            relay,
            stream,
            capture,
        );
        while !self.pending.is_empty() && !self.suppress_sensitive_continuation {
            let candidate =
                newline_or_limit_end(&self.pending, FRAME_LIMIT).unwrap_or(self.pending.len());
            let frame_end = redactor
                .safe_frame_end(&self.pending, candidate, true)
                .unwrap_or(candidate);
            relay_frame(
                relay,
                stream,
                redactor,
                &self.pending[..frame_end],
                FRAME_LIMIT,
                capture,
            );
            self.pending.drain(..frame_end);
        }
    }
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

/// Large enough that a structured stream costs one read syscall per many thousand values, and
/// small enough to stay negligible beside the child itself.
const CONSUMER_BUFFER_BYTES: usize = 256 * 1024;

/// Counts bytes past a ceiling and fails the read there.
///
/// A parser accumulates one JSON string or key into its own scratch buffer before handing it to
/// its visitor, so a per-value check inside the consumer cannot bound a single unterminated value.
/// Only a ceiling at the reader can. The failure is an `io::Error` rather than a short read,
/// because an early EOF is indistinguishable from a truncated document and would be reported as
/// malformed input instead of a limit breach.
struct LimitedReader<R> {
    reader: R,
    remaining: usize,
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            // A stream of exactly the limit is within it, so the ceiling is only breached once
            // another byte actually arrives.
            let mut probe = [0_u8; 1];
            return match self.reader.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::other(
                    "process output exceeded the configured stream limit",
                )),
            };
        }
        let end = buffer.len().min(self.remaining);
        let read = self.reader.read(&mut buffer[..end])?;
        self.remaining -= read;
        Ok(read)
    }
}

fn read_consumed(
    reader: impl Read,
    consumer: &dyn StreamConsumer,
    limit: usize,
) -> io::Result<CapturedStream> {
    if limit == 0 {
        return Err(io::Error::other(
            "process output stream limit must be greater than zero",
        ));
    }
    let mut buffered = io::BufReader::with_capacity(
        CONSUMER_BUFFER_BYTES,
        LimitedReader {
            reader,
            remaining: limit,
        },
    );
    consumer.consume(&mut buffered)?;
    io::copy(&mut buffered, &mut io::sink()).map(|_| CapturedStream::default())
}

/// A line that never ends would otherwise grow the frame forever, so an over-long one is handed
/// over in pieces.
const OBSERVED_LINE_LIMIT: usize = 1024 * 1024;

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
            let _ = reader.wake.shutdown(std::net::Shutdown::Write);
            let wake_result = reader.receiver.recv_timeout(WORKER_STOP_TIMEOUT);
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

mod event;

mod callback;

#[cfg(target_os = "linux")]
mod group_linux;

#[cfg(target_os = "macos")]
mod group_macos;
