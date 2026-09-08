use super::CallbackWorker;
use crate::process::ProcessStream;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

#[test]
fn queue_backpressure_preserves_every_chunk_and_recycles_capacity() {
    let (entered, received) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let retained = Arc::clone(&seen);
    let mut worker = CallbackWorker::new(move |stream, bytes| {
        retained
            .lock()
            .expect("seen")
            .push((stream, bytes.map(<[u8]>::to_vec)));
        if bytes == Some(b"first".as_slice()) {
            entered.send(()).expect("entered");
            released.recv().expect("release");
        }
    })
    .expect("worker");
    assert!(worker.try_send(ProcessStream::Stdout, b"first", false));
    received
        .recv_timeout(Duration::from_secs(1))
        .expect("callback entered");
    assert!(worker.try_send(ProcessStream::Stderr, b"second", false));
    assert!(!worker.can_send());
    assert!(!worker.try_send(ProcessStream::Stdout, b"dropped", false));
    release.send(()).expect("release");
    worker.finish();
    worker.join(Duration::from_secs(1)).expect("joined");
    assert_eq!(
        *seen.lock().expect("seen"),
        vec![
            (ProcessStream::Stdout, Some(b"first".to_vec())),
            (ProcessStream::Stderr, Some(b"second".to_vec()))
        ]
    );
}

#[test]
fn blocked_callback_cannot_hold_up_bounded_cleanup() {
    let (entered, received) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let (exited, exit) = mpsc::channel();
    let mut worker = CallbackWorker::new(move |_, _| {
        entered.send(()).expect("entered");
        released.recv().expect("release");
        exited.send(()).expect("exited");
    })
    .expect("worker");
    assert!(worker.try_send(ProcessStream::Stdout, b"bytes", false));
    received
        .recv_timeout(Duration::from_secs(1))
        .expect("entered");
    let started = Instant::now();
    assert!(worker.join(Duration::from_millis(10)).is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    release.send(()).expect("release");
    exit.recv_timeout(Duration::from_secs(1)).expect("exited");
}

#[test]
fn callback_panic_reports_failure_and_signals_completion() {
    let mut worker = CallbackWorker::new(|_, _| panic!("callback failed")).expect("worker");
    assert!(worker.try_send(ProcessStream::Stdout, b"bytes", false));
    assert!(worker.join(Duration::from_secs(1)).is_err());
}

#[test]
fn completion_and_reclaimed_capacity_wake_the_supervisor() {
    use nix::poll::{PollFd, PollFlags, poll};
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&seen);
    let mut worker = CallbackWorker::new(move |stream, bytes| {
        observed
            .lock()
            .expect("observed")
            .push((stream, bytes.map(<[u8]>::to_vec)));
    })
    .expect("worker");
    for index in 0..8 {
        while !worker.can_send() {
            let mut descriptors = [PollFd::new(worker.event_fd(), PollFlags::POLLIN)];
            assert!(poll(&mut descriptors, 1000_u16).expect("poll") > 0);
            worker.drain_notifications().expect("notifications");
        }
        assert!(worker.try_send(ProcessStream::Stdout, &[index], false));
    }
    while !worker.can_send() {
        let mut descriptors = [PollFd::new(worker.event_fd(), PollFlags::POLLIN)];
        assert!(poll(&mut descriptors, 1000_u16).expect("poll") > 0);
        worker.drain_notifications().expect("notifications");
    }
    assert!(worker.try_send(ProcessStream::Stdout, &[], true));
    worker.join(Duration::from_secs(1)).expect("joined");
    assert!(worker.completed());
    worker.drain_notifications().expect("completion");
    let records = seen.lock().expect("seen");
    assert_eq!(records.len(), 9);
    for (index, (_, bytes)) in records[..8].iter().enumerate() {
        assert_eq!(
            bytes.as_deref(),
            Some([u8::try_from(index).expect("index")].as_slice())
        );
    }
    assert_eq!(records[8], (ProcessStream::Stdout, None));
}
