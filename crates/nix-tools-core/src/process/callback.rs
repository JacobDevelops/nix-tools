use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::ProcessStream;
use crate::outcome::{Error, Result};

const BUFFER_BYTES: usize = 8192;
const BUFFER_COUNT: usize = 2;

struct Chunk {
    stream: ProcessStream,
    bytes: Vec<u8>,
    eof: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CompletionStatus {
    Running,
    Finished,
    Panicked,
}

struct State {
    queue: VecDeque<Chunk>,
    free: Vec<Vec<u8>>,
    closing: bool,
    stopped: bool,
    completion: CompletionStatus,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct Completion {
    shared: Arc<Shared>,
    notification: UnixStream,
}

impl Drop for Completion {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.completion = if thread::panicking() {
            CompletionStatus::Panicked
        } else {
            CompletionStatus::Finished
        };
        self.shared.changed.notify_all();
        let _ = self.notification.shutdown(std::net::Shutdown::Write);
    }
}

fn notify(mut notification: &UnixStream) {
    loop {
        match notification.write(&[1]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            _ => return,
        }
    }
}

// Two reusable chunks bound backpressure; callbacks must return before their worker can be joined.
pub(super) struct CallbackWorker {
    shared: Arc<Shared>,
    notification: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl CallbackWorker {
    pub(super) fn new(
        mut callback: impl FnMut(ProcessStream, Option<&[u8]>) + Send + 'static,
    ) -> io::Result<Self> {
        let (notification, sender) = UnixStream::pair()?;
        notification.set_nonblocking(true)?;
        sender.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queue: VecDeque::with_capacity(BUFFER_COUNT),
                free: (0..BUFFER_COUNT)
                    .map(|_| Vec::with_capacity(BUFFER_BYTES))
                    .collect(),
                closing: false,
                stopped: false,
                completion: CompletionStatus::Running,
            }),
            changed: Condvar::new(),
        });
        let completion = Completion {
            shared: Arc::clone(&shared),
            notification: sender,
        };
        let worker = thread::Builder::new()
            .name("process-callback".to_owned())
            .spawn(move || {
                loop {
                    let mut state = completion.shared.lock();
                    while state.queue.is_empty() && !state.closing && !state.stopped {
                        state = completion
                            .shared
                            .changed
                            .wait(state)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    if state.stopped {
                        break;
                    }
                    let Some(mut chunk) = state.queue.pop_front() else {
                        break;
                    };
                    drop(state);
                    callback(chunk.stream, (!chunk.eof).then_some(chunk.bytes.as_slice()));
                    chunk.bytes.clear();
                    completion.shared.lock().free.push(chunk.bytes);
                    notify(&completion.notification);
                }
            })?;
        Ok(Self {
            shared,
            notification,
            worker: Some(worker),
        })
    }

    pub(super) fn can_send(&self) -> bool {
        let state = self.shared.lock();
        !state.free.is_empty()
            && !state.closing
            && !state.stopped
            && state.completion == CompletionStatus::Running
    }

    pub(super) fn try_send(&self, stream: ProcessStream, bytes: &[u8], eof: bool) -> bool {
        if bytes.len() > BUFFER_BYTES {
            return false;
        }
        let mut state = self.shared.lock();
        if state.closing || state.stopped || state.completion != CompletionStatus::Running {
            return false;
        }
        let Some(mut buffer) = state.free.pop() else {
            return false;
        };
        buffer.extend_from_slice(bytes);
        state.queue.push_back(Chunk {
            stream,
            bytes: buffer,
            eof,
        });
        self.shared.changed.notify_one();
        true
    }

    pub(super) fn finish(&self) {
        self.shared.lock().closing = true;
        self.shared.changed.notify_one();
    }

    pub(super) fn event_fd(&self) -> BorrowedFd<'_> {
        self.notification.as_fd()
    }

    pub(super) fn drain_notifications(&self) -> io::Result<()> {
        let mut buffer = [0; 64];
        loop {
            match (&self.notification).read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        if self.shared.lock().completion == CompletionStatus::Panicked {
            return Err(io::Error::other("process callback panicked"));
        }
        Ok(())
    }

    pub(super) fn completed(&self) -> bool {
        self.shared.lock().completion != CompletionStatus::Running
    }

    pub(super) fn join(&mut self, timeout: Duration) -> Result<()> {
        self.finish();
        let started = Instant::now();
        let mut state = self.shared.lock();
        while state.completion == CompletionStatus::Running {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                state.stopped = true;
                self.shared.changed.notify_one();
                return Err(Error::io("process callback did not stop during cleanup"));
            }
            let (next, _) = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
        drop(state);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| Error::internal("process callback panicked"))?;
        }
        Ok(())
    }
}

impl Drop for CallbackWorker {
    fn drop(&mut self) {
        self.shared.lock().stopped = true;
        self.shared.changed.notify_one();
        if self.completed()
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
#[path = "callback_test.rs"]
mod tests;
