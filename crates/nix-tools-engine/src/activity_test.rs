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
    let (sender, receiver) = mpsc::channel();
    let observer = RealizationObserver::new(sender, &graph(), [DRV.to_owned()], 4096);
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
    let (sender, receiver) = mpsc::channel();
    let observer = RealizationObserver::new(sender, &graph(), [DRV.to_owned()], 8);
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
    let (sender, receiver) = mpsc::channel();
    let observer = RealizationObserver::new(sender, &graph(), [DRV.to_owned()], 512);
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
    let (sender, receiver) = mpsc::channel();
    let observer = RealizationObserver::new(sender, &graph(), [DRV.to_owned()], 4096);
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
