//! Incremental reader for the `nix build --log-format internal-json` activity stream.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard, PoisonError};

use nix_tools_core::process::LineObserver;
use serde::Deserialize;
use serde_json::Value;

use crate::{DependencyGraph, ProgressEvent};

const PREFIX: &[u8] = b"@nix ";
const ACTIVITY_COPY_PATH: u64 = 100;
const ACTIVITY_BUILD: u64 = 105;
const ACTIVITY_SUBSTITUTE: u64 = 108;
const RESULT_BUILD_LOG_LINE: u64 = 101;
const RESULT_PROGRESS: u64 = 105;
const RESULT_POST_BUILD_LOG_LINE: u64 = 107;

/// One decoded activity line. Fields absent from a given action stay at their defaults because the
/// stream is a diagnostic channel rather than a stable contract.
#[derive(Debug, Default, Deserialize, Eq, PartialEq)]
struct LogLine {
    #[serde(default)]
    action: String,
    #[serde(default)]
    id: u64,
    #[serde(default, rename = "type")]
    kind: u64,
    #[serde(default)]
    fields: Vec<Value>,
    #[serde(default)]
    msg: String,
}

fn parse_line(line: &[u8]) -> Option<LogLine> {
    serde_json::from_slice::<LogLine>(strip_prefix(line)?).ok()
}

fn strip_prefix(line: &[u8]) -> Option<&[u8]> {
    let trimmed = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map(|start| &line[start..])?;
    trimmed.strip_prefix(PREFIX)
}

fn field_str(fields: &[Value], index: usize) -> Option<&str> {
    fields.get(index).and_then(Value::as_str)
}

fn field_u64(fields: &[Value], index: usize) -> Option<u64> {
    fields.get(index).and_then(Value::as_u64)
}

/// Turns the activity stream of one realization process into per-derivation progress.
///
/// Every reported activity is attributed to a derivation the caller asked for, or dropped. The
/// observer also rebuilds a human-readable log from the message and build-log records, because
/// selecting the JSON log format removes the plain text a diagnostic would otherwise carry.
pub(crate) struct RealizationObserver {
    state: Mutex<ObserverState>,
}

struct ObserverState {
    events: Option<Sender<ProgressEvent>>,
    derivations: BTreeSet<String>,
    outputs: BTreeMap<String, String>,
    activities: BTreeMap<u64, String>,
    started: BTreeSet<String>,
    log: BoundedLog,
}

/// Reconstructed log bounded at both ends, because nix reports the error that ended a build in its
/// last lines and a head-only bound drops exactly that.
struct BoundedLog {
    limit: usize,
    head: Vec<u8>,
    tail: Vec<u8>,
    omitted: usize,
}

impl RealizationObserver {
    pub(crate) fn new(
        events: Sender<ProgressEvent>,
        graph: &DependencyGraph,
        derivations: impl IntoIterator<Item = String>,
        log_limit: usize,
    ) -> Self {
        let derivations = derivations.into_iter().collect::<BTreeSet<_>>();
        let outputs = derivations
            .iter()
            .filter_map(|drv_path| graph.get(drv_path))
            .flat_map(|node| {
                node.outputs
                    .values()
                    .flatten()
                    .map(|path| (path.clone(), node.drv_path.clone()))
            })
            .collect();
        Self {
            state: Mutex::new(ObserverState {
                events: Some(events),
                derivations,
                outputs,
                activities: BTreeMap::new(),
                started: BTreeSet::new(),
                log: BoundedLog::new(log_limit),
            }),
        }
    }

    /// Closes the progress channel so the forwarding thread can finish.
    pub(crate) fn close(&self) {
        self.state().events = None;
    }

    /// Returns the reconstructed plain-text log and whether it omitted any of it.
    pub(crate) fn take_log(&self) -> (Vec<u8>, bool) {
        self.state().log.take()
    }

    /// Recovers the guard after a panic rather than dropping the stream on the floor: an observer
    /// that silently stopped recording would strand the forwarding thread and hide the failure.
    fn state(&self) -> MutexGuard<'_, ObserverState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Poisons the state lock so a test can exercise recovery.
    #[cfg(test)]
    pub(crate) fn poison(&self) {
        let joined = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _guard = self.state.lock().expect("state");
                    panic!("poisoning the observer state");
                })
                .join()
        });
        assert!(joined.is_err(), "the poisoning thread must panic");
    }
}

impl LineObserver for RealizationObserver {
    fn line(&self, line: &[u8]) {
        let Some(parsed) = parse_line(line) else {
            return;
        };
        self.state().apply(&parsed);
    }
}

impl ObserverState {
    fn apply(&mut self, parsed: &LogLine) {
        match parsed.action.as_str() {
            "start" => self.start(parsed),
            "stop" => {
                self.activities.remove(&parsed.id);
            }
            "result" => self.result(parsed),
            "msg" => self.record_line(None, parsed.msg.as_bytes()),
            _ => {}
        }
    }

    fn start(&mut self, parsed: &LogLine) {
        let Some(drv_path) = self.attribute(parsed) else {
            return;
        };
        self.activities.insert(parsed.id, drv_path.clone());
        if self.started.insert(drv_path.clone()) {
            self.emit(ProgressEvent::NodeStarted { drv_path });
        }
    }

    fn attribute(&self, parsed: &LogLine) -> Option<String> {
        let field = field_str(&parsed.fields, 0)?;
        match parsed.kind {
            ACTIVITY_BUILD => self.derivations.get(field).cloned(),
            ACTIVITY_COPY_PATH | ACTIVITY_SUBSTITUTE => self.outputs.get(field).cloned(),
            _ => None,
        }
    }

    fn result(&mut self, parsed: &LogLine) {
        match parsed.kind {
            RESULT_BUILD_LOG_LINE | RESULT_POST_BUILD_LOG_LINE => {
                if let Some(text) = field_str(&parsed.fields, 0) {
                    let text = text.to_owned();
                    let label = self
                        .activities
                        .get(&parsed.id)
                        .map(|drv_path| derivation_label(drv_path).to_owned());
                    self.record_line(label.as_deref(), text.as_bytes());
                }
            }
            RESULT_PROGRESS => {
                let Some(drv_path) = self.activities.get(&parsed.id).cloned() else {
                    return;
                };
                let (Some(done), Some(expected)) =
                    (field_u64(&parsed.fields, 0), field_u64(&parsed.fields, 1))
                else {
                    return;
                };
                if expected > 0 {
                    self.emit(ProgressEvent::NodeProgress {
                        drv_path,
                        done,
                        expected,
                    });
                }
            }
            _ => {}
        }
    }

    fn emit(&mut self, event: ProgressEvent) {
        if let Some(events) = &self.events
            && events.send(event).is_err()
        {
            self.events = None;
        }
    }

    /// Records one log line under the derivation that produced it, the way nix's own plain output
    /// prefixes interleaved build output.
    fn record_line(&mut self, label: Option<&str>, text: &[u8]) {
        if text.is_empty() {
            return;
        }
        let mut line = Vec::with_capacity(text.len() + 1);
        if let Some(label) = label {
            line.extend_from_slice(label.as_bytes());
            line.extend_from_slice(b"> ");
        }
        line.extend_from_slice(text);
        line.push(b'\n');
        self.log.push(&line);
    }
}

/// Names a derivation the way nix does in build output: the store path without its hash or suffix.
fn derivation_label(drv_path: &str) -> &str {
    let name = drv_path.rsplit('/').next().unwrap_or(drv_path);
    let name = name.strip_suffix(".drv").unwrap_or(name);
    name.split_once('-')
        .filter(|(hash, _)| hash.len() == 32)
        .map_or(name, |(_, rest)| rest)
}

impl BoundedLog {
    const fn new(limit: usize) -> Self {
        Self {
            limit,
            head: Vec::new(),
            tail: Vec::new(),
            omitted: 0,
        }
    }

    /// Appends one complete line, keeping head and tail line-aligned so the two halves never join
    /// into a line the build never printed.
    fn push(&mut self, line: &[u8]) {
        let head_limit = self.limit.div_ceil(2);
        let tail_limit = self.limit - head_limit;
        if self.tail.is_empty() && self.omitted == 0 && self.head.len() + line.len() <= head_limit {
            self.head.extend_from_slice(line);
            return;
        }
        if tail_limit == 0 {
            self.omitted = self.omitted.saturating_add(line.len());
            return;
        }
        self.tail.extend_from_slice(line);
        if self.tail.len() > tail_limit {
            let excess = self.tail.len() - tail_limit;
            let cut = self.tail[excess..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(excess, |index| excess + index + 1);
            self.tail.drain(..cut);
            self.omitted = self.omitted.saturating_add(cut);
        }
    }

    /// Returns the retained head and tail joined, and whether anything between them was dropped.
    fn take(&mut self) -> (Vec<u8>, bool) {
        let truncated = self.omitted > 0;
        let mut bytes = std::mem::take(&mut self.head);
        bytes.append(&mut self.tail);
        self.omitted = 0;
        (bytes, truncated)
    }
}
