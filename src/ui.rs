use std::sync::atomic::Ordering;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
    Frame,
};

use crate::app::{App, ConfirmAction, Mode, ToastLevel};
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

    if let Mode::Confirm {
        ref action,
        fetch_latest,
    } = app.mode
    {
        draw_confirm_panel(f, app, area, action, fetch_latest);
    }
    if let Mode::Detail { scroll } = app.mode {
        draw_detail_panel(f, app, area, scroll);
    }
    if let Mode::Settings { selected } = app.mode {
        draw_settings_panel(f, app, area, selected);
    }
    if let Mode::Help { scroll } = app.mode {
        draw_help_panel(f, area, scroll);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let backoff = app.backoff_status();
    let stats = app.event_stats();

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
            Span::raw("   "),
            Span::styled(
                format!("Merged: {}", stats.merged_count),
                Style::default().fg(Color::Green),
            ),
            Span::raw(" │ "),
            Span::styled(
                format!("Failed: {}", stats.failed_count),
                if stats.failed_count > 0 {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw(" │ "),
            Span::styled(
                format!(
                    "Avg merge: {}",
                    match stats.avg_merge_secs() {
                        Some(s) if s >= 60 => format!("{}m", s / 60),
                        Some(s) => format!("{s}s"),
                        None => "—".to_string(),
                    }
                ),
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
    let header_cells = ["WORKER", "PHASE", "STATE", "LAST OUTPUT"].iter().map(|h| {
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
            let proc_badge = process_badge(&w.process);
            let issue_cell = format!("{} {}{}", marker, w.window_name, proc_badge);
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
    match &app.mode {
        Mode::Normal => {
            draw_footer_normal(f, app, area);
        }
        _ => {
            let (title, content) = match &app.mode {
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
                    "Prompt (Claude extracts tasks)".to_string(),
                    format!(" > {}_", app.input),
                ),
                Mode::DirectPrompt => (
                    "Direct Prompt (no issue)".to_string(),
                    format!(" > {}_", app.input),
                ),
                Mode::NewJob => (
                    "New Job — issue #".to_string(),
                    format!(" # {}_", app.input),
                ),
                Mode::Confirm { ref action, .. } => {
                    let desc = match action {
                        ConfirmAction::LaunchIssue { issue_num } => {
                            format!("Confirm — launch #{issue_num}")
                        }
                        ConfirmAction::MergeAll => "Confirm — merge all PRs".to_string(),
                        ConfirmAction::MergePr { pr_num, .. } => {
                            format!("Confirm — merge PR #{pr_num}")
                        }
                        ConfirmAction::Interrupt { window_name } => {
                            format!("Confirm — interrupt {window_name}")
                        }
                    };
                    (desc, " Enter: confirm  Esc: cancel".to_string())
                }
                Mode::Detail { .. } => {
                    ("Detail".to_string(), " j/k scroll · Esc close".to_string())
                }
                Mode::Settings { .. } => (
                    "Settings".to_string(),
                    " j/k move · Enter toggle · Esc close".to_string(),
                ),
                Mode::Help { .. } => ("Help".to_string(), " j/k scroll · ?/Esc close".to_string()),
                Mode::Normal => unreachable!(),
            };

            let block = Block::default()
                .title(format!(" {title} "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue));

            let para = Paragraph::new(content).block(block);
            f.render_widget(para, area);
        }
    }
}

fn footer_key<'a>(key: &'a str, desc: &'a str) -> Vec<Span<'a>> {
    vec![
        Span::styled(
            key,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {desc}"), Style::default().fg(Color::Gray)),
    ]
}

fn draw_footer_normal(f: &mut Frame, app: &App, area: Rect) {
    let sep = Span::styled(" │ ", Style::default().fg(Color::DarkGray));

    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw(" "));

    // Worker actions
    let groups: &[(&str, &str)] = &[("s", "send"), ("i", "int"), ("b", "bcast"), ("m", "merge")];
    for (i, (k, d)) in groups.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.extend(footer_key(k, d));
    }

    spans.push(sep.clone());

    // Launch
    let launch: &[(&str, &str)] = &[("p", "prompt"), ("P", "direct"), ("n", "job")];
    for (i, (k, d)) in launch.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.extend(footer_key(k, d));
    }

    spans.push(sep.clone());

    // View
    let view: &[(&str, &str)] = &[("d", "detail"), ("l", "log"), ("c", "cfg"), ("?", "help")];
    for (i, (k, d)) in view.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.extend(footer_key(k, d));
    }

    spans.push(sep);
    spans.extend(footer_key("q", "quit"));

    let mut lines = vec![Line::from(spans)];
    if !app.status_msg.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {}", app.status_msg),
            Style::default().fg(Color::Yellow),
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}

fn draw_confirm_panel(
    f: &mut Frame,
    app: &App,
    area: Rect,
    action: &ConfirmAction,
    fetch_latest: bool,
) {
    let width = 56u16.min(area.width.saturating_sub(4));
    let has_checkbox = matches!(action, ConfirmAction::LaunchIssue { .. });
    let height = if has_checkbox { 8u16 } else { 6u16 };
    let height = height.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, rect);

    let (title, desc_spans) = match action {
        ConfirmAction::LaunchIssue { issue_num } => {
            let default_branch = app.config.default_branch();
            (
                " Confirm Launch ",
                vec![
                    Span::raw("  Launch worker for "),
                    Span::styled(
                        format!("#{issue_num}"),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" from '{default_branch}'?")),
                ],
            )
        }
        ConfirmAction::MergeAll => (
            " Confirm Merge All ",
            vec![
                Span::raw("  "),
                Span::styled(
                    "Merge all",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" open PRs that pass checks?"),
            ],
        ),
        ConfirmAction::MergePr {
            pr_num,
            worker_name,
        } => (
            " Confirm Merge ",
            vec![
                Span::raw("  Merge "),
                Span::styled(
                    format!("PR #{pr_num}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" ({worker_name})?")),
            ],
        ),
        ConfirmAction::Interrupt { window_name } => (
            " Confirm Interrupt ",
            vec![
                Span::raw("  Send "),
                Span::styled(
                    "Ctrl+C",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" to {window_name}?")),
            ],
        ),
    };

    let mut lines = vec![Line::from(""), Line::from(desc_spans), Line::from("")];

    if has_checkbox {
        let default_branch = app.config.default_branch();
        let checkbox = if fetch_latest { "[x]" } else { "[ ]" };
        lines.push(Line::from(vec![
            Span::raw(format!("  {checkbox} ")),
            Span::styled("Fetch latest", Style::default().fg(Color::Yellow)),
            Span::raw(format!(" (git fetch origin {default_branch})")),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Enter: confirm  Space: toggle  Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            " Enter: confirm  Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, rect);
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

    let title = format!(" {worker_name} │ {pipeline} │ {status} │ {pr} ");
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

fn draw_help_panel(f: &mut Frame, area: Rect, scroll: usize) {
    let width = 68u16.min(area.width.saturating_sub(4));
    let height = area.height.saturating_sub(4);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, rect);

    let help = App::help_lines();
    let body_height = height.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = help
        .iter()
        .skip(scroll)
        .take(body_height.saturating_sub(1))
        .map(|l| {
            if l.starts_with("━") {
                Line::from(Span::styled(
                    *l,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))
            } else if l.starts_with("  :") || l.starts_with("  [") {
                let parts: Vec<&str> = l.splitn(2, "  ").collect();
                if parts.len() == 2 {
                    Line::from(vec![
                        Span::styled(
                            format!("  {}", parts[0].trim()),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!("  {}", parts[1])),
                    ])
                } else {
                    Line::from(Span::raw(*l))
                }
            } else {
                Line::from(Span::raw(*l))
            }
        })
        .collect();

    lines.push(Line::from(Span::styled(
        " [j/k] scroll  [?/Esc] close",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, rect);
}

fn draw_settings_panel(f: &mut Frame, app: &App, area: Rect, selected: usize) {
    let width = 50u16.min(area.width.saturating_sub(4));
    let height = 13u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, rect);

    let items = app.settings_items();
    let mut lines: Vec<Line> = Vec::new();

    for (i, (label, value)) in items.iter().enumerate() {
        let is_sel = i == selected;
        let marker = if is_sel { "▶ " } else { "  " };
        let label_style = if is_sel {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let value_style = if is_sel {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        };

        lines.push(Line::from(vec![
            Span::styled(marker, label_style),
            Span::styled(format!("{label}: "), label_style),
            Span::styled(value.clone(), value_style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter/Space: toggle  j/k: move  Esc: close",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .title(" Settings ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, rect);
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

/// Process badge: shows what's actually running in the pane.
fn process_badge(process: &str) -> &'static str {
    match process {
        "claude" | "node" => " [claude]",
        "bash" | "zsh" | "sh" | "fish" => " [shell]",
        "" => "",
        _ => " [other]",
    }
}

fn status_icon(status: &str) -> String {
    match status {
        "active" => "🟢 working".to_string(),
        "idle" => "🟡 waiting".to_string(),
        "shell" => "🔴 shell exited".to_string(),
        "done" => "✅ complete".to_string(),
        "queued" => "⏳ in queue".to_string(),
        "sleeping" => "💤 rate limited".to_string(),
        "posted" => "✅ commented".to_string(),
        "waiting" => "🔗 waiting on deps".to_string(),
        "no-window" => "👻 orphaned".to_string(),
        "conflict" => "⚠️  merge conflict".to_string(),
        "probing" => "🔍 checking".to_string(),
        "stale" => "💀 stale".to_string(),
        "failed" => "❌ failed".to_string(),
        "needs_approval" => "⚠️  needs approval".to_string(),
        "reviewing" => "📝 under review".to_string(),
        _ => "❓ unknown".to_string(),
    }
}

fn status_style(status: &str) -> Style {
    match status {
        "active" => Style::default().fg(Color::Green),
        "idle" => Style::default().fg(Color::Yellow),
        "shell" => Style::default().fg(Color::Red),
        "conflict" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "needs_approval" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "done" => Style::default().fg(Color::Gray),
        "queued" => Style::default().fg(Color::DarkGray),
        "sleeping" => Style::default().fg(Color::Blue),
        "posted" => Style::default().fg(Color::Cyan),
        "stale" => Style::default().fg(Color::Red),
        "failed" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "reviewing" => Style::default().fg(Color::Magenta),
        "waiting" => Style::default().fg(Color::DarkGray),
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
