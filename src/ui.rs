use std::sync::atomic::Ordering;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
    Frame,
};

use crate::app::{App, Mode, ToastLevel};
use crate::poller::WorkerState;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);

    draw_header(f, app, chunks[0]);

    if app.show_logs {
        let content = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(chunks[1]);
        draw_table(f, app, content[0]);
        draw_logs(f, app, content[1]);
    } else {
        draw_table(f, app, chunks[1]);
    }

    draw_footer(f, app, chunks[2]);
    draw_toasts(f, app, area);

    if let Mode::Detail { scroll } = app.mode {
        draw_detail_panel(f, app, area, scroll);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let backoff = app.backoff_status();

    let polling = app.is_polling.load(Ordering::Relaxed);
    let scan_span = if polling {
        let spinner = SPINNER[(app.frame as usize) % SPINNER.len()];
        Span::styled(
            format!("{spinner} Polling..."),
            Style::default().fg(Color::Cyan),
        )
    } else {
        let secs = app.last_refresh_secs();
        let ago = if secs < 60 {
            format!("{secs}s ago")
        } else {
            format!("{}m ago", secs / 60)
        };
        Span::styled(
            format!("✓ Last scan: {ago}"),
            Style::default().fg(Color::Green),
        )
    };

    let next_scan = match app.next_scan_remaining_secs() {
        Some(s) if s > 0 => format!("{s}s"),
        Some(_) => "now".to_string(),
        None => "—".to_string(),
    };

    let text = vec![
        Line::from(vec![
            Span::styled(
                format!(" Session: {} ", app.config.session),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("│ "),
            Span::styled(
                format!("Workers: {} ", app.workers.len()),
                Style::default().fg(Color::White),
            ),
            Span::raw("│ "),
            Span::styled(
                format!("Active: {} ", app.active_count()),
                Style::default().fg(Color::Green),
            ),
            Span::raw("│ "),
            Span::styled(
                format!("Idle: {} ", app.idle_count()),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw("│ "),
            Span::styled(
                format!("Queued: {} ", app.queued_count()),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::raw(format!(" Backoff: {backoff}")),
            Span::raw("   "),
            scan_span,
            Span::raw("   "),
            Span::styled(
                format!("Next scan: {next_scan}"),
                Style::default().fg(Color::Cyan),
            ),
        ]),
    ];

    let block = Block::default()
        .title(" Claude Worktree Orchestrator ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue));

    let para = Paragraph::new(text).block(block);
    f.render_widget(para, area);
}

fn draw_table(f: &mut Frame, app: &App, area: Rect) {
    let header_cells = ["WORKER", "PIPELINE", "STATE", "LAST OUTPUT"]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        });
    let header = Row::new(header_cells).height(1).bottom_margin(0);

    let rows: Vec<Row> = app
        .workers
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let is_selected = i == app.selected;
            let style = if is_selected {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let marker = if is_selected { "▶" } else { " " };
            let issue_cell = format!("{} {}", marker, w.window_name);
            let pipeline_cell = w.pipeline.clone();
            let state_cell = status_icon(&w.status);
            let output_cell = match &w.probe {
                Some(p) if p == "running" => "🔍 probing…".to_string(),
                Some(p) => format!("🔍 {p}"),
                None => w.last_output.clone(),
            };

            Row::new(vec![
                Cell::from(issue_cell).style(style),
                Cell::from(pipeline_cell).style(pipeline_style(w).patch(style)),
                Cell::from(state_cell).style(status_style(&w.status).patch(style)),
                Cell::from(output_cell).style(style),
            ])
        })
        .collect();

    let visible_height = area.height.saturating_sub(3) as usize;
    let scroll_offset = compute_scroll(app.selected, app.workers.len(), visible_height);

    let visible_rows: Vec<Row> = rows
        .into_iter()
        .skip(scroll_offset)
        .take(visible_height)
        .collect();

    let scroll_hint = if app.workers.len() > visible_height {
        format!(" {} rows (j/k or scroll)", app.workers.len())
    } else {
        String::new()
    };

    let table = Table::new(
        visible_rows,
        [
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Workers{scroll_hint} "))
            .border_style(Style::default().fg(Color::Blue)),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::BOLD));

    let mut state = TableState::default();
    f.render_stateful_widget(table, area, &mut state);
}

fn draw_logs(f: &mut Frame, app: &App, area: Rect) {
    let visible_lines = area.height.saturating_sub(2) as usize;
    let total = app.logs.len();
    let skip = total.saturating_sub(visible_lines);

    let lines: Vec<Line> = app
        .logs
        .iter()
        .skip(skip)
        .map(|l| Line::from(Span::raw(l.as_str())))
        .collect();

    let para = Paragraph::new(lines).block(
        Block::default()
            .title(" Log ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue)),
    );
    f.render_widget(para, area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let (title, content) = match &app.mode {
        Mode::Normal => {
            let hint = " [s] Send  [i] Interrupt  [b] Broadcast  [m] Merge PRs  [r] Refresh  [l] Log  [:] Cmd  [d] Detail  [p] Prompt  [n] New  [q] Quit";
            let msg = if app.status_msg.is_empty() {
                hint.to_string()
            } else {
                format!("{hint}\n {}", app.status_msg)
            };
            ("Controls".to_string(), msg)
        }
        Mode::Send => {
            let worker_name = app
                .workers
                .get(app.selected)
                .map(|w| w.window_name.as_str())
                .unwrap_or("?");
            (
                format!("Send to {worker_name}"),
                format!(" > {}_", app.input),
            )
        }
        Mode::Broadcast => (
            "Broadcast to idle workers".to_string(),
            format!(" > {}_", app.input),
        ),
        Mode::Command => ("Command".to_string(), format!(" : {}_", app.input)),
        Mode::Prompt => (
            "Prompt (Claude extracts & spins up tasks)".to_string(),
            format!(" > {}_", app.input),
        ),
        Mode::NewJob => (
            "New Job — enter issue number".to_string(),
            format!(" # {}_", app.input),
        ),
        Mode::Detail { .. } => (
            "Detail View".to_string(),
            " [j/k] scroll  [Esc] close".to_string(),
        ),
    };

    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue));

    let para = Paragraph::new(content).block(block);
    f.render_widget(para, area);
}

fn draw_detail_panel(f: &mut Frame, app: &App, area: Rect, scroll: usize) {
    let width = (area.width * 9 / 10).max(20);
    let height = (area.height * 4 / 5).max(10);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, rect);

    let worker = app.workers.get(app.selected);
    let worker_name = worker.map(|w| w.window_name.as_str()).unwrap_or("—");
    let pr = worker.and_then(|w| w.pr.as_deref()).unwrap_or("—");
    let status = worker.map(|w| w.status.as_str()).unwrap_or("—");
    let pipeline = worker.map(|w| w.pipeline.as_str()).unwrap_or("—");

    let content_height = height.saturating_sub(2) as usize;
    let body_lines = content_height.saturating_sub(1);

    let mut lines: Vec<Line> = app
        .detail_content
        .iter()
        .skip(scroll)
        .take(body_lines)
        .map(|l| Line::from(Span::raw(l.as_str())))
        .collect();

    lines.push(Line::from(Span::styled(
        "[j/k] scroll  [Esc] close",
        Style::default().fg(Color::DarkGray),
    )));

    let title = format!(" {worker_name} │ {pipeline} │ {status} │ PR: {pr} ");
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, rect);
}

fn draw_toasts(f: &mut Frame, app: &App, area: Rect) {
    if app.toasts.is_empty() {
        return;
    }

    const TOAST_WIDTH: u16 = 42;
    const TOAST_HEIGHT: u16 = 3;
    const MAX_VISIBLE: usize = 4;

    let visible: Vec<_> = app.toasts.iter().rev().take(MAX_VISIBLE).collect();

    let total_height = visible.len() as u16 * TOAST_HEIGHT;
    if area.width < TOAST_WIDTH + 2 || area.height < total_height + 2 {
        return;
    }

    let start_x = area.right().saturating_sub(TOAST_WIDTH + 1);
    let start_y = area.y + 1;

    for (i, toast) in visible.iter().enumerate() {
        let y = start_y + i as u16 * TOAST_HEIGHT;
        if y + TOAST_HEIGHT > area.bottom() {
            break;
        }

        let toast_rect = Rect {
            x: start_x,
            y,
            width: TOAST_WIDTH,
            height: TOAST_HEIGHT,
        };

        let (icon, border_color, title) = match toast.level {
            ToastLevel::Success => ("✅", Color::Green, "Done"),
            ToastLevel::Info => ("ℹ", Color::Cyan, "Info"),
            ToastLevel::Warning => ("⚠", Color::Yellow, "Warning"),
            ToastLevel::Error => ("✗", Color::Red, "Error"),
        };

        let max_msg_width = (TOAST_WIDTH as usize).saturating_sub(4);
        let msg: String = toast.message.chars().take(max_msg_width).collect();

        let block = Block::default()
            .title(format!(" {icon} {title} "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let para = Paragraph::new(msg).block(block);
        f.render_widget(para, toast_rect);
    }
}

fn pipeline_style(w: &WorkerState) -> Style {
    if w.status == "conflict" {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if w.status == "active" {
        Style::default().fg(Color::Green)
    } else if w.pr.is_some() {
        Style::default().fg(Color::Cyan)
    } else if w.worktree_exists {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn status_icon(status: &str) -> String {
    match status {
        "active" => "🟢 active".to_string(),
        "idle" => "🟡 idle".to_string(),
        "shell" => "🔴 shell".to_string(),
        "done" => "✅ done".to_string(),
        "queued" => "⏳ queued".to_string(),
        "sleeping" => "💤 sleeping".to_string(),
        "posted" => "✅ posted".to_string(),
        "no-window" => "👻 no-window".to_string(),
        "conflict" => "⚠️  conflict".to_string(),
        "probing" => "🔍 probing".to_string(),
        _ => "❓ unknown".to_string(),
    }
}

fn status_style(status: &str) -> Style {
    match status {
        "active" => Style::default().fg(Color::Green),
        "idle" => Style::default().fg(Color::Yellow),
        "shell" => Style::default().fg(Color::Red),
        "conflict" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "done" => Style::default().fg(Color::Gray),
        "queued" => Style::default().fg(Color::DarkGray),
        "sleeping" => Style::default().fg(Color::Blue),
        "posted" => Style::default().fg(Color::Cyan),
        "no-window" => Style::default().fg(Color::Magenta),
        _ => Style::default(),
    }
}

fn compute_scroll(selected: usize, total: usize, visible: usize) -> usize {
    if total <= visible {
        return 0;
    }
    if selected < visible / 2 {
        0
    } else if selected + visible / 2 >= total {
        total - visible
    } else {
        selected - visible / 2
    }
}
