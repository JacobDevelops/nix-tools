use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;

use nix_tools_core::process::LineObserver;

use crate::activity::RealizationObserver;
use crate::{DependencyGraph, DerivationNode, ProgressEvent};

const DRV: &str = "/nix/store/00000000000000000000000000000000-a.drv";
const OUT: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a";
const OTHER_DRV: &str = "/nix/store/11111111111111111111111111111111-b.drv";

fn graph() -> DependencyGraph {
    let node = DerivationNode {
        drv_path: DRV.to_owned(),
        dependencies: BTreeMap::new(),
        outputs: BTreeMap::from([("out".to_owned(), Some(OUT.to_owned()))]),
    };
    DependencyGraph::new(
        BTreeMap::from([(DRV.to_owned(), node)]),
        &BTreeSet::from([DRV.to_owned()]),
        16,
    )
    .expect("graph")
}

fn observe(lines: &[&str]) -> (Vec<ProgressEvent>, String) {
    let (sender, receiver) = mpsc::sync_channel(256);
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        4096,
        nix_tools_core::redaction::Redactor::default(),
        nix_tools_core::process::Cancellation::default(),
    );
    for line in lines {
        observer.line(format!("{line}\n").as_bytes());
    }
    observer.close();
    let events = receiver.into_iter().collect();
    let (log, _) = observer.take_log();
    (events, String::from_utf8(log).expect("UTF-8 log"))
}

#[test]
fn a_build_activity_starts_its_derivation_exactly_once() {
    let (events, _) = observe(&[
        &format!(
            r#"@nix {{"action":"start","id":7,"level":3,"parent":0,"text":"building","type":105,"fields":["{DRV}","x86_64-linux","",1]}}"#
        ),
        &format!(
            r#"@nix {{"action":"start","id":8,"level":3,"parent":0,"text":"building","type":105,"fields":["{DRV}","x86_64-linux","",1]}}"#
        ),
    ]);

    assert_eq!(
        events,
        vec![ProgressEvent::NodeStarted {
            drv_path: DRV.to_owned()
        }]
    );
}

#[test]
fn a_substitution_is_attributed_through_its_output_path() {
    let (events, _) = observe(&[&format!(
        r#"@nix {{"action":"start","id":3,"level":3,"parent":0,"text":"copying","type":100,"fields":["{OUT}","https://cache.example","local"]}}"#
    )]);

    assert_eq!(
        events,
        vec![ProgressEvent::NodeStarted {
            drv_path: DRV.to_owned()
        }]
    );
}

#[test]
fn progress_records_report_bytes_for_a_started_activity() {
    let (events, _) = observe(&[
        &format!(
            r#"@nix {{"action":"start","id":3,"type":100,"fields":["{OUT}","https://cache.example","local"]}}"#
        ),
        r#"@nix {"action":"result","id":3,"type":105,"fields":[512,2048,1,0]}"#,
        r#"@nix {"action":"result","id":3,"type":105,"fields":[512,0,1,0]}"#,
        r#"@nix {"action":"stop","id":3}"#,
        r#"@nix {"action":"result","id":3,"type":105,"fields":[1024,2048,1,0]}"#,
    ]);

    assert_eq!(
        events,
        vec![
            ProgressEvent::NodeStarted {
                drv_path: DRV.to_owned()
            },
            ProgressEvent::NodeProgress {
                drv_path: DRV.to_owned(),
                done: 512,
                expected: 2048,
            },
            ProgressEvent::NodeActivityStopped {
                drv_path: DRV.to_owned(),
            },
        ]
    );
}

#[test]
fn unattributable_and_malformed_lines_are_dropped_without_error() {
    let (events, log) = observe(&[
        "warning: ignoring untrusted substituter",
        "@nix not json at all",
        r#"@nix {"action":"start","id":1,"type":101,"fields":["https://cache.example/nar/x"]}"#,
        &format!(r#"@nix {{"action":"start","id":2,"type":105,"fields":["{OTHER_DRV}"]}}"#),
        r#"@nix {"action":"start","id":4,"type":105,"fields":[]}"#,
        r#"@nix {"action":"result","id":9,"type":105,"fields":[1,2,0,0]}"#,
        "",
    ]);

    assert!(events.is_empty());
    assert!(log.is_empty());
}

#[test]
fn messages_and_build_log_lines_rebuild_a_readable_log() {
    let (_, log) = observe(&[
        r#"@nix {"action":"msg","level":0,"msg":"error: builder for a failed"}"#,
        r#"@nix {"action":"result","id":7,"type":101,"fields":["make: *** [all] Error 1"]}"#,
        r#"@nix {"action":"result","id":7,"type":107,"fields":["post-build hook failed"]}"#,
    ]);

    assert_eq!(
        log,
        "error: builder for a failed\nmake: *** [all] Error 1\npost-build hook failed\n"
    );
}

#[test]
fn the_rebuilt_log_stays_within_its_limit() {
    let (sender, receiver) = mpsc::sync_channel(256);
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        8,
        nix_tools_core::redaction::Redactor::default(),
        nix_tools_core::process::Cancellation::default(),
    );
    observer.line(br#"@nix {"action":"msg","level":0,"msg":"0123456789"}"#);
    observer.line(br#"@nix {"action":"msg","level":0,"msg":"more"}"#);
    observer.close();

    let (log, truncated) = observer.take_log();
    assert!(log.len() <= 8);
    assert!(truncated);
    assert!(receiver.into_iter().next().is_none());
}

#[test]
fn build_log_lines_name_the_derivation_that_printed_them() {
    let (_, log) = observe(&[
        &format!(
            r#"@nix {{"action":"start","id":7,"type":105,"fields":["{DRV}","x86_64-linux","",1]}}"#
        ),
        r#"@nix {"action":"result","id":7,"type":101,"fields":["make: *** [all] Error 1"]}"#,
        r#"@nix {"action":"result","id":9,"type":101,"fields":["unattributed line"]}"#,
        r#"@nix {"action":"msg","level":0,"msg":"error: builder for a failed"}"#,
    ]);

    assert_eq!(
        log,
        "a> make: *** [all] Error 1\nunattributed line\nerror: builder for a failed\n"
    );
}

#[test]
fn the_error_that_ended_a_build_survives_a_log_past_the_limit() {
    let (sender, receiver) = mpsc::sync_channel(256);
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        512,
        nix_tools_core::redaction::Redactor::default(),
        nix_tools_core::process::Cancellation::default(),
    );
    for index in 0..200 {
        observer.line(
            format!(r#"@nix {{"action":"msg","level":0,"msg":"chatter {index}"}}"#).as_bytes(),
        );
    }
    observer.line(br#"@nix {"action":"msg","level":0,"msg":"error: builder failed"}"#);
    observer.close();

    let (log, truncated) = observer.take_log();
    let log = String::from_utf8(log).expect("UTF-8 log");
    assert!(truncated);
    assert!(log.len() <= 512, "log stayed bounded: {}", log.len());
    assert!(
        log.ends_with("error: builder failed\n"),
        "the terminating error must survive: {log}"
    );
    assert!(
        log.starts_with("chatter 0\n"),
        "the opening survives: {log}"
    );
    assert!(
        log.lines().all(|line| !line.is_empty()),
        "head and tail must not join into a partial line: {log}"
    );
    drop(receiver);
}

#[test]
fn a_poisoned_state_keeps_recording_instead_of_stalling_the_stream() {
    let (sender, receiver) = mpsc::sync_channel(256);
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        4096,
        nix_tools_core::redaction::Redactor::default(),
        nix_tools_core::process::Cancellation::default(),
    );
    observer.poison();

    observer.line(
        format!(r#"@nix {{"action":"start","id":1,"type":105,"fields":["{DRV}","","",1]}}"#)
            .as_bytes(),
    );
    observer.line(br#"@nix {"action":"msg","level":0,"msg":"error: builder failed"}"#);
    observer.close();

    let (log, truncated) = observer.take_log();
    assert_eq!(log, b"error: builder failed\n");
    assert!(!truncated);
    assert_eq!(
        receiver.into_iter().collect::<Vec<_>>(),
        vec![ProgressEvent::NodeStarted {
            drv_path: DRV.to_owned()
        }]
    );
}

#[test]
fn copying_a_realized_output_to_a_remote_builder_is_not_this_derivation_running() {
    let (events, _) = observe(&[&format!(
        r#"@nix {{"action":"start","id":4,"type":100,"fields":["{OUT}","local","ssh://builder"]}}"#
    )]);

    assert!(events.is_empty());
}

#[test]
fn a_post_build_line_keeps_its_derivation_after_the_activity_stopped() {
    let (_, log) = observe(&[
        &format!(
            r#"@nix {{"action":"start","id":7,"type":105,"fields":["{DRV}","x86_64-linux","",1]}}"#
        ),
        r#"@nix {"action":"stop","id":7}"#,
        r#"@nix {"action":"result","id":7,"type":107,"fields":["post-build hook failed"]}"#,
    ]);

    assert_eq!(log, "a> post-build hook failed\n");
}

#[test]
fn only_transfers_report_progress_a_caller_can_read_as_bytes() {
    let (events, _) = observe(&[
        &format!(
            r#"@nix {{"action":"start","id":7,"type":105,"fields":["{DRV}","x86_64-linux","",1]}}"#
        ),
        r#"@nix {"action":"result","id":7,"type":105,"fields":[3,8,1,0]}"#,
    ]);

    assert_eq!(
        events,
        vec![ProgressEvent::NodeStarted {
            drv_path: DRV.to_owned()
        }]
    );
}

#[test]
fn an_over_long_log_marks_where_it_dropped_lines() {
    let (sender, receiver) = mpsc::sync_channel(256);
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        256,
        nix_tools_core::redaction::Redactor::default(),
        nix_tools_core::process::Cancellation::default(),
    );
    for index in 0..100 {
        observer.line(
            format!(r#"@nix {{"action":"msg","level":0,"msg":"chatter {index}"}}"#).as_bytes(),
        );
    }
    observer.close();

    let (log, truncated) = observer.take_log();
    let log = String::from_utf8(log).expect("UTF-8 log");
    assert!(truncated);
    assert!(log.len() <= 256, "excerpt stayed bounded: {}", log.len());
    assert!(
        log.contains("[log truncated]\n"),
        "a reader must see where lines went missing: {log}"
    );
    drop(receiver);
}

#[test]
fn an_unrecognised_copy_destination_is_treated_as_this_machine() {
    let (events, _) = observe(&[&format!(
        r#"@nix {{"action":"start","id":4,"type":100,"fields":["{OUT}","https://cache.example","local-overlay"]}}"#
    )]);

    assert_eq!(
        events,
        vec![ProgressEvent::NodeStarted {
            drv_path: DRV.to_owned()
        }],
        "an unknown store URI must cost a spurious start, never a substitution that never reports"
    );
}

#[test]
fn a_line_longer_than_the_excerpt_keeps_its_ending() {
    let (sender, receiver) = mpsc::sync_channel(256);
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        256,
        nix_tools_core::redaction::Redactor::default(),
        nix_tools_core::process::Cancellation::default(),
    );
    let message = format!("{} TERMINAL ERROR", "x".repeat(400));
    observer.line(format!(r#"@nix {{"action":"msg","level":0,"msg":"{message}"}}"#).as_bytes());
    observer.close();

    let (log, truncated) = observer.take_log();
    let log = String::from_utf8(log).expect("UTF-8 log");
    assert!(truncated);
    assert!(log.len() <= 256, "excerpt stayed bounded: {}", log.len());
    assert!(
        log.ends_with(" TERMINAL ERROR\n"),
        "one long error line must not be dropped whole: {log}"
    );
    drop(receiver);
}

#[test]
fn a_log_within_the_excerpt_limit_is_preserved_in_full() {
    let (sender, _) = mpsc::sync_channel(256);
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        256,
        nix_tools_core::redaction::Redactor::default(),
        nix_tools_core::process::Cancellation::default(),
    );
    let message = format!("error: {}", "x".repeat(180));
    observer.line(format!(r#"@nix {{"action":"msg","msg":"{message}"}}"#).as_bytes());
    observer.line(br#"@nix {"action":"msg","msg":"last line"}"#);

    let (log, truncated) = observer.take_log();
    assert_eq!(log, format!("{message}\nlast line\n").as_bytes());
    assert!(!truncated);
}

#[test]
fn a_tiny_excerpt_retains_text_without_a_truncation_marker() {
    let (sender, _) = mpsc::sync_channel(256);
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        8,
        nix_tools_core::redaction::Redactor::default(),
        nix_tools_core::process::Cancellation::default(),
    );
    observer.line(br#"@nix {"action":"msg","msg":"error: failed"}"#);

    let (log, truncated) = observer.take_log();
    assert_eq!(log, b" failed\n");
    assert!(truncated);
}

#[test]
fn overlapping_activities_stop_only_when_the_last_one_stops_and_can_restart() {
    let (events, _) = observe(&[
        &format!(r#"@nix {{"action":"start","id":1,"type":108,"fields":["{OUT}"]}}"#),
        &format!(
            r#"@nix {{"action":"start","id":2,"type":100,"fields":["{OUT}","cache","local"]}}"#
        ),
        r#"@nix {"action":"stop","id":2}"#,
        r#"@nix {"action":"stop","id":2}"#,
        r#"@nix {"action":"stop","id":1}"#,
        &format!(r#"@nix {{"action":"start","id":3,"type":105,"fields":["{DRV}"]}}"#),
        r#"@nix {"action":"stop","id":3}"#,
    ]);
    assert_eq!(
        events,
        vec![
            ProgressEvent::NodeStarted {
                drv_path: DRV.to_owned()
            },
            ProgressEvent::NodeActivityStopped {
                drv_path: DRV.to_owned()
            },
            ProgressEvent::NodeStarted {
                drv_path: DRV.to_owned()
            },
            ProgressEvent::NodeActivityStopped {
                drv_path: DRV.to_owned()
            },
            ProgressEvent::NodeProvisionalFinished {
                drv_path: DRV.to_owned(),
                state: crate::NodeState::Built
            },
        ]
    );
}

#[test]
fn build_logs_stream_with_attribution_and_stop_is_provisional() {
    let (events, _) = observe(&[
        &format!(r#"@nix {{"action":"start","id":7,"type":105,"fields":["{DRV}"]}}"#),
        r#"@nix {"action":"result","id":7,"type":101,"fields":["hello"]}"#,
        r#"@nix {"action":"stop","id":7}"#,
        r#"@nix {"action":"result","id":7,"type":107,"fields":["after"]}"#,
    ]);
    assert!(events.contains(&ProgressEvent::NodeLogLine {
        drv_path: DRV.to_owned(),
        line: "hello".to_owned()
    }));
    assert!(events.contains(&ProgressEvent::NodeLogLine {
        drv_path: DRV.to_owned(),
        line: "after".to_owned()
    }));
    assert!(events.contains(&ProgressEvent::NodeProvisionalFinished {
        drv_path: DRV.to_owned(),
        state: crate::NodeState::Built
    }));
}

#[test]
fn decoded_logs_normalize_controls_and_redact_registered_secrets() {
    let (sender, receiver) = mpsc::sync_channel(256);
    let redactor = nix_tools_core::redaction::Redactor::default();
    redactor.register(b"private-value");
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        4096,
        redactor,
        nix_tools_core::process::Cancellation::default(),
    );
    observer.line(
        format!(r#"@nix {{"action":"start","id":7,"type":105,"fields":["{DRV}"]}}"#).as_bytes(),
    );
    observer.line(br#"@nix {"action":"result","id":7,"type":101,"fields":["\u001b[31mprivate-\u001b[0mvalue\u001b]52;c;clipboard\u0007"]}"#);
    observer.close();
    assert!(receiver.into_iter().any(|event| event
        == ProgressEvent::NodeLogLine {
            drv_path: DRV.to_owned(),
            line: "[REDACTED]".to_owned()
        }));
    let (log, _) = observer.take_log();
    assert_eq!(log, b"a> [REDACTED]\n");
}

#[test]
fn completed_build_is_provisional_after_overlapping_transfer_stops() {
    let (events, _) = observe(&[
        &format!(r#"@nix {{"action":"start","id":7,"type":105,"fields":["{DRV}"]}}"#),
        &format!(r#"@nix {{"action":"start","id":8,"type":108,"fields":["{OUT}"]}}"#),
        r#"@nix {"action":"stop","id":7}"#,
        r#"@nix {"action":"stop","id":8}"#,
    ]);
    assert!(matches!(
        events.last(),
        Some(ProgressEvent::NodeProvisionalFinished {
            state: crate::NodeState::Built,
            ..
        })
    ));
}

#[test]
fn cancellation_unblocks_a_full_live_log_queue() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let cancellation = nix_tools_core::process::Cancellation::default();
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        4096,
        nix_tools_core::redaction::Redactor::default(),
        cancellation.clone(),
    );
    observer.line(
        format!(r#"@nix {{"action":"start","id":7,"type":105,"fields":["{DRV}"]}}"#).as_bytes(),
    );
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let line = serde_json::json!({"action":"result", "id":7, "type":101, "fields":["compiler output\n".repeat(10_000)]});
            observer.line(format!("@nix {line}").as_bytes());
        });
        cancellation.request(2);
        worker.join().unwrap();
    });
    assert!(matches!(
        receiver.try_recv(),
        Ok(ProgressEvent::NodeStarted { .. })
    ));
    observer.close();
    assert!(observer.take_log().1);
}

#[test]
fn a_live_log_flood_preserves_the_final_lines_through_a_bounded_queue() {
    let (sender, receiver) = mpsc::sync_channel(8);
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        4096,
        nix_tools_core::redaction::Redactor::default(),
        nix_tools_core::process::Cancellation::default(),
    );
    std::thread::scope(|scope| {
        let consumer = scope.spawn(|| {
            let mut lines = 0;
            let mut last = String::new();
            for event in receiver {
                if let ProgressEvent::NodeLogLine { line, .. } = event {
                    lines += 1;
                    last = line;
                }
            }
            (lines, last)
        });
        observer.line(
            format!(r#"@nix {{"action":"start","id":7,"type":105,"fields":["{DRV}"]}}"#).as_bytes(),
        );
        let text = format!("{}final compiler line", "compiler output\n".repeat(1_000));
        let line = serde_json::json!({"action":"result", "id":7, "type":101, "fields":[text]});
        observer.line(format!("@nix {line}").as_bytes());
        observer.close();
        assert_eq!(
            consumer.join().unwrap(),
            (1_001, "final compiler line".to_owned())
        );
    });
}

#[test]
fn shared_context_survives_before_activity_and_stays_out_of_node_excerpts() {
    let (sender, _receiver) = mpsc::sync_channel(256);
    let observer = RealizationObserver::new(
        sender,
        &graph(),
        [DRV.to_owned()],
        4096,
        nix_tools_core::redaction::Redactor::default(),
        nix_tools_core::process::Cancellation::default(),
    );
    observer.line(br#"@nix {"action":"msg","msg":"early Nix warning"}"#);
    observer.line(
        format!(r#"@nix {{"action":"start","id":7,"type":105,"fields":["{DRV}"]}}"#).as_bytes(),
    );
    observer.line(br#"@nix {"action":"msg","msg":"global build failure"}"#);
    assert_eq!(observer.take_node_log(DRV), Some((Vec::new(), false)));
    assert_eq!(
        observer.take_context(),
        (b"early Nix warning\nglobal build failure\n".to_vec(), false)
    );
}
