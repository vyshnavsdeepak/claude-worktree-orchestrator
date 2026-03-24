use std::sync::atomic::Ordering;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
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
            Constraint::Length(4),
        ])
        .split(area);

    draw_header(f, app, chunks[0]);

    // Split workers: "on PR" = done/posted/shell with a PR (not yet merged)
    let on_pr_workers: Vec<&WorkerState> = app
        .workers
        .iter()
        .filter(|w| is_on_pr(w, &app.merged_prs))
        .collect();

    let on_pr_height = if on_pr_workers.is_empty() {
        0
    } else {
        (on_pr_workers.len() as u16 + 2).min(8)
    };
    let merged_height = if app.merged_prs.is_empty() {
        0
    } else {
        (app.merged_prs.len() as u16 + 2).min(8)
    };
    let queue_height = if app.merge_queue.is_empty() {
        0
    } else {
        (app.merge_queue.len() as u16 + 2).min(10)
    };
    let upcoming_height = if app.upcoming_issues.is_empty() {
        0
    } else {
        (app.upcoming_issues.len() as u16 + 2).min(6)
    };

    if app.show_logs {
        let content = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(on_pr_height),
                Constraint::Length(queue_height),
                Constraint::Length(merged_height),
                Constraint::Length(upcoming_height),
                Constraint::Percentage(35),
            ])
            .split(chunks[1]);
        draw_table(f, app, content[0]);
        if on_pr_height > 0 {
            draw_on_pr(f, &on_pr_workers, content[1]);
        }
        if queue_height > 0 {
            draw_merge_queue(f, app, content[2]);
        }
        if merged_height > 0 {
            draw_merged(f, app, content[3]);
        }
        if upcoming_height > 0 {
            draw_upcoming(f, app, content[4]);
        }
        draw_logs(f, app, content[5]);
    } else {
        let content = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(on_pr_height),
                Constraint::Length(queue_height),
                Constraint::Length(merged_height),
                Constraint::Length(upcoming_height),
            ])
            .split(chunks[1]);
        draw_table(f, app, content[0]);
        if on_pr_height > 0 {
            draw_on_pr(f, &on_pr_workers, content[1]);
        }
        if queue_height > 0 {
            draw_merge_queue(f, app, content[2]);
        }
        if merged_height > 0 {
            draw_merged(f, app, content[3]);
        }
        if upcoming_height > 0 {
            draw_upcoming(f, app, content[4]);
        }
    }

    draw_footer(f, app, chunks[2]);
    draw_toasts(f, app, area);

    if let Mode::Confirm {
        ref action,
        fetch_latest,
        ..
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
    if let Mode::ActionPicker { selected } = app.mode {
        draw_action_picker(f, app, area, selected);
    }
    if let Mode::BranchConflict {
        issue_num,
        selected,
    } = app.mode
    {
        draw_branch_conflict(f, area, issue_num, selected);
    }
    if let Mode::AutopilotConfig { selected } = app.mode {
        draw_autopilot_config(f, app, area, selected);
    }
    if app.mode == Mode::StartupConfirm {
        draw_startup_confirm(f, app, area);
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
                format!("PRs: {} ", app.on_pr_count()),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("│ "),
            Span::styled(
                format!("Queued: {} ", app.queued_count()),
                Style::default().fg(Color::DarkGray),
            ),
            if app.autopilot_enabled {
                Span::styled(
                    format!(
                        "│ AUTOPILOT {}",
                        if app.autopilot_status.is_empty() {
                            "ON"
                        } else {
                            &app.autopilot_status
                        }
                    ),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("")
            },
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
                format!(
                    "Merged: {}",
                    stats.merged_count + app.merged_prs.len() as u64
                ),
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
            if let Some((open, closed)) = app.repo_issue_counts {
                Span::styled(
                    format!(" │ Issues: {open} open / {closed} closed"),
                    Style::default().fg(Color::DarkGray),
                )
            } else {
                Span::raw("")
            },
        ]),
    ];

    let block = Block::default()
        .title(" Claude Worktree Orchestrator ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue));

    let para = Paragraph::new(text).block(block);
    f.render_widget(para, area);
}

/// Worker has a PR and it's NOT yet merged — belongs in "On PR" section
fn is_on_pr(w: &WorkerState, merged_prs: &[(u64, String)]) -> bool {
    if !matches!(w.status.as_str(), "done" | "posted" | "shell") || w.pr.is_none() {
        return false;
    }
    if w.pr_merged {
        return false;
    }
    // Also check the app's merged_prs list (updates faster than poller slow path)
    if let Some(pr_num) = w.pr_num() {
        if merged_prs.iter().any(|(n, _)| *n == pr_num) {
            return false;
        }
    }
    true
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

    // Build rows, keeping track of which original index maps to which display row
    let mut rows: Vec<Row> = Vec::new();
    let mut display_to_orig: Vec<usize> = Vec::new();

    for (i, w) in app.workers.iter().enumerate() {
        // Skip workers that belong in "On PR" or are already merged
        // Skip workers in On PR or already merged
        let is_merged = w.pr_merged
            || w.pr_num()
                .is_some_and(|n| app.merged_prs.iter().any(|(mn, _)| *mn == n));
        if is_on_pr(w, &app.merged_prs) || is_merged {
            continue;
        }

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
        let issue_cell = match &w.issue_title {
            Some(title) => format!("{} {}{} {}", marker, w.window_name, proc_badge, title),
            None => format!("{} {}{}", marker, w.window_name, proc_badge),
        };
        let pipeline_cell = w.pipeline.clone();
        let state_cell = status_icon(&w.status);
        let output_cell = match &w.probe {
            Some(p) if p == "running" => "🔍 probing…".to_string(),
            Some(p) => format!("🔍 {p}"),
            None => w.last_output.clone(),
        };

        rows.push(Row::new(vec![
            Cell::from(issue_cell).style(style),
            Cell::from(pipeline_cell).style(pipeline_style(w).patch(style)),
            Cell::from(state_cell).style(status_style(&w.status).patch(style)),
            Cell::from(output_cell).style(style),
        ]));
        display_to_orig.push(i);
    }

    let display_count = rows.len();
    let visible_height = area.height.saturating_sub(3) as usize;

    // Find which display row corresponds to the selected original index
    let display_selected = display_to_orig
        .iter()
        .position(|&orig| orig == app.selected)
        .unwrap_or(0);
    let scroll_offset = compute_scroll(display_selected, display_count, visible_height);

    let visible_rows: Vec<Row> = rows
        .into_iter()
        .skip(scroll_offset)
        .take(visible_height)
        .collect();

    let working_label = if display_count != app.workers.len() {
        format!(" Working ({display_count})")
    } else {
        format!(" Workers ({display_count})")
    };
    let scroll_hint = if display_count > visible_height {
        format!("{working_label} — {display_count} rows (j/k) ")
    } else {
        format!("{working_label} ")
    };

    let table = Table::new(
        visible_rows,
        [
            Constraint::Percentage(35),
            Constraint::Length(12),
            Constraint::Length(15),
            Constraint::Percentage(30),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(scroll_hint)
            .border_style(Style::default().fg(Color::Blue)),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::BOLD));

    let mut state = TableState::default();
    f.render_stateful_widget(table, area, &mut state);
}

fn draw_on_pr(f: &mut Frame, workers: &[&WorkerState], area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let skip = workers.len().saturating_sub(visible);

    let lines: Vec<Line> = workers
        .iter()
        .skip(skip)
        .map(|w| {
            let pr_str =
                w.pr.as_deref()
                    .map(|p| format!(" PR#{p}"))
                    .unwrap_or_default();
            let merge_state = w.pr_merge_state.as_deref().unwrap_or("");
            let (merge_label, merge_color) = match merge_state {
                "CLEAN" => ("CLEAN", Color::Green),
                "BEHIND" => ("BEHIND", Color::Yellow),
                "BLOCKED" => ("BLOCKED", Color::Red),
                "UNSTABLE" => ("UNSTABLE", Color::Yellow),
                "UNKNOWN" => ("UNKNOWN", Color::DarkGray),
                _ => ("…", Color::DarkGray),
            };
            Line::from(vec![
                Span::styled("  ◦ ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    w.window_name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(pr_str, Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(format!("[{merge_label}]"), Style::default().fg(merge_color)),
                Span::raw(
                    w.issue_title
                        .as_ref()
                        .map(|t| format!(" {t}"))
                        .unwrap_or_default(),
                ),
            ])
        })
        .collect();

    let para = Paragraph::new(lines).block(
        Block::default()
            .title(format!(" On PR ({}) ", workers.len()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(para, area);
}

fn draw_upcoming(f: &mut Frame, app: &App, area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let skip = app.upcoming_issues.len().saturating_sub(visible);

    let lines: Vec<Line> = app
        .upcoming_issues
        .iter()
        .skip(skip)
        .map(|(num, title, priority, complexity, reason)| {
            let complexity_color = match complexity.as_str() {
                "small" => Color::Green,
                "medium" => Color::Yellow,
                "large" => Color::Red,
                _ => Color::DarkGray,
            };
            let pri_str = if priority.is_empty() {
                String::new()
            } else {
                format!("p{priority}")
            };
            let cplx_str = if complexity.is_empty() {
                String::new()
            } else {
                complexity
                    .chars()
                    .next()
                    .unwrap_or(' ')
                    .to_uppercase()
                    .to_string()
            };
            let reason_str = if reason.is_empty() {
                String::new()
            } else {
                format!(" — {reason}")
            };
            let mut spans = vec![Span::raw("  ".to_string())];
            if !pri_str.is_empty() || !cplx_str.is_empty() {
                spans.push(Span::styled(
                    format!("{pri_str} "),
                    Style::default().fg(Color::DarkGray),
                ));
                spans.push(Span::styled(
                    format!("[{cplx_str}] "),
                    Style::default().fg(complexity_color),
                ));
            }
            spans.push(Span::styled(
                format!("#{num}"),
                Style::default().fg(Color::Cyan),
            ));
            spans.push(Span::raw(format!(" {title}")));
            if !reason_str.is_empty() {
                spans.push(Span::styled(
                    reason_str,
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        })
        .collect();

    let para = Paragraph::new(lines).block(
        Block::default()
            .title(format!(" Upcoming ({}) ", app.upcoming_issues.len()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(para, area);
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

    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Log ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn draw_merged(f: &mut Frame, app: &App, area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let skip = app.merged_prs.len().saturating_sub(visible);

    let lines: Vec<Line> = app
        .merged_prs
        .iter()
        .skip(skip)
        .map(|(pr, title)| {
            Line::from(vec![
                Span::styled("  ✓ ", Style::default().fg(Color::Green)),
                Span::styled(format!("#{pr}"), Style::default().fg(Color::Cyan)),
                Span::raw(format!(" {title}")),
            ])
        })
        .collect();

    let para = Paragraph::new(lines).block(
        Block::default()
            .title(format!(" Merged ({}) ", app.merged_prs.len()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );
    f.render_widget(para, area);
}

fn draw_merge_queue(f: &mut Frame, app: &App, area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let skip = app.merge_queue.len().saturating_sub(visible);

    let lines: Vec<Line> = app
        .merge_queue
        .iter()
        .skip(skip)
        .map(|(pr, title, status)| {
            let (icon, color) = match status.as_str() {
                "queued" => ("◦", Color::DarkGray),
                "checking" => ("⟳", Color::Yellow),
                "merging" => ("▸", Color::Green),
                "conflicts → resolving" => ("⚠", Color::Red),
                "behind → updating" => ("↓", Color::Yellow),
                "unknown → trying" => ("?", Color::Yellow),
                _ => ("·", Color::DarkGray),
            };
            Line::from(vec![
                Span::styled(format!("  {icon} "), Style::default().fg(color)),
                Span::styled(format!("#{pr}"), Style::default().fg(Color::Cyan)),
                Span::raw(format!(" {title} ")),
                Span::styled(format!("[{status}]"), Style::default().fg(color)),
            ])
        })
        .collect();

    let para = Paragraph::new(lines).block(
        Block::default()
            .title(format!(" Merge Queue ({}) ", app.merge_queue.len()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
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
                    if app.plan_mode_pending {
                        "Plan Mode — issue #".to_string()
                    } else {
                        "New Job — issue #".to_string()
                    },
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
                        ConfirmAction::CloseWorker { window_name, .. } => {
                            format!("Confirm — close {window_name}")
                        }
                        ConfirmAction::CloseFinished { workers } => {
                            format!("Confirm — close {} finished workers", workers.len())
                        }
                        ConfirmAction::RunAction { ref name, .. } => {
                            format!("Confirm — run {name}")
                        }
                        ConfirmAction::QuitClean => "Quit".to_string(),
                    };
                    let hint = if matches!(action, ConfirmAction::QuitClean) {
                        " Enter: quit  a: tear down all  Esc: cancel".to_string()
                    } else {
                        " Enter: confirm  Esc: cancel".to_string()
                    };
                    (desc, hint)
                }
                Mode::Detail { .. } => {
                    ("Detail".to_string(), " j/k scroll · Esc close".to_string())
                }
                Mode::Settings { .. } => (
                    "Settings".to_string(),
                    " j/k move · Enter toggle · Esc close".to_string(),
                ),
                Mode::Help { .. } => ("Help".to_string(), " j/k scroll · ?/Esc close".to_string()),
                Mode::ActionPicker { .. } => (
                    "Actions".to_string(),
                    " j/k move · Enter select · Esc close".to_string(),
                ),
                Mode::BranchConflict { issue_num, .. } => (
                    format!("Branch Conflict — #{issue_num}"),
                    " j/k move · Enter confirm · Esc skip".to_string(),
                ),
                Mode::AutopilotConfig { .. } => (
                    "Autopilot Config".to_string(),
                    " j/k move · Enter/Space toggle · Esc close".to_string(),
                ),
                Mode::StartupConfirm => (
                    "Startup Issues".to_string(),
                    " ↑↓ move · Space toggle · Enter launch · Esc skip".to_string(),
                ),
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

    // Row 1: worker actions + launch
    let mut row1: Vec<Span> = Vec::new();
    row1.push(Span::raw(" "));

    let worker_keys: &[(&str, &str)] = &[
        ("s", "send"),
        ("t", "tmux"),
        ("i", "int"),
        ("x", "close"),
        ("b", "bcast"),
        ("m", "merge"),
    ];
    for (i, (k, d)) in worker_keys.iter().enumerate() {
        if i > 0 {
            row1.push(Span::raw(" "));
        }
        row1.extend(footer_key(k, d));
    }

    row1.push(sep.clone());

    let launch_keys: &[(&str, &str)] = &[
        ("p", "prompt"),
        ("P", "direct"),
        ("n", "job"),
        ("N", "plan"),
    ];
    for (i, (k, d)) in launch_keys.iter().enumerate() {
        if i > 0 {
            row1.push(Span::raw(" "));
        }
        row1.extend(footer_key(k, d));
    }

    // Row 2: view + quit
    let mut row2: Vec<Span> = Vec::new();
    row2.push(Span::raw(" "));

    let view_keys: &[(&str, &str)] = &[
        ("a", "action"),
        ("A", "autopilot"),
        ("U", "update"),
        ("d", "detail"),
        ("v", "pr"),
        ("l", "log"),
        ("c", "cfg"),
        ("?", "help"),
        ("q", "quit"),
    ];
    for (i, (k, d)) in view_keys.iter().enumerate() {
        if i > 0 {
            row2.push(Span::raw(" "));
        }
        row2.extend(footer_key(k, d));
    }

    if !app.status_msg.is_empty() {
        row2.push(sep);
        row2.push(Span::styled(
            app.status_msg.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }

    let lines = vec![Line::from(row1), Line::from(row2)];

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
    let width = 60u16.min(area.width.saturating_sub(4));
    let has_checkbox = matches!(action, ConfirmAction::LaunchIssue { .. });
    let has_branch = app.branch_input.is_some();
    let has_base = app.base_branch_input.is_some();
    let is_quit = matches!(action, ConfirmAction::QuitClean);
    let height = if has_branch && has_base {
        12u16
    } else if has_branch {
        10u16
    } else if has_checkbox || is_quit {
        8u16
    } else {
        6u16
    };
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
        ConfirmAction::CloseWorker { window_name, .. } => (
            " Confirm Close ",
            vec![
                Span::raw("  Close "),
                Span::styled(
                    window_name.clone(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("? (kill window + remove worktree)"),
            ],
        ),
        ConfirmAction::CloseFinished { workers } => (
            " Confirm Close Finished ",
            vec![
                Span::raw("  Close "),
                Span::styled(
                    format!("{}", workers.len()),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" finished workers? (done/shell/failed)"),
            ],
        ),
        ConfirmAction::RunAction {
            ref name,
            ref command,
        } => {
            let cmd_preview: String = command.chars().take(40).collect();
            (
                " Confirm Action ",
                vec![
                    Span::raw("  Run "),
                    Span::styled(
                        name.clone(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("? ({cmd_preview})")),
                ],
            )
        }
        ConfirmAction::QuitClean => (
            " Quit ",
            vec![
                Span::raw("  "),
                Span::styled(
                    "Enter",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(": quit (workers keep running)"),
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
        if let Some(ref branch) = app.branch_input {
            let max_len = (width as usize).saturating_sub(14); // "  Branch: " + padding
            let display: String = if branch.len() > max_len {
                format!("{}...", &branch[..max_len.saturating_sub(3)])
            } else {
                branch.clone()
            };
            let loading = if app.branch_loading {
                " (loading...)"
            } else {
                ""
            };
            let branch_style = if app.branch_focused {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default().fg(Color::Cyan)
            };
            lines.push(Line::from(vec![
                Span::raw("  Branch: "),
                Span::styled(display, branch_style),
                Span::styled(loading, Style::default().fg(Color::DarkGray)),
            ]));
        }
        if let Some(ref base) = app.base_branch_input {
            let max_len = (width as usize).saturating_sub(12);
            let display: String = if base.len() > max_len {
                format!("{}...", &base[..max_len.saturating_sub(3)])
            } else {
                base.clone()
            };
            let base_style = if app.base_branch_focused {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default().fg(Color::Yellow)
            };
            lines.push(Line::from(vec![
                Span::raw("  Base:   "),
                Span::styled(display, base_style),
            ]));
        }
        lines.push(Line::from(""));
        let hint = if has_branch {
            " Enter: confirm  Space: toggle  Tab: branch/base  Esc: cancel"
        } else {
            " Enter: confirm  Space: toggle  Esc: cancel"
        };
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )));
    } else if is_quit {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "a",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(": quit + kill session + remove worktrees"),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Enter: quit only  a: tear down all  Esc: cancel",
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

fn draw_action_picker(f: &mut Frame, app: &App, area: Rect, selected: usize) {
    let width = 50u16.min(area.width.saturating_sub(4));
    let item_count = app.config.actions.len() as u16;
    let height = (item_count + 4).min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, rect);

    let mut lines: Vec<Line> = Vec::new();
    for (i, action) in app.config.actions.iter().enumerate() {
        let is_sel = i == selected;
        let marker = if is_sel { "▶ " } else { "  " };
        let style = if is_sel {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}{}", action.name),
            style,
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter: select  j/k: move  Esc: close",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .title(" Actions ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

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

fn draw_autopilot_config(f: &mut Frame, app: &App, area: Rect, selected: usize) {
    let width = 54u16.min(area.width.saturating_sub(4));
    let height = 12u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, rect);

    let items = app.autopilot_config_items();
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
        let value_style = if i == 0 {
            // Autopilot toggle — color based on state
            if value == "ON" {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Red)
            }
        } else if is_sel {
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

    if !app.autopilot_status.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  Status: "),
            Span::styled(
                app.autopilot_status.clone(),
                Style::default().fg(Color::Magenta),
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter/Space: toggle  j/k: move  Esc: close",
        Style::default().fg(Color::DarkGray),
    )));

    let title = if app.autopilot_enabled {
        " Autopilot [ON] "
    } else {
        " Autopilot [OFF] "
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));

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

fn draw_branch_conflict(f: &mut Frame, area: Rect, issue_num: u64, selected: usize) {
    let width = 52u16.min(area.width.saturating_sub(4));
    let height = 10u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, rect);

    let options = ["Reuse existing branch", "Reset (delete + recreate)", "Skip"];

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Branch for "),
            Span::styled(
                format!("#{issue_num}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" already exists."),
        ]),
        Line::from(""),
    ];

    for (i, label) in options.iter().enumerate() {
        let marker = if i == selected { " > " } else { "   " };
        let style = if i == selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(format!("{marker}{label}"), style)));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter: confirm  j/k: move  Esc: skip",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .title(" Branch Conflict ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, rect);
}

fn draw_startup_confirm(f: &mut Frame, app: &App, area: Rect) {
    let count = app.startup_pending.len();
    let inner_height = (count as u16 + 2).max(3);
    let height = (inner_height + 6).min(area.height.saturating_sub(4));
    let width = 62u16.min(area.width.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, rect);

    let mut lines: Vec<Line> = vec![Line::from("")];

    for (i, (issue_num, selected, state)) in app.startup_pending.iter().enumerate() {
        let marker = if i == app.startup_selected {
            "▶ "
        } else {
            "  "
        };
        let check = if *selected { "[x]" } else { "[ ]" };
        let state_str = match state.as_deref() {
            Some("closed") => " [closed]",
            Some("merged") => " [merged]",
            _ => "",
        };
        let item_style = if i == app.startup_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::raw(format!("{marker}{check} ")),
            Span::styled(format!("#{issue_num}"), item_style),
            Span::styled(state_str, Style::default().fg(Color::DarkGray)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " g: check GitHub state  p: check merged PR",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        " ↑↓: select  Space: toggle  Enter: launch  Esc: skip",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .title(" Issues from config ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, rect);
}
