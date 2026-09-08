//! Incremental reader for the `nix build --log-format internal-json` activity stream.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Mutex, MutexGuard, PoisonError};

use nix_tools_core::process::{Cancellation, LineObserver};
use nix_tools_core::redaction::Redactor;
use nix_tools_core::terminal::normalize_terminal_output;
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
/// Stands in for the lines an over-long log dropped between its retained head and tail.
const MARKER: &[u8] = b"[log truncated]\n";

/// One decoded activity line. Fields absent from a given action stay at their defaults because the
/// stream is a diagnostic channel rather than a stable contract.
#[derive(Debug, Default, Deserialize, Eq, PartialEq)]
struct LogLine {
    #[serde(default)]
    action: String,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    parent: u64,
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
/// Every reported activity is attributed to a derivation in the validated graph, or dropped. The
/// observer also rebuilds a human-readable log from the message and build-log records, because
/// selecting the JSON log format removes the plain text a diagnostic would otherwise carry.
pub(crate) struct RealizationObserver {
    state: Mutex<ObserverState>,
    delivery: Mutex<()>,
}

struct ObserverState {
    events: Option<SyncSender<ProgressEvent>>,
    derivations: BTreeSet<String>,
    outputs: BTreeMap<String, String>,
    activities: BTreeMap<u64, Activity>,
    parents: BTreeMap<u64, u64>,
    running: BTreeMap<String, usize>,
    completed_builds: BTreeSet<String>,
    log: BoundedLog,
    context: BoundedLog,
    node_logs: BTreeMap<String, BoundedLog>,
    redactor: Redactor,
    cancellation: Cancellation,
}

/// One activity nix reported for a derivation in the validated graph.
struct Activity {
    drv_path: String,
    /// Whether the activity moves bytes, which is the only progress the caller can read as one.
    transfer: bool,
    /// Cleared on `stop`, so a late log line still knows its derivation without reviving progress.
    running: bool,
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
        events: SyncSender<ProgressEvent>,
        graph: &DependencyGraph,
        derivations: impl IntoIterator<Item = String>,
        log_limit: usize,
        redactor: Redactor,
        cancellation: Cancellation,
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
            delivery: Mutex::new(()),
            state: Mutex::new(ObserverState {
                events: Some(events),
                derivations,
                outputs,
                activities: BTreeMap::new(),
                parents: BTreeMap::new(),
                running: BTreeMap::new(),
                completed_builds: BTreeSet::new(),
                log: BoundedLog::new(log_limit),
                context: BoundedLog::new(log_limit),
                node_logs: BTreeMap::new(),
                redactor,
                cancellation,
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

    pub(crate) fn take_context(&self) -> (Vec<u8>, bool) {
        self.state().context.take()
    }

    pub(crate) fn take_node_log(&self, drv_path: &str) -> Option<(Vec<u8>, bool)> {
        self.state()
            .node_logs
            .remove(drv_path)
            .map(|mut log| log.take())
    }

    /// Recovers the guard after a panic rather than dropping the stream on the floor: an observer
    /// that silently stopped recording would strand the forwarding thread and hide the failure.
    fn state(&self) -> MutexGuard<'_, ObserverState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn deliver(&self, mut event: ProgressEvent) -> bool {
        loop {
            let result = self.state().emit(event);
            match result {
                Ok(()) => return true,
                Err(TrySendError::Disconnected(_)) => return false,
                Err(TrySendError::Full(pending)) => event = pending,
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
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
        // Serialize callbacks without preventing close or diagnostics from taking the state lock.
        let _delivery = self.delivery.lock().unwrap_or_else(PoisonError::into_inner);
        let events = self.state().apply(&parsed);
        for event in events.into_iter().flatten() {
            if let ProgressEvent::NodeLogLine { drv_path, line } = event {
                for line in line.lines() {
                    if !self.deliver(ProgressEvent::NodeLogLine {
                        drv_path: drv_path.clone(),
                        line: line.to_owned(),
                    }) {
                        return;
                    }
                }
            } else if !self.deliver(event) {
                return;
            }
        }
    }
}

impl ObserverState {
    fn apply(&mut self, parsed: &LogLine) -> [Option<ProgressEvent>; 2] {
        match parsed.action.as_str() {
            "start" => [self.start(parsed), None],
            "stop" => self.stop(parsed.id),
            "result" => [self.result(parsed), None],
            "msg" => {
                let (_, line) = self.record_line(None, parsed.msg.as_bytes());
                self.context.push(&line);
                [None, None]
            }
            _ => [None, None],
        }
    }

    fn start(&mut self, parsed: &LogLine) -> Option<ProgressEvent> {
        self.parents.insert(parsed.id, parsed.parent);
        if self
            .activities
            .get(&parsed.id)
            .is_some_and(|activity| activity.running)
        {
            return None;
        }
        let drv_path = self.attribute(parsed)?;
        if self.derivations.contains(&drv_path) {
            self.node_logs
                .entry(drv_path.clone())
                .or_insert_with(|| BoundedLog::new(self.log.limit));
        }
        self.activities.insert(
            parsed.id,
            Activity {
                drv_path: drv_path.clone(),
                transfer: parsed.kind != ACTIVITY_BUILD,
                running: true,
            },
        );
        let running = self.running.entry(drv_path.clone()).or_default();
        *running += 1;
        (*running == 1).then_some(ProgressEvent::NodeStarted { drv_path })
    }

    fn stop(&mut self, id: u64) -> [Option<ProgressEvent>; 2] {
        self.parents.remove(&id);
        let mut events = [None, None];
        let Some(activity) = self
            .activities
            .get_mut(&id)
            .filter(|activity| activity.running)
        else {
            return events;
        };
        activity.running = false;
        let drv_path = activity.drv_path.clone();
        if !activity.transfer {
            self.completed_builds.insert(drv_path.clone());
        }
        if let Some(running) = self.running.get_mut(&drv_path) {
            *running -= 1;
            if *running == 0 {
                self.running.remove(&drv_path);
                events[0] = Some(ProgressEvent::NodeActivityStopped {
                    drv_path: drv_path.clone(),
                });
                if self.completed_builds.remove(&drv_path) {
                    events[1] = Some(ProgressEvent::NodeProvisionalFinished {
                        drv_path,
                        state: crate::NodeState::Built,
                    });
                }
            }
        }
        events
    }

    fn attribute(&self, parsed: &LogLine) -> Option<String> {
        let field = field_str(&parsed.fields, 0)?;
        match parsed.kind {
            ACTIVITY_BUILD => (self.derivations.contains(field)
                || (field.starts_with('/')
                    && std::path::Path::new(field)
                        .extension()
                        .is_some_and(|extension| extension == "drv")))
            .then(|| field.to_owned()),
            // Nix copies an output that is already realized whenever a remote builder needs it as
            // an input, and that is not this derivation running again.
            ACTIVITY_COPY_PATH if field_str(&parsed.fields, 2).is_some_and(is_remote_store) => None,
            ACTIVITY_COPY_PATH | ACTIVITY_SUBSTITUTE => self
                .outputs
                .get(field)
                .cloned()
                .or_else(|| self.parent_derivation(parsed.parent)),
            _ => None,
        }
    }

    fn parent_derivation(&self, mut id: u64) -> Option<String> {
        let mut visited = BTreeSet::new();
        while id != 0 && visited.insert(id) {
            if let Some(activity) = self.activities.get(&id) {
                return Some(activity.drv_path.clone());
            }
            id = *self.parents.get(&id)?;
        }
        None
    }

    fn result(&mut self, parsed: &LogLine) -> Option<ProgressEvent> {
        match parsed.kind {
            RESULT_BUILD_LOG_LINE | RESULT_POST_BUILD_LOG_LINE => {
                let text = field_str(&parsed.fields, 0)?;
                let drv_path = self
                    .activities
                    .get(&parsed.id)
                    .map(|activity| activity.drv_path.clone());
                let label = drv_path.as_deref().map(derivation_label);
                let (text, line) = self.record_line(label, text.as_bytes());
                if let Some(log) = drv_path
                    .as_ref()
                    .and_then(|path| self.node_logs.get_mut(path))
                {
                    log.push(&line);
                }
                drv_path.map(|drv_path| ProgressEvent::NodeLogLine {
                    drv_path,
                    line: text,
                })
            }
            RESULT_PROGRESS => {
                let drv_path = self
                    .activities
                    .get(&parsed.id)
                    .filter(|activity| activity.running && activity.transfer)
                    .map(|activity| activity.drv_path.clone())?;
                let done = field_u64(&parsed.fields, 0)?;
                let expected = field_u64(&parsed.fields, 1)?;
                (expected > 0).then_some(ProgressEvent::NodeProgress {
                    drv_path,
                    done,
                    expected,
                })
            }
            _ => None,
        }
    }

    fn emit(&mut self, event: ProgressEvent) -> Result<(), TrySendError<ProgressEvent>> {
        if self.cancellation.signal().is_some() {
            return Err(TrySendError::Disconnected(event));
        }
        let Some(events) = &self.events else {
            return Err(TrySendError::Disconnected(event));
        };
        let result = events.try_send(event);
        if matches!(result, Err(TrySendError::Disconnected(_))) {
            self.events = None;
        }
        result
    }

    fn safe_text(&self, text: &[u8]) -> String {
        let redacted = self.redactor.redact_bytes(text);
        self.redactor
            .redact(&String::from_utf8_lossy(&normalize_terminal_output(
                &redacted,
            )))
    }

    /// Records one log line under the derivation that produced it, the way nix's own plain output
    /// prefixes interleaved build output.
    fn record_line(&mut self, label: Option<&str>, text: &[u8]) -> (String, Vec<u8>) {
        if text.is_empty() {
            return (String::new(), Vec::new());
        }
        let text = self.safe_text(text);
        let mut line = Vec::with_capacity(text.len() + 1);
        if let Some(label) = label {
            line.extend_from_slice(label.as_bytes());
            line.extend_from_slice(b"> ");
        }
        line.extend_from_slice(text.as_bytes());
        line.push(b'\n');
        self.log.push(&line);
        (text, line)
    }
}

/// Reports whether a copy destination is another machine's store, which is the direction that
/// means an already-realized output is being sent to a builder rather than fetched for us.
///
/// Unrecognised destinations count as local: a store URI this list has not seen should cost a
/// spurious start at worst, never a substitution that never reports itself at all.
fn is_remote_store(uri: &str) -> bool {
    const REMOTE_SCHEMES: [&str; 7] = [
        "ssh://",
        "ssh-ng://",
        "s3://",
        "http://",
        "https://",
        "gs://",
        "file://",
    ];
    REMOTE_SCHEMES.iter().any(|scheme| uri.starts_with(scheme))
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
    /// into a line the build never printed. A line boundary is also a character boundary, so the
    /// excerpt cannot split an encoded character either.
    fn push(&mut self, line: &[u8]) {
        if self.omitted == 0 && self.head.len() + line.len() <= self.limit {
            self.head.extend_from_slice(line);
            return;
        }
        let budget = if self.limit > MARKER.len() {
            self.limit - MARKER.len()
        } else {
            self.limit
        };
        if self.omitted == 0 {
            let head_end = self.head[..self.head.len().min(budget.div_ceil(2))]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index + 1);
            self.tail = self.head.split_off(head_end);
        }
        let tail_limit = budget - self.head.len();
        if tail_limit == 0 {
            self.omitted = self.omitted.saturating_add(line.len());
            return;
        }
        self.tail.extend_from_slice(line);
        if self.tail.len() > tail_limit {
            let cut = self.line_cut(self.tail.len() - tail_limit);
            self.tail.drain(..cut);
            self.omitted = self.omitted.saturating_add(cut);
        }
    }

    /// Returns where to cut the tail so it starts on the first line boundary at or after `excess`.
    ///
    /// A line longer than the tail itself has no such boundary to offer, so rather than discard the
    /// line whole, its ending is kept from the first character boundary instead.
    fn line_cut(&self, excess: usize) -> usize {
        let line = self.tail[excess..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| excess + index + 1);
        match line {
            Some(cut) if cut < self.tail.len() => cut,
            _ => {
                let mut cut = excess;
                while cut < self.tail.len() && self.tail[cut] & 0b1100_0000 == 0b1000_0000 {
                    cut += 1;
                }
                cut
            }
        }
    }

    /// Returns the retained head and tail joined, and whether anything between them was dropped.
    ///
    /// The marker is there for a person reading the excerpt. The returned flag stays the signal a
    /// caller acts on, so nothing has to parse the text back.
    fn take(&mut self) -> (Vec<u8>, bool) {
        let truncated = self.omitted > 0;
        let mut bytes = std::mem::take(&mut self.head);
        if truncated && bytes.len() + MARKER.len() + self.tail.len() <= self.limit {
            bytes.extend_from_slice(MARKER);
        }
        bytes.append(&mut self.tail);
        self.omitted = 0;
        (bytes, truncated)
    }
}
