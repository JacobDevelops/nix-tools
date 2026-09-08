use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use nix_tools_engine::{DerivationNode, Phase, ProgressEvent};
use ratatui::{Terminal, backend::TestBackend};

use super::{model::Model, view::render};

#[test]
fn full_frame_exposes_phases_jobs_and_dependencies() {
    let mut model = Model::fixed("nt check");
    model.apply(ProgressEvent::PhaseStarted(Phase::Realization));
    model.apply(ProgressEvent::GraphDiscovered(vec![
        node("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv", &[]),
        node(
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-cli.drv",
            &["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv"],
        ),
    ]));
    model.select_next();

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &model)).unwrap();
    let screen = terminal.backend().to_string();

    assert!(screen.contains("nt check"));
    assert!(screen.contains("DISCOVER"));
    assert!(screen.contains("REALIZE"));
    assert!(screen.contains("core"));
    assert!(screen.contains("cli"));
    assert!(screen.contains("depends on: core"));
    assert!(screen.contains("↑/↓ select"));
}

#[test]
fn narrow_frame_keeps_the_job_map_and_controls_visible() {
    let mut model = Model::fixed("nt build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node(
        "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv",
        &[],
    )]));

    let backend = TestBackend::new(48, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &model)).unwrap();
    let screen = terminal.backend().to_string();

    assert!(screen.contains("core"));
    assert!(screen.contains("q cancel"));
}

#[test]
fn filtered_frame_shows_only_matches_and_exposes_filter_controls() {
    let mut model = Model::fixed("nt check");
    model.apply(ProgressEvent::GraphDiscovered(vec![
        node(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-api-test.drv",
            &[],
        ),
        node(
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-web-test.drv",
            &[],
        ),
        node(
            "/nix/store/cccccccccccccccccccccccccccccccc-api-lint.drv",
            &[],
        ),
    ]));
    model.start_filter_input();
    for character in "api".chars() {
        model.push_filter_character(character);
    }
    model.select_last();
    model.apply(ProgressEvent::NodeLogLine {
        drv_path: "/nix/store/cccccccccccccccccccccccccccccccc-api-lint.drv".to_owned(),
        line: "selected filtered log".to_owned(),
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    terminal.draw(|frame| render(frame, &model)).unwrap();
    let screen = terminal.backend().to_string();

    assert!(screen.contains("api-test"));
    assert!(screen.contains("api-lint"));
    assert!(!screen.contains("web-test"));
    assert!(screen.contains("selected filtered log"));
    assert!(screen.contains("/api"));
    assert!(screen.contains("Esc clear"));
}

#[test]
fn stopped_activity_waits_without_a_spinner_or_a_success_status() {
    let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv";
    let mut model = Model::fixed("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node(path, &[])]));
    model.apply(ProgressEvent::NodeStarted {
        drv_path: path.to_owned(),
    });
    model.advance(Duration::from_secs(1));
    model.apply(ProgressEvent::NodeActivityStopped {
        drv_path: path.to_owned(),
    });
    model.advance(Duration::from_mins(10));

    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    terminal.draw(|frame| render(frame, &model)).unwrap();
    let screen = terminal.backend().to_string();
    assert!(screen.contains("awaiting result"));
    assert!(screen.contains("elapsed: 1.0s"));
    assert!(screen.contains("0/1"));
    assert!(screen.contains('◌'));
    assert!(
        !screen
            .chars()
            .any(|character| "⠋⠙⠹⠸⠼⠴⠦⠧✓".contains(character))
    );
}

fn node(path: &str, dependencies: &[&str]) -> Arc<DerivationNode> {
    Arc::new(DerivationNode {
        drv_path: path.to_owned(),
        dependencies: dependencies
            .iter()
            .map(|dependency| ((*dependency).to_owned(), BTreeSet::from(["out".to_owned()])))
            .collect::<BTreeMap<_, _>>(),
        outputs: BTreeMap::from([("out".to_owned(), None)]),
    })
}

#[test]
fn live_progress_shows_a_settled_counter_timings_and_reverse_dependencies() {
    let core = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv";
    let mut model = Model::fixed("nt build");
    model.apply(ProgressEvent::GraphDiscovered(vec![
        node(core, &[]),
        node(
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-cli.drv",
            &[core],
        ),
    ]));
    model.apply(ProgressEvent::NodeStarted {
        drv_path: core.to_owned(),
    });
    model.apply(ProgressEvent::NodeProgress {
        drv_path: core.to_owned(),
        done: 1_048_576,
        expected: 2_097_152,
    });
    model.advance(Duration::from_millis(1_500));

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &model)).unwrap();
    let screen = terminal.backend().to_string();

    assert!(screen.contains("0/2"));
    assert!(screen.contains("TIME"));
    assert!(
        screen.contains("1.5s"),
        "a running job renders the time its own clock reports: {screen}"
    );
    assert!(screen.contains("required by: cli"));
    assert!(screen.contains("transferred: 1.0/2.0 MiB 50%"));
}

#[test]
fn a_transfer_past_its_own_estimate_still_renders_a_whole_share() {
    let core = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-core.drv";
    let mut model = Model::fixed("nt build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node(core, &[])]));
    model.apply(ProgressEvent::NodeProgress {
        drv_path: core.to_owned(),
        done: 3_145_728,
        expected: 2_097_152,
    });

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &model)).unwrap();
    let screen = terminal.backend().to_string();

    assert!(
        screen.contains("transferred: 3.0/2.0 MiB") && screen.contains("100%"),
        "nix reports a transfer past its estimate transiently: {screen}"
    );
}

#[test]
fn selected_log_panel_follows_tail_and_scrolls_back() {
    let mut model = Model::fixed("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node("a", &[])]));
    for index in 0..20 {
        model.apply(ProgressEvent::NodeLogLine {
            drv_path: "a".to_owned(),
            line: format!("log-{index:02}"),
        });
    }
    let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
    terminal.draw(|frame| render(frame, &model)).unwrap();
    assert!(terminal.backend().to_string().contains("log-19"));
    assert!(!terminal.backend().to_string().contains("log-00"));
    model.scroll_logs(10);
    terminal.draw(|frame| render(frame, &model)).unwrap();
    assert!(terminal.backend().to_string().contains("log-09"));
    assert!(!terminal.backend().to_string().contains("log-19"));
}

#[test]
fn awaiting_result_uses_a_different_color_from_queued() {
    assert_ne!(
        super::view::status_style(super::model::JobStatus::Queued),
        super::view::status_style(super::model::JobStatus::AwaitingResult)
    );
}

#[test]
fn narrow_terminal_keeps_selected_live_logs_visible() {
    let mut model = Model::fixed("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node("a", &[])]));
    model.apply(ProgressEvent::NodeLogLine {
        drv_path: "a".to_owned(),
        line: "live compiler output".to_owned(),
    });
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal.draw(|frame| render(frame, &model)).unwrap();
    assert!(
        terminal
            .backend()
            .to_string()
            .contains("live compiler output")
    );
}

#[test]
fn repeated_log_batches_render_without_losing_the_live_tail() {
    let mut model = Model::fixed("build");
    model.apply(ProgressEvent::GraphDiscovered(vec![node("a", &[])]));
    let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
    let started = std::time::Instant::now();
    for batch in 0..40 {
        for offset in 0..256 {
            model.apply(ProgressEvent::NodeLogLine {
                drv_path: "a".to_owned(),
                line: format!("compiler line {}", batch * 256 + offset),
            });
        }
        terminal.draw(|frame| render(frame, &model)).unwrap();
        assert!(model.jobs()[0].logs.len() <= 1_000);
        assert!(
            terminal
                .backend()
                .to_string()
                .contains(&format!("compiler line {}", batch * 256 + 255))
        );
    }
    eprintln!(
        "10240 log lines, 40 rendered frames: {:?}",
        started.elapsed()
    );
}
