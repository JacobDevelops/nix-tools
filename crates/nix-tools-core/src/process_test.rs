use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::redaction::Redactor;
use crate::temp_dir_test::TempDir;

use super::{
    CONSUMER_BUFFER_BYTES, Cancellation, ChildTermination, DiscardProcessOutputRelay, InputPolicy,
    LimitedReader, LineObserver, ProcessOutputRelay, ProcessRunner, ProcessSpec, ProcessStream,
    StdProcessRunner, StreamConsumer, StreamPolicy, join_reader, spawn_reader,
};

const RELAY_FRAME_BYTES: usize = 8 * 1024;

#[derive(Default)]
struct RecordingRelay(Mutex<Vec<(ProcessStream, Vec<u8>)>>);

impl RecordingRelay {
    fn rendered(&self) -> String {
        let bytes = self
            .0
            .lock()
            .expect("relay writes")
            .iter()
            .flat_map(|(_, bytes)| bytes)
            .copied()
            .collect::<Vec<_>>();
        String::from_utf8(bytes).expect("utf8")
    }
}

impl ProcessOutputRelay for RecordingRelay {
    fn write(&self, stream: ProcessStream, bytes: &[u8]) -> std::io::Result<()> {
        self.0
            .lock()
            .expect("relay writes")
            .push((stream, bytes.to_vec()));
        Ok(())
    }
}

struct FailingRelay;

impl ProcessOutputRelay for FailingRelay {
    fn write(&self, _stream: ProcessStream, _bytes: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
    }
}

#[test]
fn preserves_child_exit_status() {
    let spec = ProcessSpec::new("/bin/sh").args(["-c", "exit 42"]);
    let result = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect("run");
    assert_eq!(result.termination, ChildTermination::Exited(42));
    let error = result
        .require_success(OsStr::new("sh"))
        .expect_err("failure");
    assert_eq!(error.exit_code.get(), 42);
}

#[test]
fn captures_bounded_output_while_draining_child() {
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", "printf 1234567890"]);
    spec.stdout = StreamPolicy::Capture { limit: 4 };
    spec.stderr = StreamPolicy::Discard;
    spec.stdin = InputPolicy::Null;
    let result = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect("run");
    assert_eq!(result.stdout.bytes, b"1234");
    assert!(result.stdout.truncated);
}

#[test]
fn sends_bytes_over_stdin_without_argv_exposure() {
    let mut spec =
        ProcessSpec::new("/bin/sh").args(["-c", "IFS= read -r line; printf '%s\\n' \"$line\""]);
    spec.stdin = InputPolicy::Bytes(b"secret\n".to_vec());
    spec.stdout = StreamPolicy::Capture { limit: 1024 };
    let result = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect("run");
    assert_eq!(result.stdout.bytes, b"secret\n");
}

#[test]
fn resolving_a_bare_program_does_not_inherit_the_parent_environment() {
    let mut spec = ProcessSpec::new("env");
    spec.stdin = InputPolicy::Null;
    spec.stdout = StreamPolicy::Capture { limit: 1024 };
    spec.stderr = StreamPolicy::Discard;

    let result = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect("run env by bare name");

    assert!(result.termination.success());
    assert!(result.stdout.bytes.is_empty());
}

#[test]
fn inherited_output_is_framed_and_redacted() {
    let redactor = Redactor::default();
    redactor.register("secret-value");
    let relay = Arc::new(RecordingRelay::default());
    let runner = StdProcessRunner::with_output(Duration::from_millis(1), redactor, relay.clone());
    let mut spec = ProcessSpec::new("/bin/sh").args([
        "-c",
        "printf 'visible secret-value\\n'; printf 'token=other\\n' >&2",
    ]);
    spec.stdin = InputPolicy::Null;
    runner.run(&spec, &Cancellation::default()).expect("run");

    let rendered = relay.rendered();
    assert!(rendered.contains("visible [REDACTED]"));
    assert!(rendered.contains("token=[REDACTED]"));
    assert!(!rendered.contains("secret-value"));
    assert!(!rendered.contains("other"));
}

#[test]
fn terminal_controls_are_normalized_before_secret_redaction() {
    let redactor = Redactor::default();
    redactor.register("secret-value");
    redactor.register("format\u{200d}-secret");
    let relay = Arc::new(RecordingRelay::default());
    let runner = StdProcessRunner::with_output(Duration::from_millis(1), redactor, relay.clone());
    let mut spec = ProcessSpec::new("/bin/sh").args([
        "-c",
        "printf 'sec\\033[31mret-value\\033[0m\\n'; printf 'sec\\302\\23331mret-value\\302\\2330m\\n'; printf 'sec\\342\\200\\256ret-value\\n'; printf 'format\\342\\200\\215-secret\\n'",
    ]);
    spec.stdin = InputPolicy::Null;
    spec.stderr = StreamPolicy::Discard;

    runner.run(&spec, &Cancellation::default()).expect("run");

    let rendered = relay.rendered();
    assert_eq!(rendered, "[REDACTED]\n[REDACTED]\n[REDACTED]\n[REDACTED]\n");
    assert!(!rendered.contains("secret-value"));
    assert!(!rendered.contains('\u{1b}'));
    assert!(!rendered.contains('\u{9b}'));
}

#[test]
fn sensitive_environment_values_are_registered_before_child_output() {
    let redactor = Redactor::default();
    let relay = Arc::new(RecordingRelay::default());
    let runner = StdProcessRunner::with_output(Duration::from_millis(1), redactor, relay.clone());
    let mut spec = ProcessSpec::new("/bin/sh")
        .args(["-c", "printf '%s\\n' \"$PULUMI_CONFIG_PASSPHRASE\""])
        .env("PULUMI_CONFIG_PASSPHRASE", "hunter2");
    spec.stdin = InputPolicy::Null;
    spec.stderr = StreamPolicy::Discard;

    runner.run(&spec, &Cancellation::default()).expect("run");

    assert_eq!(relay.rendered(), "[REDACTED]\n");
}

#[test]
fn distant_assignment_separator_suppresses_the_value_continuation() {
    let relay = Arc::new(RecordingRelay::default());
    let runner =
        StdProcessRunner::with_output(Duration::from_millis(1), Redactor::default(), relay.clone());
    let script = format!("printf 'TOKEN{}=hunter2\\n'", " ".repeat(9_000));
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", &script]);
    spec.stdin = InputPolicy::Null;
    spec.stderr = StreamPolicy::Discard;

    runner.run(&spec, &Cancellation::default()).expect("run");

    let rendered = relay.rendered();
    assert!(!rendered.contains("hunter2"));
    assert!(rendered.ends_with('\n'));
}

#[test]
fn inherited_output_frames_are_bounded_without_newlines() {
    let relay = Arc::new(RecordingRelay::default());
    let runner =
        StdProcessRunner::with_output(Duration::from_millis(1), Redactor::default(), relay.clone());
    let mut spec = ProcessSpec::new("/bin/sh").args([
        "-c",
        "i=0; while [ \"$i\" -lt 20000 ]; do printf x; i=$((i + 1)); done",
    ]);
    spec.stdin = InputPolicy::Null;
    spec.stderr = StreamPolicy::Discard;
    runner.run(&spec, &Cancellation::default()).expect("run");

    let writes = relay.0.lock().expect("relay writes");
    assert_eq!(
        writes.iter().map(|(_, bytes)| bytes.len()).sum::<usize>(),
        20_000
    );
    assert!(
        writes
            .iter()
            .all(|(_, bytes)| bytes.len() <= RELAY_FRAME_BYTES)
    );
}

#[test]
fn registered_secret_crossing_frame_boundary_is_redacted() {
    let redactor = Redactor::default();
    redactor.register("secret-value");
    let relay = Arc::new(RecordingRelay::default());
    let runner = StdProcessRunner::with_output(Duration::from_millis(1), redactor, relay.clone());
    let script = format!(
        "i=0; while [ \"$i\" -lt {} ]; do printf x; i=$((i + 1)); done; printf secret-value",
        RELAY_FRAME_BYTES - 4
    );
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", &script]);
    spec.stdin = InputPolicy::Null;
    spec.stderr = StreamPolicy::Discard;
    runner.run(&spec, &Cancellation::default()).expect("run");

    let rendered = relay.rendered();
    assert!(!rendered.contains("secret-value"));
    assert!(rendered.ends_with("[REDACTED]"));
}

#[test]
fn multiline_secret_crossing_frame_and_newline_is_redacted() {
    let redactor = Redactor::default();
    redactor.register("alpha\nbeta");
    let relay = Arc::new(RecordingRelay::default());
    let runner = StdProcessRunner::with_output(Duration::from_millis(1), redactor, relay.clone());
    let script = format!(
        "i=0; while [ \"$i\" -lt {} ]; do printf x; i=$((i + 1)); done; printf 'alpha\\nbeta'",
        RELAY_FRAME_BYTES - 4
    );
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", &script]);
    spec.stdin = InputPolicy::Null;
    spec.stderr = StreamPolicy::Discard;
    runner.run(&spec, &Cancellation::default()).expect("run");

    let rendered = relay.rendered();
    assert!(!rendered.contains("alpha"));
    assert!(!rendered.contains("beta"));
    assert!(rendered.ends_with("[REDACTED]"));
}

#[test]
fn split_sensitive_assignment_and_quoted_value_are_fully_redacted() {
    let relay = Arc::new(RecordingRelay::default());
    let runner =
        StdProcessRunner::with_output(Duration::from_millis(1), Redactor::default(), relay.clone());
    let script = format!(
        "i=0; while [ \"$i\" -lt {} ]; do printf x; i=$((i + 1)); done; printf \"TOKEN='correct horse battery' trailing\\nvisible\\n\"",
        RELAY_FRAME_BYTES - 4
    );
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", &script]);
    spec.stdin = InputPolicy::Null;
    spec.stderr = StreamPolicy::Discard;
    runner.run(&spec, &Cancellation::default()).expect("run");

    let rendered = relay.rendered();
    assert!(!rendered.contains("correct"));
    assert!(!rendered.contains("horse"));
    assert!(!rendered.contains("battery"));
    assert!(rendered.contains("TOKEN='[REDACTED]'"));
    assert!(rendered.ends_with("visible\n"));
}

#[test]
fn combined_capture_relays_in_order_and_preserves_bounded_head_and_tail() {
    let relay = Arc::new(RecordingRelay::default());
    let runner =
        StdProcessRunner::with_output(Duration::from_millis(1), Redactor::default(), relay.clone());
    let mut spec = ProcessSpec::new("/bin/sh").args([
        "-c",
        "printf 'out-1\\n'; printf 'err-1\\n' >&2; printf 'middle-padding\\n'; printf 'Resources: 2 created\\n' >&2",
    ]);
    spec.stdin = InputPolicy::Null;
    spec.stdout = StreamPolicy::RelayAndCapture { limit: 24 };
    spec.stderr = StreamPolicy::RelayAndCapture { limit: 24 };
    let result = runner.run(&spec, &Cancellation::default()).expect("run");

    let relayed = relay.rendered();
    let combined = result.combined.expect("combined capture");
    assert!(combined.truncated());
    assert!(combined.head.len() + combined.tail.len() <= 24);
    assert!(combined.omitted_bytes > 0);
    assert_eq!(
        relayed,
        "out-1\nerr-1\nmiddle-padding\nResources: 2 created\n"
    );
    assert!(
        String::from_utf8(combined.head)
            .expect("head")
            .starts_with("out-1\n")
    );
    assert!(
        String::from_utf8(combined.tail)
            .expect("tail")
            .ends_with("2 created\n")
    );
}

#[test]
fn suppressed_output_still_drains_and_preserves_combined_capture() {
    let runner = StdProcessRunner::without_output(Duration::from_millis(1), Redactor::default());
    let mut spec = ProcessSpec::new("/bin/sh").args([
        "-c",
        "printf 'structured-output-only\\n'; printf 'captured-error\\n' >&2",
    ]);
    spec.stdin = InputPolicy::Null;
    spec.stdout = StreamPolicy::RelayAndCapture { limit: 1024 };
    spec.stderr = StreamPolicy::RelayAndCapture { limit: 1024 };

    let result = runner.run(&spec, &Cancellation::default()).expect("run");

    assert_eq!(
        result.combined.expect("combined capture").into_bytes(),
        b"structured-output-only\ncaptured-error\n"
    );
}

#[test]
fn combined_capture_contains_only_redacted_output_across_frames() {
    let redactor = Redactor::default();
    redactor.register("secret-value");
    let relay = Arc::new(RecordingRelay::default());
    let runner = StdProcessRunner::with_output(Duration::from_millis(1), redactor, relay.clone());
    let script = format!(
        "i=0; while [ \"$i\" -lt {} ]; do printf x; i=$((i + 1)); done; printf secret-value; printf '\\nsummary\\n' >&2",
        RELAY_FRAME_BYTES - 4
    );
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", &script]);
    spec.stdin = InputPolicy::Null;
    spec.stdout = StreamPolicy::RelayAndCapture { limit: 20_000 };
    spec.stderr = StreamPolicy::RelayAndCapture { limit: 20_000 };
    let result = runner.run(&spec, &Cancellation::default()).expect("run");

    let combined =
        String::from_utf8(result.combined.expect("combined capture").into_bytes()).expect("utf8");
    let relayed = relay.rendered();
    assert!(!combined.contains("secret-value"));
    assert!(!relayed.contains("secret-value"));
    assert!(combined.contains("[REDACTED]"));
    assert_eq!(combined, relayed);
}

#[test]
fn prior_cancellation_prevents_spawn() {
    let cancellation = Cancellation::default();
    cancellation.request(2);
    let root = TempDir::new("process-cancelled");
    let marker = root.path().join("marker");
    let script = format!(": > {}", marker.display());
    let spec = ProcessSpec::new("/bin/sh").args(["-c", &script]);
    let error = StdProcessRunner::new(Duration::from_millis(1), Redactor::default())
        .run(&spec, &cancellation)
        .expect_err("cancelled");
    assert_eq!(error.exit_code.get(), 130);
    assert!(!marker.exists());
}

#[test]
fn cancellation_winning_the_spawn_gate_prevents_child_side_effects() {
    let cancellation = Cancellation::default();
    let root = TempDir::new("process-spawn-gate");
    let marker = root.path().join("marker");
    let script = format!(": > {}", marker.display());
    let spec = ProcessSpec::new("/bin/sh").args(["-c", &script]);
    let relay: Arc<dyn ProcessOutputRelay> = Arc::new(DiscardProcessOutputRelay);

    let result = super::event::run_with_hook(
        &StdProcessRunner::with_output(Duration::from_secs(1), Redactor::default(), relay),
        &spec,
        &cancellation,
        || {
            cancellation.request(2);
        },
    );

    let Err(error) = result else {
        panic!("cancellation must prevent spawn");
    };
    assert_eq!(error.kind, crate::outcome::ErrorKind::Cancelled);
    assert!(!marker.exists());
}

#[test]
fn cancellation_kills_stubborn_process_group() {
    let cancellation = Cancellation::default();
    let requester = cancellation.clone();
    let signal = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        requester.request(2);
    });
    let mut spec = ProcessSpec::new("/bin/sh").args([
        "-c",
        "trap '' TERM; (trap '' TERM; while :; do :; done) & wait",
    ]);
    spec.cleanup_timeout = Duration::from_millis(50);
    let started = std::time::Instant::now();
    let error = StdProcessRunner::new(Duration::from_millis(1), Redactor::default())
        .run(&spec, &cancellation)
        .expect_err("cancelled");
    signal.join().expect("signal thread");
    assert_eq!(error.exit_code.get(), 130);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn cancellation_wins_over_stdin_pipe_errors() {
    let cancellation = Cancellation::default();
    let requester = cancellation.clone();
    let signal = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        requester.request(15);
    });
    let mut spec =
        ProcessSpec::new("/bin/sh").args(["-c", "exec 0<&-; trap '' TERM; while :; do :; done"]);
    spec.stdin = InputPolicy::Bytes(vec![b'x'; 1024 * 1024]);
    spec.stdout = StreamPolicy::RelayAndCapture { limit: 1024 };
    spec.stderr = StreamPolicy::RelayAndCapture { limit: 1024 };
    spec.cleanup_timeout = Duration::from_millis(50);
    let error = StdProcessRunner::new(Duration::from_millis(1), Redactor::default())
        .run(&spec, &cancellation)
        .expect_err("cancelled");
    signal.join().expect("signal thread");
    assert_eq!(error.exit_code.get(), 143);
}

#[cfg(target_os = "linux")]
#[test]
fn escaped_writer_cannot_block_reader_cleanup() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let setsid = std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("setsid"))
                .find(|candidate| candidate.is_file())
        })
        .expect("requires setsid on PATH");
    let root = TempDir::new("process-escaped-writer");
    let marker = root.path().join("marker");
    let script = format!(
        "{} -f /bin/sh -c \"exec 0<&-; echo \\$\\$ > {}; trap '' TERM; while :; do :; done\" & while [ ! -s {} ]; do :; done",
        setsid.display(),
        marker.display(),
        marker.display()
    );
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", &script]);
    spec.stdin = InputPolicy::Null;
    spec.stderr = StreamPolicy::Discard;
    spec.cleanup_timeout = Duration::from_millis(50);
    let started = std::time::Instant::now();
    let result = StdProcessRunner::new(Duration::from_millis(1), Redactor::default())
        .run(&spec, &Cancellation::default());
    let error = result.expect_err("wedged reader must not be reported as success");
    assert_eq!(error.exit_code.get(), 74);
    assert!(error.message.contains("did not close during cleanup"));
    assert!(started.elapsed() < Duration::from_secs(1));

    let pid = std::fs::read_to_string(&marker)
        .expect("escaped descendant pid")
        .trim()
        .parse::<i32>()
        .expect("numeric escaped descendant pid");
    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
}

#[cfg(unix)]
#[test]
fn timed_out_reader_releases_its_pipe_before_returning() {
    use std::os::unix::net::UnixStream;

    struct Drain;
    impl StreamConsumer for Drain {
        fn consume(&self, reader: &mut dyn std::io::Read) -> std::io::Result<()> {
            std::io::copy(reader, &mut std::io::sink()).map(|_| ())
        }
    }
    let (reader, writer) = UnixStream::pair().expect("pipe");
    let handle = spawn_reader(reader, Arc::new(Drain), 64).expect("spawn reader");
    let worker_reference = Arc::downgrade(&handle.cancelled);

    let error = join_reader(Some(handle), Duration::from_millis(1)).expect_err("held pipe");

    assert!(error.message.contains("did not close during cleanup"));
    assert!(
        worker_reference.upgrade().is_none(),
        "reader worker and descriptor must be gone before cleanup returns"
    );
    drop(writer);
}

#[test]
fn a_child_shorter_than_the_poll_interval_is_reaped_without_waiting_for_it() {
    let sleep = std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("sleep"))
                .find(|candidate| candidate.is_file())
        })
        .expect("requires sleep on PATH");
    let spec = ProcessSpec::new(sleep).args(["0.05"]);
    let result = StdProcessRunner::new(Duration::from_secs(2), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect("run");
    assert_eq!(result.termination, ChildTermination::Exited(0));
    assert!(
        result.duration < Duration::from_millis(500),
        "reaping cost {:?} of a 2s poll interval",
        result.duration
    );
}

#[test]
fn a_failing_relay_leaves_the_child_outcome_intact() {
    let runner = StdProcessRunner::with_output(
        Duration::from_millis(1),
        Redactor::default(),
        Arc::new(FailingRelay),
    );
    let mut spec =
        ProcessSpec::new("/bin/sh").args(["-c", "printf 'out\\n'; printf 'err\\n' >&2; exit 7"]);
    spec.stdin = InputPolicy::Null;

    let result = runner.run(&spec, &Cancellation::default()).expect("run");

    assert_eq!(result.termination, ChildTermination::Exited(7));
}

#[test]
fn a_failing_relay_still_captures_combined_output() {
    let runner = StdProcessRunner::with_output(
        Duration::from_millis(1),
        Redactor::default(),
        Arc::new(FailingRelay),
    );
    let mut spec =
        ProcessSpec::new("/bin/sh").args(["-c", "printf 'diff\\n'; printf 'warning\\n' >&2"]);
    spec.stdin = InputPolicy::Null;
    spec.stdout = StreamPolicy::RelayAndCapture { limit: 1024 };
    spec.stderr = StreamPolicy::RelayAndCapture { limit: 1024 };

    let result = runner.run(&spec, &Cancellation::default()).expect("run");

    assert_eq!(result.termination, ChildTermination::Exited(0));
    let combined =
        String::from_utf8(result.combined.expect("combined capture").into_bytes()).expect("utf8");
    let mut lines = combined.lines().collect::<Vec<_>>();
    lines.sort_unstable();
    assert_eq!(lines, ["diff", "warning"]);
    assert_eq!(combined.len(), "diff\nwarning\n".len());
}

#[derive(Default)]
struct RecordingObserver {
    lines: Mutex<Vec<String>>,
}

impl LineObserver for RecordingObserver {
    fn line(&self, line: &[u8]) {
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(String::from_utf8_lossy(line).into_owned());
    }
}

impl RecordingObserver {
    fn lines(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[test]
fn observed_output_reaches_the_observer_line_by_line_and_still_captures() {
    let observer = Arc::new(RecordingObserver::default());
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", "printf 'one\\ntwo\\nthree'"]);
    spec.stdout = StreamPolicy::Observe {
        limit: 1024,
        observer: Arc::clone(&observer) as Arc<dyn LineObserver>,
    };
    spec.stderr = StreamPolicy::Discard;
    spec.stdin = InputPolicy::Null;
    let result = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect("run");
    assert_eq!(observer.lines(), vec!["one\n", "two\n", "three"]);
    assert_eq!(result.stdout.bytes, b"one\ntwo\nthree");
    assert!(!result.stdout.truncated);
}

#[test]
fn an_observed_capture_keeps_the_bound_and_truncation_flag_of_a_plain_capture() {
    let observer = Arc::new(RecordingObserver::default());
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", "printf '1234567890\\n'"]);
    spec.stdout = StreamPolicy::Observe {
        limit: 4,
        observer: Arc::clone(&observer) as Arc<dyn LineObserver>,
    };
    spec.stderr = StreamPolicy::Discard;
    spec.stdin = InputPolicy::Null;
    let result = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect("run");
    assert_eq!(result.stdout.bytes, b"1234");
    assert!(result.stdout.truncated);
    // The bound is on what the caller keeps, not on what the observer is allowed to see.
    assert_eq!(observer.lines(), vec!["1234567890\n"]);
}

/// Reads at most `prefix` bytes and leaves the rest of the stream for the runner to drain.
struct PrefixConsumer {
    prefix: usize,
    seen: Mutex<Vec<u8>>,
}

impl PrefixConsumer {
    fn new(prefix: usize) -> Self {
        Self {
            prefix,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<u8> {
        self.seen.lock().expect("consumed bytes").clone()
    }
}

impl StreamConsumer for PrefixConsumer {
    fn consume(&self, reader: &mut dyn std::io::Read) -> std::io::Result<()> {
        let mut buffer = vec![0_u8; self.prefix];
        let mut filled = 0;
        while filled < buffer.len() {
            let read = reader.read(&mut buffer[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        buffer.truncate(filled);
        *self.seen.lock().expect("consumed bytes") = buffer;
        Ok(())
    }
}

#[test]
fn a_consumed_stream_reaches_the_consumer_and_retains_nothing() {
    let consumer = Arc::new(PrefixConsumer::new(1024));
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", "printf 'graph'; printf 'noise' >&2"]);
    spec.stdout = StreamPolicy::Consume {
        consumer: Arc::clone(&consumer) as Arc<dyn StreamConsumer>,
        limit: 1024 * 1024,
    };
    spec.stderr = StreamPolicy::Capture { limit: 1024 };
    spec.stdin = InputPolicy::Null;

    let result = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect("run");

    assert_eq!(consumer.seen(), b"graph");
    assert!(result.stdout.bytes.is_empty());
    assert!(!result.stdout.truncated);
    // Diagnostics still need the other stream, which is drained at the same time.
    assert_eq!(result.stderr.bytes, b"noise");
}

#[test]
fn a_consumer_that_stops_early_does_not_block_a_child_that_keeps_writing() {
    let consumer = Arc::new(PrefixConsumer::new(16));
    // The child must outwrite the reader's own buffer plus the pipe, otherwise the buffered reader
    // swallows the whole stream and the drain this test exists to prove is never needed.
    let blocks = (CONSUMER_BUFFER_BYTES / 16) * 4;
    let mut spec = ProcessSpec::new("/bin/sh").args([
        "-c",
        &format!("i=0; while [ $i -lt {blocks} ]; do printf '0123456789abcdef'; i=$((i+1)); done"),
    ]);
    spec.stdout = StreamPolicy::Consume {
        consumer: Arc::clone(&consumer) as Arc<dyn StreamConsumer>,
        limit: 16 * 1024 * 1024,
    };
    spec.stderr = StreamPolicy::Discard;
    spec.stdin = InputPolicy::Null;

    let result = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect("run");

    assert!(result.termination.success());
    assert_eq!(consumer.seen(), b"0123456789abcdef");
}

#[test]
fn a_consumer_that_stops_early_still_enforces_the_stream_ceiling() {
    let consumer = Arc::new(PrefixConsumer::new(2));
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", "printf '0123456789'"]);
    spec.stdout = StreamPolicy::Consume {
        consumer: Arc::clone(&consumer) as Arc<dyn StreamConsumer>,
        limit: 4,
    };
    spec.stderr = StreamPolicy::Discard;
    spec.stdin = InputPolicy::Null;

    let error = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect_err("stream limit while draining");

    assert_eq!(consumer.seen(), b"01");
    assert!(
        error
            .message
            .contains("exceeded the configured stream limit")
    );
}

#[test]
fn a_limited_reader_does_not_probe_for_an_empty_read() {
    use std::io::Read;

    let mut reader = LimitedReader {
        reader: std::io::Cursor::new(b"x"),
        remaining: 0,
    };

    assert_eq!(reader.read(&mut []).expect("empty read"), 0);
    assert_eq!(reader.reader.position(), 0);
    assert!(reader.read(&mut [0]).is_err());
}

#[test]
fn a_consumed_stream_stops_at_its_ceiling_with_a_read_failure() {
    let consumer = Arc::new(PrefixConsumer::new(4096));
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", "printf '0123456789'"]);
    spec.stdout = StreamPolicy::Consume {
        consumer: Arc::clone(&consumer) as Arc<dyn StreamConsumer>,
        limit: 4,
    };
    spec.stderr = StreamPolicy::Discard;
    spec.stdin = InputPolicy::Null;

    let error = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect_err("stream limit");

    // A limit breach must not look like a short read, or a truncated document and an oversized one
    // become the same diagnostic.
    assert!(
        error
            .message
            .contains("exceeded the configured stream limit")
    );
}

#[test]
fn a_consumed_stream_of_exactly_the_ceiling_is_within_it() {
    let consumer = Arc::new(PrefixConsumer::new(4096));
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", "printf '0123456789'"]);
    spec.stdout = StreamPolicy::Consume {
        consumer: Arc::clone(&consumer) as Arc<dyn StreamConsumer>,
        limit: 10,
    };
    spec.stderr = StreamPolicy::Discard;
    spec.stdin = InputPolicy::Null;

    let result = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect("run");

    assert!(result.termination.success());
    assert_eq!(consumer.seen(), b"0123456789");
}

#[test]
fn a_zero_ceiling_rejects_rather_than_admitting_everything() {
    let consumer = Arc::new(PrefixConsumer::new(4096));
    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", "printf 'graph'"]);
    spec.stdout = StreamPolicy::Consume {
        consumer: Arc::clone(&consumer) as Arc<dyn StreamConsumer>,
        limit: 0,
    };
    spec.stderr = StreamPolicy::Discard;
    spec.stdin = InputPolicy::Null;

    let error = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect_err("zero limit");

    assert!(error.message.contains("must be greater than zero"));
    assert!(consumer.seen().is_empty());
}

#[test]
fn a_failing_consumer_reports_the_read_failure() {
    struct RefusingConsumer;

    impl StreamConsumer for RefusingConsumer {
        fn consume(&self, _reader: &mut dyn std::io::Read) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "consumer read failure",
            ))
        }
    }

    let mut spec = ProcessSpec::new("/bin/sh").args(["-c", "printf 'graph'"]);
    spec.stdout = StreamPolicy::Consume {
        consumer: Arc::new(RefusingConsumer) as Arc<dyn StreamConsumer>,
        limit: 1,
    };
    spec.stderr = StreamPolicy::Discard;
    spec.stdin = InputPolicy::Null;

    let error = StdProcessRunner::new(Duration::from_millis(10), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect_err("consumer failure");

    assert!(error.message.contains("read process output"));
    assert!(error.message.contains("consumer read failure"));
}

#[test]
fn cancellation_wakes_an_idle_runner_without_waiting_for_its_legacy_interval() {
    let cancellation = Cancellation::default();
    let requester = cancellation.clone();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(180));
        requester.request(2);
    });
    let mut spec = shell_with_test_tools("sleep 10");
    spec.cleanup_timeout = Duration::from_millis(20);
    let started = std::time::Instant::now();
    let error = StdProcessRunner::without_output(Duration::from_secs(2), Redactor::default())
        .run(&spec, &cancellation)
        .expect_err("cancelled");
    trigger.join().expect("requester");
    assert_eq!(error.exit_code.get(), 130);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "cancellation took {:?}",
        started.elapsed()
    );
}

#[test]
fn consumer_failure_terminates_a_child_that_keeps_running() {
    struct Reject;
    impl StreamConsumer for Reject {
        fn consume(&self, _: &mut dyn std::io::Read) -> std::io::Result<()> {
            Err(std::io::Error::other("parser rejected output"))
        }
    }
    let mut spec = shell_with_test_tools("sleep 5");
    spec.stdout = StreamPolicy::Consume {
        consumer: Arc::new(Reject),
        limit: 100,
    };
    spec.cleanup_timeout = Duration::from_millis(20);
    let started = std::time::Instant::now();
    let error = StdProcessRunner::without_output(Duration::from_secs(2), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect_err("consumer failed");
    assert!(
        error.message.contains("parser rejected output"),
        "{}",
        error.message
    );
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn one_cancellation_wakes_every_concurrent_runner() {
    let cancellation = Cancellation::default();
    let workers = (0..4)
        .map(|_| {
            let token = cancellation.clone();
            std::thread::spawn(move || {
                let mut spec = shell_with_test_tools("sleep 5");
                spec.cleanup_timeout = Duration::from_millis(20);
                StdProcessRunner::without_output(Duration::from_secs(2), Redactor::default())
                    .run(&spec, &token)
            })
        })
        .collect::<Vec<_>>();
    std::thread::sleep(Duration::from_millis(40));
    let started = std::time::Instant::now();
    cancellation.request(2);
    for worker in workers {
        assert_eq!(
            worker
                .join()
                .expect("runner")
                .expect_err("cancelled")
                .exit_code
                .get(),
            130
        );
    }
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn child_exit_before_event_registration_is_still_successful() {
    use std::os::fd::AsFd;
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "read line; exit 0"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("child");
    let initial = super::event::child_event(&child).expect("register running child");
    std::io::Write::write_all(&mut child.stdin.take().expect("input pipe"), b"exit\n")
        .expect("release child");
    let mut descriptors = [nix::poll::PollFd::new(
        initial.as_fd(),
        nix::poll::PollFlags::POLLIN,
    )];
    assert_eq!(
        nix::poll::poll(&mut descriptors, 1000_u16).expect("exit event"),
        1
    );
    let _event = super::event::child_event(&child).expect("register exited child");
    assert!(child.try_wait().expect("reap").expect("exited").success());
}

#[test]
fn consumer_panic_terminates_a_child_that_keeps_running() {
    struct Panic;
    impl StreamConsumer for Panic {
        fn consume(&self, _: &mut dyn std::io::Read) -> std::io::Result<()> {
            panic!("consumer panic fixture");
        }
    }
    let mut spec = shell_with_test_tools("sleep 5");
    spec.stdout = StreamPolicy::Consume {
        consumer: Arc::new(Panic),
        limit: 100,
    };
    spec.cleanup_timeout = Duration::from_millis(20);
    let started = std::time::Instant::now();
    let error = StdProcessRunner::without_output(Duration::from_secs(2), Redactor::default())
        .run(&spec, &Cancellation::default())
        .expect_err("consumer panicked");
    assert!(error.message.contains("panicked"));
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn cancellation_stays_responsive_while_an_output_callback_is_blocked() {
    struct BlockingRelay {
        entered: std::sync::mpsc::SyncSender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        exited: std::sync::mpsc::SyncSender<()>,
    }
    impl ProcessOutputRelay for BlockingRelay {
        fn write(&self, _stream: ProcessStream, _bytes: &[u8]) -> std::io::Result<()> {
            self.entered.send(()).expect("entered");
            self.release
                .lock()
                .expect("release lock")
                .recv()
                .expect("release");
            self.exited.send(()).expect("exited");
            Ok(())
        }
    }
    let (entered, received) = std::sync::mpsc::sync_channel(1);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let (exited, exit) = std::sync::mpsc::sync_channel(1);
    let relay = Arc::new(BlockingRelay {
        entered,
        release: Mutex::new(released),
        exited,
    });
    let cancellation = Cancellation::default();
    let child_cancellation = cancellation.clone();
    let (done, finished) = std::sync::mpsc::sync_channel(1);
    let runner = std::thread::spawn(move || {
        let mut spec = shell_with_test_tools("printf 'blocked callback\\n'; sleep 30");
        spec.cleanup_timeout = Duration::from_millis(20);
        let result =
            StdProcessRunner::with_output(Duration::from_secs(1), Redactor::default(), relay)
                .run(&spec, &child_cancellation);
        done.send(result).expect("runner result");
    });
    received
        .recv_timeout(Duration::from_secs(2))
        .expect("callback entered");
    cancellation.request(2);
    let result = finished.recv_timeout(Duration::from_secs(2));
    release.send(()).expect("release callback");
    exit.recv_timeout(Duration::from_secs(2))
        .expect("callback exited");
    runner.join().expect("runner joined");
    let error = result
        .expect("bounded runner cleanup")
        .expect_err("cancelled");
    assert_eq!(error.kind, crate::outcome::ErrorKind::Cancelled);
}

#[cfg(target_os = "linux")]
#[test]
fn unavailable_pidfd_uses_an_exit_event_without_reaping_the_child() {
    use std::os::fd::AsFd;
    for errno in [
        nix::errno::Errno::ENOSYS,
        nix::errno::Errno::EPERM,
        nix::errno::Errno::EINVAL,
    ] {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 0.01; exit 7"])
            .spawn()
            .expect("child");
        let event = super::event::child_event_with(&child, |_| {
            Err(std::io::Error::from_raw_os_error(errno as i32))
        })
        .expect("fallback event");
        let mut descriptors = [nix::poll::PollFd::new(
            event.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        assert_eq!(
            nix::poll::poll(&mut descriptors, 1000_u16).expect("event wait"),
            1
        );
        assert_eq!(
            child.wait().expect("child remains reapable").code(),
            Some(7)
        );
    }
}

#[test]
fn cancellation_does_not_wait_for_default_grace_after_the_group_exits() {
    let cancellation = Cancellation::default();
    let requester = cancellation.clone();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        requester.request(2);
    });
    let started = std::time::Instant::now();
    let spec = shell_with_test_tools("sleep 5");
    let error = StdProcessRunner::without_output(Duration::from_millis(10), Redactor::default())
        .run(&spec, &cancellation)
        .expect_err("cancelled");
    trigger.join().expect("requester");
    assert_eq!(error.exit_code.get(), 130);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "cancellation took {:?}",
        started.elapsed()
    );
}

#[test]
fn cancellation_preserves_descendant_grace_and_tracks_cleanup_children() {
    struct CancelOnReady(Cancellation);
    impl LineObserver for CancelOnReady {
        fn line(&self, line: &[u8]) {
            if line == b"ready\n" {
                self.0.request(2);
            }
        }
    }
    let root = TempDir::new("descendant-grace");
    let script = root.path().join("child.sh");
    let marker = root.path().join("cleaned");
    std::fs::write(&script, "trap 'sleep 0.08; printf complete > \"$1\"; exit 0' TERM\nprintf 'ready\\n'\nwhile :; do :; done\n").expect("child script");
    let cancellation = Cancellation::default();
    let mut spec = shell_with_test_tools("/bin/sh \"$1\" \"$2\" & wait")
        .arg("parent")
        .arg(script)
        .arg(&marker);
    spec.stdout = StreamPolicy::Observe {
        limit: 0,
        observer: Arc::new(CancelOnReady(cancellation.clone())),
    };
    let started = std::time::Instant::now();
    let error = StdProcessRunner::without_output(Duration::from_millis(10), Redactor::default())
        .run(&spec, &cancellation)
        .expect_err("cancelled");
    assert_eq!(error.exit_code.get(), 130);
    assert_eq!(
        std::fs::read_to_string(marker).expect("graceful cleanup finished"),
        "complete"
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "cleanup took {:?}",
        started.elapsed()
    );
}

fn shell_with_test_tools(script: &str) -> ProcessSpec {
    ProcessSpec::new("/bin/sh")
        .args(["-c", script])
        .env("PATH", std::env::var_os("PATH").expect("test tool PATH"))
}
