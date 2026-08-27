use std::collections::{BTreeMap, BTreeSet};

use nix_tools_engine::{DerivationNode, Phase, ProgressEvent};
use ratatui::{Terminal, backend::TestBackend};

use super::{model::Model, view::render};

#[test]
fn full_frame_exposes_phases_jobs_and_dependencies() {
    let mut model = Model::new("nt check");
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
    let mut model = Model::new("nt build");
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
