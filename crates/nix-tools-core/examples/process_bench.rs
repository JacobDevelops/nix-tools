//! Repeatable subprocess throughput, idle, and cancellation workloads.
use nix_tools_core::process::{
    Cancellation, ProcessRunner, ProcessSpec, StdProcessRunner, StreamPolicy,
};
use nix_tools_core::redaction::Redactor;
use std::time::{Duration, Instant};

fn main() {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "throughput".into());
    let runner = StdProcessRunner::without_output(Duration::from_millis(10), Redactor::default());
    let (script, count) = match mode.as_str() {
        "idle" => ("sleep 0.25", 20),
        "cancel" | "cancel-relay" | "cancel-default" => ("sleep 10", 20),
        _ => ("printf 'ok\\n'", 1000),
    };
    for _ in 0..count {
        let cancellation = Cancellation::default();
        let mut spec = ProcessSpec::new("/bin/sh").args(["-c", script]);
        spec.stdout = StreamPolicy::Capture { limit: 4096 };
        spec.stderr = StreamPolicy::Capture { limit: 4096 };
        if mode == "relay" || mode == "cancel-relay" {
            spec.stdout = StreamPolicy::RelayAndCapture { limit: 4096 };
            spec.stderr = StreamPolicy::RelayAndCapture { limit: 4096 };
        }
        if mode != "cancel-default" {
            spec.cleanup_timeout = Duration::from_millis(20);
        }
        let sender = if mode.starts_with("cancel") {
            let token = cancellation.clone();
            Some(std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let started = Instant::now();
                token.request(2);
                started
            }))
        } else {
            None
        };
        let result = runner.run(&spec, &cancellation);
        assert!(result.is_ok() || mode.starts_with("cancel"));
        let finished = Instant::now();
        if let Some(sender) = sender {
            println!(
                "{}",
                finished.duration_since(sender.join().unwrap()).as_nanos()
            );
        }
    }
}
