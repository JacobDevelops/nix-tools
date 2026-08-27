use nix_tools_engine::{NodeState, Phase};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState},
};

use super::model::{JobStatus, Model, PhaseStatus};

const PHASES: [(Phase, &str); 5] = [
    (Phase::Discovery, "DISCOVER"),
    (Phase::Evaluation, "EVALUATE"),
    (Phase::Graph, "MAP"),
    (Phase::Probe, "PROBE"),
    (Phase::Realization, "REALIZE"),
];

pub fn render(frame: &mut Frame<'_>, model: &Model) {
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .split(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" nt ", Style::new().fg(Color::Black).bg(Color::Cyan).bold()),
            Span::raw("  "),
            Span::styled(&model.title, Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(outcome_label(model), outcome_style(model)),
        ]))
        .block(panel()),
        areas[0],
    );
    frame.render_widget(phase_rail(model).block(panel()), areas[1]);
    render_jobs(frame, model, areas[2]);
    let footer = if frame.area().width < 64 {
        " j/k select  ? help  q cancel"
    } else {
        " ↑/↓ select  j/k navigate  ? help  q/Ctrl-C cancel"
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::new().fg(Color::DarkGray)),
        areas[3],
    );
    if model.help_visible() {
        render_help(frame);
    }
}

fn render_help(frame: &mut Frame<'_>) {
    let area = frame.area();
    let width = area.width.saturating_sub(8).min(58);
    let height = area.height.saturating_sub(4).min(12);
    let popup = ratatui::layout::Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("↑/k   select previous job"),
            Line::from("↓/j   select next job"),
            Line::from("q     cancel active work"),
            Line::from("Ctrl-C cancel active work"),
            Line::from("?/Esc toggle this help"),
        ])
        .block(panel().title(" keys ")),
        popup,
    );
}

fn render_jobs(frame: &mut Frame<'_>, model: &Model, area: ratatui::layout::Rect) {
    let regions = if area.width >= 72 && area.height >= 8 {
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).split(area)
    } else {
        Layout::horizontal([Constraint::Percentage(100), Constraint::Length(0)]).split(area)
    };
    let rows = model.jobs().iter().map(|job| {
        let dependencies = job
            .dependencies
            .iter()
            .filter_map(|index| model.jobs().get(*index))
            .map(|dependency| dependency.label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Row::new([
            Cell::from(status_symbol(job.status)).style(status_style(job.status)),
            Cell::from(job.label.as_str()),
            Cell::from(dependencies),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Percentage(48),
            Constraint::Percentage(52),
        ],
    )
    .header(
        Row::new(["", "JOB", "NEEDS"]).style(
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .row_highlight_style(
        Style::new()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("› ")
    .block(panel().title(" dependency map "));
    let mut state = TableState::default().with_selected(model.selected());
    frame.render_stateful_widget(table, regions[0], &mut state);

    if regions[1].width > 0 {
        let detail = model
            .selected()
            .and_then(|selected| model.jobs().get(selected))
            .map_or_else(
                || "waiting for graph".to_owned(),
                |job| {
                    let dependencies = job
                        .dependencies
                        .iter()
                        .filter_map(|index| model.jobs().get(*index))
                        .map(|dependency| dependency.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "{}\n\nstatus: {}\ndepends on: {}\n\n{}",
                        job.label,
                        status_name(job.status),
                        if dependencies.is_empty() {
                            "none"
                        } else {
                            &dependencies
                        },
                        job.drv_path
                    )
                },
            );
        frame.render_widget(
            Paragraph::new(detail)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(panel().title(" selected ")),
            regions[1],
        );
    }
}

fn phase_rail(model: &Model) -> Paragraph<'static> {
    let mut spans = Vec::new();
    for (index, (phase, label)) in PHASES.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" ─ ", Style::new().fg(Color::DarkGray)));
        }
        let (symbol, style) = match model.phase(phase) {
            PhaseStatus::Waiting => ("○", Style::new().fg(Color::DarkGray)),
            PhaseStatus::Active => ("◆", Style::new().fg(Color::Yellow).bold()),
            PhaseStatus::Complete => ("●", Style::new().fg(Color::Green)),
        };
        spans.push(Span::styled(format!("{symbol} {label}"), style));
    }
    Paragraph::new(Line::from(spans))
}

fn panel() -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
}

fn outcome_label(model: &Model) -> &'static str {
    match model.outcome {
        Some(nix_tools_engine::ManifestOutcome::Success) => "SUCCESS",
        Some(nix_tools_engine::ManifestOutcome::Failed) => "FAILED",
        Some(nix_tools_engine::ManifestOutcome::Cancelled) => "CANCELLED",
        None => "RUNNING",
    }
}

fn outcome_style(model: &Model) -> Style {
    match model.outcome {
        Some(nix_tools_engine::ManifestOutcome::Success) => Style::new().fg(Color::Green).bold(),
        Some(nix_tools_engine::ManifestOutcome::Failed) => Style::new().fg(Color::Red).bold(),
        Some(nix_tools_engine::ManifestOutcome::Cancelled) => {
            Style::new().fg(Color::Magenta).bold()
        }
        None => Style::new().fg(Color::Yellow),
    }
}

const fn status_symbol(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "○",
        JobStatus::Running => "◆",
        JobStatus::Settled(NodeState::Cached) => "●",
        JobStatus::Settled(NodeState::Substituted) => "↓",
        JobStatus::Settled(NodeState::Built | NodeState::Realized) => "✓",
        JobStatus::Settled(NodeState::Failed) => "✕",
        JobStatus::Settled(NodeState::Skipped) => "—",
        JobStatus::Settled(NodeState::Cancelled) => "!",
    }
}

const fn status_name(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Settled(NodeState::Cached) => "cached",
        JobStatus::Settled(NodeState::Substituted) => "substituted",
        JobStatus::Settled(NodeState::Built) => "built",
        JobStatus::Settled(NodeState::Realized) => "realized",
        JobStatus::Settled(NodeState::Failed) => "failed",
        JobStatus::Settled(NodeState::Skipped) => "skipped",
        JobStatus::Settled(NodeState::Cancelled) => "cancelled",
    }
}

const fn status_style(status: JobStatus) -> Style {
    match status {
        JobStatus::Queued => Style::new().fg(Color::DarkGray),
        JobStatus::Running => Style::new().fg(Color::Yellow),
        JobStatus::Settled(NodeState::Cached | NodeState::Built | NodeState::Realized) => {
            Style::new().fg(Color::Green)
        }
        JobStatus::Settled(NodeState::Substituted) => Style::new().fg(Color::Cyan),
        JobStatus::Settled(NodeState::Failed) => Style::new().fg(Color::Red),
        JobStatus::Settled(NodeState::Skipped | NodeState::Cancelled) => {
            Style::new().fg(Color::Magenta)
        }
    }
}
