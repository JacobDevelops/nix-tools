use std::collections::{BTreeMap, BTreeSet};
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

fn node(path: &str, dependencies: &[&str]) -> DerivationNode {
    DerivationNode {
        drv_path: path.to_owned(),
        dependencies: dependencies
            .iter()
            .map(|dependency| ((*dependency).to_owned(), BTreeSet::from(["out".to_owned()])))
            .collect::<BTreeMap<_, _>>(),
        outputs: BTreeMap::from([("out".to_owned(), None)]),
    }
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
