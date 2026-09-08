use super::callback::CallbackWorker;
#[cfg(target_os = "linux")]
use super::group_linux as group;
#[cfg(target_os = "macos")]
use super::group_macos as group;
use super::{
    Arc, AsFd, Cancellation, CapturedStream, CombinedCaptureBuffer, Command, CommandExt, Errno,
    Error, InputPolicy, Instant, Mutex, OBSERVED_LINE_LIMIT, OwnedFd, Pid, PollFd, PollFlags,
    PollTimeout, ProcessResult, ProcessSpec, ProcessStream, Read, ReaderHandle, RelayState, Result,
    Signal, StdProcessRunner, StreamPolicy, UnixStream, Write, check_before_spawn,
    child_termination, combined_capture_limit, configure_combined_stream, configure_input, io,
    join_reader, kill, poll, resolve_program, spawn_reader, stdio_for,
};
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use std::fs::File;

struct Output {
    file: File,
    stream: ProcessStream,
    policy: StreamPolicy,
    capture: CapturedStream,
    inline: Option<Box<InlineCapture>>,
    closed: bool,
}

impl Output {
    fn new(fd: OwnedFd, stream: ProcessStream, policy: &StreamPolicy) -> io::Result<Self> {
        nonblocking(&fd)?;
        Ok(Self {
            file: File::from(fd),
            stream,
            policy: policy.clone(),
            capture: CapturedStream::default(),
            inline: None,
            closed: false,
        })
    }

    fn needs_callback(&self) -> bool {
        self.inline.is_none()
            && matches!(
                self.policy,
                StreamPolicy::Inherit
                    | StreamPolicy::Observe { .. }
                    | StreamPolicy::RelayAndCapture { .. }
            )
    }

    fn read(&mut self, callbacks: Option<&CallbackWorker>) -> io::Result<()> {
        let mut buffer = [0_u8; 8192];
        let count = match self.file.read(&mut buffer) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(());
            }
            result => result?,
        };
        self.closed = count == 0;
        let bytes = &buffer[..count];
        if let StreamPolicy::Capture { limit } | StreamPolicy::Observe { limit, .. } = &self.policy
        {
            let retained = count.min(limit.saturating_sub(self.capture.bytes.len()));
            self.capture.bytes.extend_from_slice(&bytes[..retained]);
            self.capture.truncated |= retained < count;
        }
        if let Some(inline) = &mut self.inline {
            inline.deliver(bytes, self.closed);
        }
        if self.needs_callback()
            && !callbacks.is_some_and(|worker| worker.try_send(self.stream, bytes, self.closed))
        {
            return Err(io::Error::other("process callback worker stopped"));
        }
        Ok(())
    }
}

struct InlineCapture {
    relay: RelayState,
    redactor: super::Redactor,
    capture: Arc<Mutex<CombinedCaptureBuffer>>,
}

impl InlineCapture {
    fn deliver(&mut self, bytes: &[u8], eof: bool) {
        if eof {
            self.relay.finish(
                ProcessStream::Combined,
                &self.redactor,
                &super::DiscardProcessOutputRelay,
                Some(&self.capture),
            );
        } else {
            self.relay.push(
                bytes,
                ProcessStream::Combined,
                &self.redactor,
                &super::DiscardProcessOutputRelay,
                Some(&self.capture),
            );
        }
    }
}

struct CallbackOutput {
    stream: ProcessStream,
    policy: StreamPolicy,
    pending: Vec<u8>,
    relay: RelayState,
}

impl CallbackOutput {
    fn deliver(
        &mut self,
        bytes: Option<&[u8]>,
        runner: &StdProcessRunner,
        combined: Option<&Arc<Mutex<CombinedCaptureBuffer>>>,
    ) {
        if let StreamPolicy::Observe { observer, .. } = &self.policy {
            self.pending.extend_from_slice(bytes.unwrap_or_default());
            while let Some(end) = self
                .pending
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|index| index + 1)
                .or_else(|| {
                    (self.pending.len() >= OBSERVED_LINE_LIMIT).then_some(OBSERVED_LINE_LIMIT)
                })
            {
                observer.line(&self.pending[..end]);
                self.pending.drain(..end);
            }
            if bytes.is_none() && !self.pending.is_empty() {
                observer.line(&self.pending);
                self.pending.clear();
            }
        } else if let Some(bytes) = bytes {
            self.relay.push(
                bytes,
                self.stream,
                &runner.redactor,
                runner.relay.as_ref(),
                combined,
            );
        } else {
            self.relay.finish(
                self.stream,
                &runner.redactor,
                runner.relay.as_ref(),
                combined,
            );
        }
    }
}

fn callbacks(
    outputs: &[Output],
    runner: &StdProcessRunner,
    combined: Option<&Arc<Mutex<CombinedCaptureBuffer>>>,
) -> io::Result<Option<CallbackWorker>> {
    let mut states = outputs
        .iter()
        .filter(|output| output.needs_callback())
        .map(|output| CallbackOutput {
            stream: output.stream,
            policy: output.policy.clone(),
            pending: Vec::new(),
            relay: RelayState::default(),
        })
        .collect::<Vec<_>>();
    if states.is_empty() {
        return Ok(None);
    }
    let runner = runner.clone();
    let combined = combined.cloned();
    CallbackWorker::new(move |stream, bytes| {
        if let Some(state) = states.iter_mut().find(|state| state.stream == stream) {
            state.deliver(bytes, &runner, combined.as_ref());
        }
    })
    .map(Some)
}

fn nonblocking(fd: &impl AsFd) -> io::Result<()> {
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::other)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io::Error::other)?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn child_event(child: &std::process::Child) -> io::Result<OwnedFd> {
    child_event_with(child, |pid| {
        rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).map_err(Into::into)
    })
}

#[cfg(target_os = "linux")]
pub(super) fn child_event_with(
    child: &std::process::Child,
    open: impl FnOnce(rustix::process::Pid) -> io::Result<OwnedFd>,
) -> io::Result<OwnedFd> {
    let raw = i32::try_from(child.id()).map_err(io::Error::other)?;
    let pid = rustix::process::Pid::from_raw(raw)
        .ok_or_else(|| io::Error::other("invalid child process identifier"))?;
    match open(pid) {
        Ok(fd) => Ok(fd),
        Err(error) if matches!(error.raw_os_error(), Some(code) if [Errno::ENOSYS as i32, Errno::EINVAL as i32, Errno::EPERM as i32].contains(&code)) => {
            child_wait_event(Pid::from_raw(raw))
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn child_wait_event(pid: Pid) -> io::Result<OwnedFd> {
    use nix::sys::wait::{Id, WaitPidFlag, waitid};
    let (reader, writer) = UnixStream::pair()?;
    std::thread::Builder::new()
        .name("process-exit".into())
        .spawn(move || {
            let _notification = writer;
            while let Err(Errno::EINTR) =
                waitid(Id::Pid(pid), WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT)
            {}
        })?;
    Ok(reader.into())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn child_event(child: &std::process::Child) -> io::Result<OwnedFd> {
    use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
    let queue = Kqueue::new().map_err(io::Error::other)?;
    let event = KEvent::new(
        child.id() as usize,
        EventFilter::EVFILT_PROC,
        EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
        FilterFlag::NOTE_EXIT,
        0,
        0,
    );
    match queue.kevent(&[event], &mut [], None) {
        Ok(_) | Err(Errno::ESRCH) => {}
        Err(error) => return Err(io::Error::other(error)),
    }
    Ok(queue.into())
}

fn effective_stdio(policy: &StreamPolicy, runner: &StdProcessRunner) -> std::process::Stdio {
    if runner.discard_output && matches!(policy, StreamPolicy::Inherit) {
        std::process::Stdio::null()
    } else {
        stdio_for(policy)
    }
}

struct ChildGuard {
    child: std::process::Child,
    armed: bool,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = signal_group(&self.child, Signal::SIGKILL);
            let _ = self.child.wait();
        }
    }
}

fn signal_group(child: &std::process::Child, signal: Signal) -> nix::Result<()> {
    let pid = i32::try_from(child.id()).map_err(|_| Errno::EINVAL)?;
    kill(Pid::from_raw(-pid), signal)
}

fn group_exists(child: &std::process::Child) -> bool {
    i32::try_from(child.id()).is_ok_and(|pid| kill(Pid::from_raw(-pid), None).is_ok())
}

fn add_output(
    fd: Option<OwnedFd>,
    policy: &StreamPolicy,
    stream: ProcessStream,
    outputs: &mut Vec<Output>,
    consumers: &mut Vec<ReaderHandle>,
) -> io::Result<()> {
    let Some(fd) = fd else {
        return Ok(());
    };
    if let StreamPolicy::Consume { consumer, limit } = policy {
        consumers.push(spawn_reader(File::from(fd), Arc::clone(consumer), *limit)?);
    } else {
        outputs.push(Output::new(fd, stream, policy)?);
    }
    Ok(())
}

pub(super) fn run(
    runner: &StdProcessRunner,
    spec: &ProcessSpec,
    cancellation: &Cancellation,
) -> Result<ProcessResult> {
    run_with_hook(runner, spec, cancellation, || {})
}

pub(super) fn run_with_hook(
    runner: &StdProcessRunner,
    spec: &ProcessSpec,
    cancellation: &Cancellation,
    before_spawn: impl FnOnce(),
) -> Result<ProcessResult> {
    check_before_spawn(spec, cancellation)?;
    runner.redactor.register_sensitive_environment(&spec.env);
    let started = Instant::now();
    let (wake, _subscription) = cancellation
        .subscribe()
        .map_err(|error| Error::io(format!("subscribe to cancellation: {error}")))?;
    let mut command = Command::new(resolve_program(spec)?);
    command
        .env_clear()
        .args(&spec.args)
        .envs(&spec.env)
        .process_group(0);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    configure_input(&mut command, &spec.stdin);
    let combined_limit = combined_capture_limit(&spec.stdout, &spec.stderr)?;
    let combined_pipe = if combined_limit.is_some() {
        Some(configure_combined_stream(&mut command)?)
    } else {
        command
            .stdout(effective_stdio(&spec.stdout, runner))
            .stderr(effective_stdio(&spec.stderr, runner));
        None
    };
    before_spawn();
    let Some(spawned) = cancellation.commit_if_not_cancelled(|| command.spawn()) else {
        check_before_spawn(spec, cancellation)?;
        return Err(Error::internal("spawn gate closed without cancellation"));
    };
    let mut child = ChildGuard {
        child: spawned.map_err(|error| {
            Error::io(format!("start {}: {error}", spec.program.to_string_lossy()))
        })?,
        armed: true,
    };
    drop(command);
    let result = run_spawned(
        runner,
        spec,
        cancellation,
        started,
        &wake,
        &mut child.child,
        combined_pipe,
    );
    if result.is_ok() {
        child.armed = false;
    }
    result
}

fn run_spawned(
    runner: &StdProcessRunner,
    spec: &ProcessSpec,
    cancellation: &Cancellation,
    started: Instant,
    wake: &UnixStream,
    child: &mut std::process::Child,
    combined_pipe: Option<UnixStream>,
) -> Result<ProcessResult> {
    let io_error = |error| {
        Error::io(format!(
            "supervise {}: {error}",
            spec.program.to_string_lossy()
        ))
    };
    let event = child_event(child).map_err(io_error)?;
    let combined = combined_capture_limit(&spec.stdout, &spec.stderr)?
        .map(|limit| Arc::new(Mutex::new(CombinedCaptureBuffer::new(limit))));
    let mut outputs = Vec::with_capacity(2);
    let mut consumers = ReaderGroup {
        handles: Vec::new(),
        timeout: spec.cleanup_timeout,
    };
    if let Some(pipe) = combined_pipe {
        outputs.push(
            Output::new(pipe.into(), ProcessStream::Combined, &spec.stdout).map_err(io_error)?,
        );
    } else {
        add_output(
            child.stdout.take().map(Into::into),
            &spec.stdout,
            ProcessStream::Stdout,
            &mut outputs,
            &mut consumers.handles,
        )
        .map_err(io_error)?;
        add_output(
            child.stderr.take().map(Into::into),
            &spec.stderr,
            ProcessStream::Stderr,
            &mut outputs,
            &mut consumers.handles,
        )
        .map_err(io_error)?;
    }
    let stdin = child.stdin.take();
    if let Some(stdin) = &stdin {
        nonblocking(stdin).map_err(io_error)?;
    }
    if runner.discard_output
        && let Some(capture) = &combined
    {
        outputs[0].inline = Some(Box::new(InlineCapture {
            relay: RelayState::default(),
            redactor: runner.redactor.clone(),
            capture: Arc::clone(capture),
        }));
    }
    let callbacks = callbacks(&outputs, runner, combined.as_ref()).map_err(io_error)?;
    let mut supervisor = Supervisor {
        spec,
        cancellation,
        wake,
        child,
        event,
        outputs,
        consumers,
        stdin,
        written: 0,
        status: None,
        deadline: None,
        killed: false,
        cancelled: None,
        failure: None,
        combined,
        callbacks,
        callbacks_finished: false,
        group: None,
    };
    supervisor.drive()?;
    supervisor.finish(started)
}

struct Supervisor<'a> {
    spec: &'a ProcessSpec,
    cancellation: &'a Cancellation,
    wake: &'a UnixStream,
    child: &'a mut std::process::Child,
    event: OwnedFd,
    outputs: Vec<Output>,
    consumers: ReaderGroup,
    stdin: Option<std::process::ChildStdin>,
    written: usize,
    status: Option<std::process::ExitStatus>,
    deadline: Option<Instant>,
    killed: bool,
    cancelled: Option<i32>,
    failure: Option<Error>,
    combined: Option<Arc<Mutex<CombinedCaptureBuffer>>>,
    callbacks: Option<CallbackWorker>,
    callbacks_finished: bool,
    group: Option<GroupWatch>,
}

struct GroupWatch {
    descriptors: Vec<OwnedFd>,
    live: bool,
}

impl GroupWatch {
    fn refresh(child: &std::process::Child) -> io::Result<Self> {
        if !group_exists(child) {
            return Ok(Self {
                descriptors: Vec::new(),
                live: false,
            });
        }
        let members = group::group_members(child.id())?;
        let mut descriptors = Vec::new();
        let mut live = !members.is_empty();
        for pid in members {
            if pid != child.id()
                && let Some(fd) = group::watch(pid)?
            {
                descriptors.push(fd);
            }
        }
        if live && descriptors.is_empty() {
            live = !group::group_members(child.id())?.is_empty();
        }
        Ok(Self { descriptors, live })
    }
}

struct ReaderGroup {
    handles: Vec<ReaderHandle>,
    timeout: std::time::Duration,
}

impl Drop for ReaderGroup {
    fn drop(&mut self) {
        for consumer in &self.handles {
            consumer
                .cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = consumer.wake.shutdown(std::net::Shutdown::Write);
        }
        for consumer in self.handles.drain(..) {
            let _ = join_reader(Some(consumer), self.timeout);
        }
    }
}

impl Supervisor<'_> {
    fn input(&self) -> &[u8] {
        match &self.spec.stdin {
            InputPolicy::Bytes(bytes) => bytes,
            _ => &[],
        }
    }

    fn drive(&mut self) -> Result<()> {
        self.status = self
            .child
            .try_wait()
            .map_err(|error| Error::io(error.to_string()))?;
        loop {
            if self
                .callbacks
                .as_ref()
                .is_some_and(CallbackWorker::completed)
                && !self.callbacks_finished
            {
                self.failure
                    .get_or_insert_with(|| Error::io("process callback stopped unexpectedly"));
            }
            if self.cancelled.is_none() {
                self.cancelled = self.cancellation.signal();
            }
            if self.deadline.is_none()
                && (self.status.is_some() || self.cancelled.is_some() || self.failure.is_some())
            {
                let _ = signal_group(self.child, Signal::SIGTERM);
                self.deadline = Some(Instant::now() + self.spec.cleanup_timeout);
                self.refresh_group();
                if self.status.is_some()
                    && self.stdin.is_some()
                    && self.written < self.input().len()
                {
                    self.failure.get_or_insert_with(|| {
                        Error::io(
                            "write process input: child closed stdin before all bytes were written",
                        )
                    });
                }
                self.stdin = None;
            }
            if !self.callbacks_finished && self.outputs.iter().all(|output| output.closed) {
                if let Some(callbacks) = &self.callbacks {
                    callbacks.finish();
                }
                self.callbacks_finished = true;
            }
            if self.status.is_some()
                && self
                    .callbacks
                    .as_ref()
                    .is_none_or(CallbackWorker::completed)
                && self.outputs.iter().all(|output| output.closed)
                && self.consumers.handles.is_empty()
                && (self.group.as_ref().is_some_and(|group| !group.live) || self.killed)
            {
                return Ok(());
            }
            if self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                if self.killed {
                    self.failure.get_or_insert_with(|| {
                        Error::io("process output did not close during cleanup")
                    });
                    return Ok(());
                }
                let _ = signal_group(self.child, Signal::SIGKILL);
                self.killed = true;
                self.deadline = Some(Instant::now() + self.spec.cleanup_timeout);
            }
            if self.written == self.input().len() {
                self.stdin = None;
            }
            let ready = self.wait()?;
            self.service(ready)?;
        }
    }

    fn wait(&self) -> Result<[bool; 9]> {
        let mut descriptors =
            std::array::from_fn::<_, 8, _>(|_| PollFd::new(self.wake.as_fd(), PollFlags::POLLIN));
        let mut slots = [usize::MAX; 8];
        let mut count = 0;
        if self.cancelled.is_none() {
            slots[7] = count;
            count += 1;
        }
        if self.status.is_none() {
            descriptors[count] = PollFd::new(self.event.as_fd(), PollFlags::POLLIN);
            slots[0] = count;
            count += 1;
        }
        for (position, output) in self.outputs.iter().enumerate() {
            if !output.closed
                && (!output.needs_callback()
                    || self
                        .callbacks
                        .as_ref()
                        .is_some_and(CallbackWorker::can_send))
            {
                descriptors[count] = PollFd::new(output.file.as_fd(), PollFlags::POLLIN);
                slots[1 + position] = count;
                count += 1;
            }
        }
        for (position, consumer) in self.consumers.handles.iter().enumerate() {
            descriptors[count] = PollFd::new(consumer.finished.as_fd(), PollFlags::POLLIN);
            slots[3 + position] = count;
            count += 1;
        }
        if let Some(callbacks) = &self.callbacks
            && !callbacks.completed()
        {
            descriptors[count] = PollFd::new(callbacks.event_fd(), PollFlags::POLLIN);
            slots[5] = count;
            count += 1;
        }
        if let Some(stdin) = &self.stdin {
            descriptors[count] = PollFd::new(stdin.as_fd(), PollFlags::POLLOUT);
            slots[6] = count;
            count += 1;
        }
        let timeout = self
            .deadline
            .map_or(Ok(PollTimeout::NONE), |deadline| {
                PollTimeout::try_from(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .saturating_add(1)
                        .min(i32::MAX as u128),
                )
            })
            .map_err(|error| Error::io(error.to_string()))?;
        let mut group_ready = false;
        let result = if let Some(group) = &self.group
            && !group.descriptors.is_empty()
        {
            let mut combined = Vec::with_capacity(count + group.descriptors.len());
            combined.extend_from_slice(&descriptors[..count]);
            combined.extend(
                group
                    .descriptors
                    .iter()
                    .map(|fd| PollFd::new(fd.as_fd(), PollFlags::POLLIN)),
            );
            let result = poll(&mut combined, timeout);
            descriptors[..count].clone_from_slice(&combined[..count]);
            group_ready = combined[count..]
                .iter()
                .any(|fd| fd.revents().is_some_and(|events| !events.is_empty()));
            result
        } else {
            poll(&mut descriptors[..count], timeout)
        };
        match result {
            Err(Errno::EINTR) => return Ok([false; 9]),
            Err(error) => return Err(Error::io(format!("wait for process events: {error}"))),
            Ok(_) => {}
        }
        Ok(std::array::from_fn(|index| {
            if index == 8 {
                group_ready
            } else {
                slots[index] < count
                    && descriptors[slots[index]]
                        .revents()
                        .is_some_and(|events| !events.is_empty())
            }
        }))
    }

    fn refresh_group(&mut self) {
        self.group = Some(
            GroupWatch::refresh(self.child).unwrap_or_else(|_| GroupWatch {
                descriptors: Vec::new(),
                live: true,
            }),
        );
    }

    fn service(&mut self, ready: [bool; 9]) -> Result<()> {
        let child_exited = self.status.is_none() && ready[0];
        if self.status.is_none() && ready[0] {
            self.status = self
                .child
                .try_wait()
                .map_err(|error| Error::io(error.to_string()))?;
        }
        if self.group.is_some() && (child_exited || ready[8]) {
            self.refresh_group();
        }
        for (position, output) in self.outputs.iter_mut().enumerate() {
            if !output.closed
                && ready[1 + position]
                && (!output.needs_callback()
                    || self
                        .callbacks
                        .as_ref()
                        .is_some_and(CallbackWorker::can_send))
                && let Err(error) = output.read(self.callbacks.as_ref())
            {
                self.failure
                    .get_or_insert_with(|| Error::io(format!("read process output: {error}")));
                output.closed = true;
            }
        }
        for position in (0..self.consumers.handles.len()).rev() {
            if ready[3 + position] {
                let consumer = self.consumers.handles.remove(position);
                if let Err(error) = join_reader(Some(consumer), self.spec.cleanup_timeout) {
                    self.failure.get_or_insert(error);
                }
            }
        }
        if ready[5]
            && let Some(callbacks) = &self.callbacks
            && let Err(error) = callbacks.drain_notifications()
        {
            self.failure
                .get_or_insert_with(|| Error::io(format!("process callbacks: {error}")));
        }
        if let Some(writer) = &mut self.stdin
            && ready[6]
        {
            let input = match &self.spec.stdin {
                InputPolicy::Bytes(bytes) => bytes.as_slice(),
                _ => &[],
            };
            match writer.write(&input[self.written..]) {
                Ok(count) => self.written += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => {
                    self.failure
                        .get_or_insert_with(|| Error::io(format!("write process input: {error}")));
                    self.stdin = None;
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self, started: Instant) -> Result<ProcessResult> {
        if let Some(mut callbacks) = self.callbacks.take()
            && let Err(error) = callbacks.join(self.spec.cleanup_timeout)
        {
            self.failure.get_or_insert(error);
        }
        if let Some(signal) = self.cancelled.or_else(|| self.cancellation.signal()) {
            return Err(Error::cancelled(
                signal,
                format!(
                    "{} cancelled by signal {signal}",
                    self.spec.program.to_string_lossy()
                ),
            ));
        }
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        let status = self
            .status
            .ok_or_else(|| Error::io("child did not exit during cleanup"))?;
        let mut stdout = CapturedStream::default();
        let mut stderr = CapturedStream::default();
        for output in self.outputs.drain(..) {
            match output.stream {
                ProcessStream::Stdout => stdout = output.capture,
                ProcessStream::Stderr => stderr = output.capture,
                ProcessStream::Combined => {}
            }
        }
        let combined = self.combined.take().map(|capture| {
            std::mem::take(
                &mut capture
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .stream,
            )
        });
        Ok(ProcessResult {
            termination: child_termination(status),
            stdout,
            stderr,
            combined,
            duration: started.elapsed(),
        })
    }
}
