//! Incremental reader for the `nix build --log-format internal-json` activity stream.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::mpsc::Sender;

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
    log: Vec<u8>,
    log_limit: usize,
    log_truncated: bool,
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
                log: Vec::new(),
                log_limit,
                log_truncated: false,
            }),
        }
    }

    /// Closes the progress channel so the forwarding thread can finish.
    pub(crate) fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.events = None;
        }
    }

    /// Returns the reconstructed plain-text log and whether it was bounded.
    pub(crate) fn take_log(&self) -> (Vec<u8>, bool) {
        self.state.lock().map_or_else(
            |_| (Vec::new(), false),
            |mut state| (std::mem::take(&mut state.log), state.log_truncated),
        )
    }
}

impl LineObserver for RealizationObserver {
    fn line(&self, line: &[u8]) {
        let Some(parsed) = parse_line(line) else {
            return;
        };
        if let Ok(mut state) = self.state.lock() {
            state.apply(&parsed);
        }
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
            "msg" => self.record(parsed.msg.as_bytes()),
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
                    self.record(text.as_bytes());
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

    fn record(&mut self, text: &[u8]) {
        if text.is_empty() {
            return;
        }
        let remaining = self.log_limit.saturating_sub(self.log.len());
        if remaining == 0 {
            self.log_truncated = true;
            return;
        }
        let retained = text.len().min(remaining.saturating_sub(1));
        self.log.extend_from_slice(&text[..retained]);
        self.log.push(b'\n');
        self.log_truncated |= retained < text.len();
    }
}
