use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::events::{EventLog, EventStats};
use crate::poller::WorkerState;
use crate::state::StateDir;

const LOG_CAP: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Send,
    Broadcast,
    Command,
    Detail {
        scroll: usize,
    },
    Prompt,
    DirectPrompt,
    NewJob,
    Confirm {
        action: ConfirmAction,
        fetch_latest: bool,
    },
    Settings {
        selected: usize,
    },
    Help {
        scroll: usize,
    },
    ActionPicker {
        selected: usize,
    },
    BranchConflict {
        issue_num: u64,
        selected: usize,
    },
    AutopilotConfig {
        selected: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmAction {
    LaunchIssue {
        issue_num: u64,
    },
    MergeAll,
    MergePr {
        pr_num: u64,
        worker_name: String,
    },
    Interrupt {
        window_name: String,
    },
    CloseWorker {
        window_name: String,
        window_index: usize,
        worktree: String,
    },
    CloseFinished {
        workers: Vec<(String, usize, String)>,
    },
    QuitClean,
    RunAction {
        name: String,
        command: String,
    },
}

#[derive(Clone, Debug)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub message: String,
    pub level: ToastLevel,
    pub expires_at: Instant,
}

pub struct App {
    pub config: Arc<Config>,
    pub state_dir: Arc<StateDir>,
    pub workers: Vec<WorkerState>,
    pub selected: usize,
    pub mode: Mode,
    pub input: String,
    pub status_msg: String,
    pub last_refresh: Instant,
    pub logs: VecDeque<String>,
    pub show_logs: bool,
    pub next_scan_at: Option<Instant>,
    pub toasts: Vec<Toast>,
    pub frame: u64,
    pub is_polling: Arc<AtomicBool>,
    pub detail_content: Vec<String>,
    prev_worker_states: HashMap<String, String>,
    rx: watch::Receiver<Vec<WorkerState>>,
    log_rx: Option<mpsc::UnboundedReceiver<String>>,
    cmd_tx: Option<mpsc::UnboundedSender<String>>,
    prompt_tx: Option<mpsc::UnboundedSender<String>>,
    log_tx: mpsc::UnboundedSender<String>,
    pub event_log: EventLog,
    input_histories: HashMap<String, Vec<String>>,
    history_idx: Option<usize>,
    input_saved: String,
    // Branch editing state for LaunchIssue confirm dialog
    pub branch_input: Option<String>,
    pub branch_loading: bool,
    pub branch_focused: bool,
    branch_edited: bool,
    // Autopilot state
    pub autopilot_enabled: bool,
    pub autopilot_status: String,
    autopilot_tx: Option<watch::Sender<bool>>,
    pub merged_prs: Vec<(u64, String)>, // (pr_num, title)
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<Config>,
        state_dir: Arc<StateDir>,
        rx: watch::Receiver<Vec<WorkerState>>,
        log_rx: Option<mpsc::UnboundedReceiver<String>>,
        is_polling: Arc<AtomicBool>,
        cmd_tx: Option<mpsc::UnboundedSender<String>>,
        prompt_tx: Option<mpsc::UnboundedSender<String>>,
        log_tx: mpsc::UnboundedSender<String>,
        event_log: EventLog,
        autopilot_tx: Option<watch::Sender<bool>>,
    ) -> Self {
        let input_histories = load_history(&state_dir);
        let autopilot_enabled = config.autopilot;
        Self {
            config,
            state_dir,
            workers: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            status_msg: String::new(),
            last_refresh: Instant::now(),
            logs: VecDeque::with_capacity(LOG_CAP),
            show_logs: false,
            next_scan_at: None,
            toasts: Vec::new(),
            frame: 0,
            is_polling,
            detail_content: Vec::new(),
            prev_worker_states: HashMap::new(),
            rx,
            log_rx,
            cmd_tx,
            prompt_tx,
            log_tx,
            event_log,
            input_histories,
            history_idx: None,
            input_saved: String::new(),
            branch_input: None,
            branch_loading: false,
            branch_focused: false,
            branch_edited: false,
            autopilot_enabled,
            autopilot_status: String::new(),
            autopilot_tx,
            merged_prs: Vec::new(),
        }
    }

    pub fn event_stats(&self) -> EventStats {
        self.event_log.stats()
    }

    fn push_log(&mut self, msg: &str) {
        if self.logs.len() >= LOG_CAP {
            self.logs.pop_front();
        }
        self.logs.push_back(msg.to_string());
    }

    pub fn push_toast(&mut self, msg: &str, level: ToastLevel) {
        let duration = match level {
            ToastLevel::Info | ToastLevel::Success => Duration::from_secs(4),
            ToastLevel::Warning => Duration::from_secs(6),
            ToastLevel::Error => Duration::from_secs(8),
        };
        self.toasts.push(Toast {
            message: msg.to_string(),
            level,
            expires_at: Instant::now() + duration,
        });
        if self.toasts.len() > 10 {
            self.toasts.remove(0);
        }
    }

    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);

        let now = Instant::now();
        self.toasts.retain(|t| t.expires_at > now);

        if self.rx.has_changed().unwrap_or(false) {
            let new_workers = self.rx.borrow_and_update().clone();

            for w in &new_workers {
                if let Some(prev_status) = self.prev_worker_states.get(&w.window_name) {
                    if prev_status != &w.status {
                        let toast = match (prev_status.as_str(), w.status.as_str()) {
                            (prev, "active") if prev != "active" => Some((
                                format!("{} started working", w.window_name),
                                ToastLevel::Info,
                            )),
                            ("active", "done") => {
                                Some((format!("{} has a PR!", w.window_name), ToastLevel::Success))
                            }
                            ("shell", "idle") => Some((
                                format!("{} Claude relaunched", w.window_name),
                                ToastLevel::Info,
                            )),
                            (_, "no-window") => Some((
                                format!("{} window lost", w.window_name),
                                ToastLevel::Warning,
                            )),
                            _ => None,
                        };
                        if let Some((msg, level)) = toast {
                            self.push_toast(&msg, level);
                        }
                    }
                }
            }

            self.prev_worker_states.clear();
            for w in &new_workers {
                self.prev_worker_states
                    .insert(w.window_name.clone(), w.status.clone());
            }

            self.workers = new_workers;
            self.last_refresh = Instant::now();

            if !self.workers.is_empty() && self.selected >= self.workers.len() {
                self.selected = self.workers.len() - 1;
            }
        }

        let messages: Vec<String> = if let Some(rx) = &mut self.log_rx {
            let mut buf = Vec::new();
            while let Ok(msg) = rx.try_recv() {
                buf.push(msg);
            }
            buf
        } else {
            Vec::new()
        };

        for msg in messages {
            if let Some(rest) = msg.strip_prefix("__NEXT_SCAN_") {
                if let Some(secs_str) = rest.strip_suffix("__") {
                    if let Ok(secs) = secs_str.parse::<u64>() {
                        self.next_scan_at = Some(Instant::now() + Duration::from_secs(secs));
                    }
                }
            } else if let Some(rest) = msg.strip_prefix("__ISSUE_TITLE_DONE_") {
                // Title fetch failed — just stop loading indicator
                if let Some(num_str) = rest.strip_suffix("__") {
                    if let Ok(issue_num) = num_str.parse::<u64>() {
                        if let Mode::Confirm {
                            action: ConfirmAction::LaunchIssue { issue_num: n },
                            ..
                        } = &self.mode
                        {
                            if *n == issue_num {
                                self.branch_loading = false;
                            }
                        }
                    }
                }
            } else if let Some(rest) = msg.strip_prefix("__ISSUE_TITLE_") {
                // Parse __ISSUE_TITLE_{num}_{title}__
                if let Some(body) = rest.strip_suffix("__") {
                    if let Some(sep) = body.find('_') {
                        let num_str = &body[..sep];
                        let title = &body[sep + 1..];
                        if let Ok(issue_num) = num_str.parse::<u64>() {
                            if let Mode::Confirm {
                                action: ConfirmAction::LaunchIssue { issue_num: n },
                                ..
                            } = &self.mode
                            {
                                if *n == issue_num && !self.branch_edited {
                                    self.branch_input =
                                        Some(self.config.branch_name_with_title(issue_num, title));
                                }
                                if *n == issue_num {
                                    self.branch_loading = false;
                                }
                            }
                        }
                    }
                }
            } else if let Some(rest) = msg.strip_prefix("__BRANCH_CONFLICT_") {
                if let Some(num_str) = rest.strip_suffix("__") {
                    if let Ok(issue_num) = num_str.parse::<u64>() {
                        self.mode = Mode::BranchConflict {
                            issue_num,
                            selected: 0,
                        };
                    }
                }
            } else if let Some(rest) = msg.strip_prefix("__AUTOPILOT_STATUS_") {
                if let Some(status) = rest.strip_suffix("__") {
                    self.autopilot_status = status.to_string();
                }
            } else if let Some(rest) = msg.strip_prefix("__AUTOPILOT_MERGED_") {
                if let Some(body) = rest.strip_suffix("__") {
                    // Format: "pr_num\ttitle"
                    let mut parts = body.splitn(2, '\t');
                    if let (Some(num_str), Some(title)) = (parts.next(), parts.next()) {
                        if let Ok(pr_num) = num_str.parse::<u64>() {
                            if !self.merged_prs.iter().any(|(n, _)| *n == pr_num) {
                                self.merged_prs.push((pr_num, title.to_string()));
                            }
                        }
                    }
                }
            } else if let Some(rest) = msg.strip_prefix("__TOAST_") {
                if let Some(body) = rest.strip_suffix("__") {
                    let parsed: Option<(ToastLevel, String)> = body
                        .strip_prefix("INFO_")
                        .map(|m| (ToastLevel::Info, m.to_string()))
                        .or_else(|| {
                            body.strip_prefix("SUCCESS_")
                                .map(|m| (ToastLevel::Success, m.to_string()))
                        })
                        .or_else(|| {
                            body.strip_prefix("WARNING_")
                                .map(|m| (ToastLevel::Warning, m.to_string()))
                        })
                        .or_else(|| {
                            body.strip_prefix("ERROR_")
                                .map(|m| (ToastLevel::Error, m.to_string()))
                        });
                    if let Some((level, message)) = parsed {
                        self.push_toast(&message, level);
                    } else {
                        if self.logs.len() >= LOG_CAP {
                            self.logs.pop_front();
                        }
                        self.logs.push_back(msg);
                    }
                }
            } else {
                if self.logs.len() >= LOG_CAP {
                    self.logs.pop_front();
                }
                self.logs.push_back(msg);
            }
        }
    }

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        match &self.mode {
            Mode::Normal => self.handle_normal_key(code, modifiers),
            Mode::Send
            | Mode::Broadcast
            | Mode::Command
            | Mode::Prompt
            | Mode::DirectPrompt
            | Mode::NewJob => self.handle_input_key(code),
            Mode::Confirm { .. } => self.handle_confirm_key(code),
            Mode::Detail { .. } => self.handle_detail_key(code),
            Mode::Settings { .. } => self.handle_settings_key(code),
            Mode::Help { .. } => self.handle_help_key(code),
            Mode::ActionPicker { .. } => self.handle_action_picker_key(code),
            Mode::BranchConflict { .. } => self.handle_branch_conflict_key(code),
            Mode::AutopilotConfig { .. } => self.handle_autopilot_config_key(code),
        }
    }

    fn handle_normal_key(&mut self, code: KeyCode, _modifiers: KeyModifiers) -> bool {
        match code {
            KeyCode::Char('q') => {
                self.mode = Mode::Confirm {
                    action: ConfirmAction::QuitClean,
                    fetch_latest: false,
                };
            }
            KeyCode::Esc => return true,
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_prev(),
            KeyCode::Char('s') => {
                if !self.workers.is_empty() {
                    self.mode = Mode::Send;
                    self.input.clear();
                    self.status_msg =
                        "Send prompt to selected worker (Enter to send, Esc to cancel)".into();
                }
            }
            KeyCode::Char('i') => {
                if let Some(w) = self.workers.get(self.selected) {
                    let name = w.window_name.clone();
                    self.mode = Mode::Confirm {
                        action: ConfirmAction::Interrupt { window_name: name },
                        fetch_latest: false,
                    };
                }
            }
            KeyCode::Char('b') => {
                self.mode = Mode::Broadcast;
                self.input.clear();
                self.status_msg =
                    "Broadcast to all idle workers (Enter to send, Esc to cancel)".into();
            }
            KeyCode::Char('r') => {
                self.status_msg = "Refreshing…".into();
            }
            KeyCode::Char('l') => {
                self.show_logs = !self.show_logs;
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.input.clear();
                self.status_msg = "Builder command (Enter to send, Esc to cancel)".into();
            }
            KeyCode::Char('d') | KeyCode::Enter => {
                self.detail_content = self.capture_detail_content();
                self.mode = Mode::Detail { scroll: 0 };
            }
            KeyCode::Char('p') => {
                self.mode = Mode::Prompt;
                self.input.clear();
                self.status_msg = "Free-form prompt — Claude extracts & spins up tasks".into();
            }
            KeyCode::Char('P') => {
                self.mode = Mode::DirectPrompt;
                self.input.clear();
                self.status_msg =
                    "Direct prompt — launches a worker immediately (no GitHub issue)".into();
            }
            KeyCode::Char('n') => {
                self.mode = Mode::NewJob;
                self.input.clear();
                self.status_msg = "Enter issue number to spin up a worker".into();
            }
            KeyCode::Char('v') => {
                if let Some(w) = self.workers.get(self.selected) {
                    if let Some(pr) = &w.pr {
                        let pr_num = pr.trim_start_matches('#');
                        let url = format!("https://github.com/{}/pull/{pr_num}", self.config.repo);
                        let _ = std::process::Command::new("open").arg(&url).spawn();
                        self.status_msg = format!("Opening {url}");
                    } else {
                        self.status_msg = "No PR for selected worker".into();
                    }
                }
            }
            KeyCode::Char('t') => {
                if let Some(w) = self.workers.get(self.selected) {
                    if w.window_index != usize::MAX {
                        let target = format!("{}:{}", self.config.session, w.window_index);
                        let _ = std::process::Command::new(&self.config.tmux)
                            .args(["select-window", "-t", &target])
                            .output();
                        self.status_msg = format!("Switched to {}", w.window_name);
                    } else {
                        self.push_toast("No tmux window for this worker", ToastLevel::Warning);
                    }
                }
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help { scroll: 0 };
            }
            KeyCode::Char('c') => {
                self.mode = Mode::Settings { selected: 0 };
                self.status_msg = "Settings — j/k navigate, Enter/Space toggle, Esc close".into();
            }
            KeyCode::Char('x') => {
                if let Some(w) = self.workers.get(self.selected) {
                    let name = w.window_name.clone();
                    let idx = w.window_index;
                    let worktree = w
                        .window_name
                        .strip_prefix(self.config.window_prefix.as_str())
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(|n| self.config.worktree_path(n))
                        .unwrap_or_default();
                    self.mode = Mode::Confirm {
                        action: ConfirmAction::CloseWorker {
                            window_name: name,
                            window_index: idx,
                            worktree,
                        },
                        fetch_latest: false,
                    };
                }
            }
            KeyCode::Char('X') => {
                let finished: Vec<(String, usize, String)> = self
                    .workers
                    .iter()
                    .filter(|w| matches!(w.status.as_str(), "done" | "shell" | "failed"))
                    .map(|w| {
                        let worktree = w
                            .window_name
                            .strip_prefix(self.config.window_prefix.as_str())
                            .and_then(|s| s.parse::<u64>().ok())
                            .map(|n| self.config.worktree_path(n))
                            .unwrap_or_default();
                        (w.window_name.clone(), w.window_index, worktree)
                    })
                    .collect();
                if finished.is_empty() {
                    self.push_toast("No finished workers to close", ToastLevel::Info);
                } else {
                    self.mode = Mode::Confirm {
                        action: ConfirmAction::CloseFinished { workers: finished },
                        fetch_latest: false,
                    };
                }
            }
            KeyCode::Char('m') => {
                self.mode = Mode::Confirm {
                    action: ConfirmAction::MergeAll,
                    fetch_latest: false,
                };
            }
            KeyCode::Char('a') => {
                if self.config.actions.is_empty() {
                    self.push_toast("No actions configured", ToastLevel::Info);
                } else {
                    self.mode = Mode::ActionPicker { selected: 0 };
                }
            }
            KeyCode::Char('A') => {
                self.mode = Mode::AutopilotConfig { selected: 0 };
            }
            KeyCode::Char('M') => {
                if let Some(w) = self.workers.get(self.selected) {
                    if let Some(pr) = &w.pr {
                        let pr_num_str = pr.trim_start_matches('#').to_string();
                        if let Ok(pr_num) = pr_num_str.parse::<u64>() {
                            self.mode = Mode::Confirm {
                                action: ConfirmAction::MergePr {
                                    pr_num,
                                    worker_name: w.window_name.clone(),
                                },
                                fetch_latest: false,
                            };
                        }
                    } else {
                        self.status_msg = "No PR for selected worker".into();
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn handle_detail_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                if let Mode::Detail { scroll } = &mut self.mode {
                    *scroll = scroll.saturating_add(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Mode::Detail { scroll } = &mut self.mode {
                    *scroll = scroll.saturating_sub(1);
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
            }
            _ => {}
        }
        false
    }

    fn capture_detail_content(&self) -> Vec<String> {
        let Some(w) = self.workers.get(self.selected) else {
            return vec!["No worker selected".to_string()];
        };

        if w.window_index == usize::MAX {
            let worktree = self.config.worktree_path(
                w.window_name
                    .strip_prefix(self.config.window_prefix.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
            );
            let mut lines = vec![
                format!("Branch: {}", w.branch_name),
                format!("Worktree: {worktree}"),
                format!("Pipeline: {}", w.pipeline),
                String::new(),
                "--- git log ---".to_string(),
            ];
            let out = std::process::Command::new("git")
                .args(["-C", &worktree, "log", "--oneline", "-20"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_else(|| "(no git log available)\n".to_string());
            lines.extend(out.lines().map(|l| l.to_string()));
            lines
        } else {
            let target = format!("{}:{}", self.config.session, w.window_index);
            let out = std::process::Command::new(&self.config.tmux)
                .args(["capture-pane", "-t", &target, "-p", "-S", "-30"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_else(|| "(failed to capture pane)\n".to_string());
            let mut lines: Vec<String> = out.lines().map(|l| l.to_string()).collect();

            // Append review notes if available for this worker
            if let Some(issue_num) = w
                .window_name
                .strip_prefix(self.config.window_prefix.as_str())
                .and_then(|s| s.parse::<u64>().ok())
            {
                let review_file = self.state_dir.review_file(issue_num);
                if let Ok(notes) = std::fs::read_to_string(&review_file) {
                    lines.push(String::new());
                    lines.push("--- Review Notes ---".to_string());
                    lines.extend(notes.lines().map(|l| l.to_string()));
                }
            }
            lines
        }
    }

    fn handle_confirm_key(&mut self, code: KeyCode) -> bool {
        let (action, fetch_latest) = match &self.mode {
            Mode::Confirm {
                action,
                fetch_latest,
            } => (action.clone(), *fetch_latest),
            _ => return false,
        };

        // QuitClean has its own key handling: Enter = quit only, 'a' = quit + tear down
        if matches!(action, ConfirmAction::QuitClean) {
            match code {
                KeyCode::Enter => {
                    return true; // quit TUI only
                }
                KeyCode::Char('a') => {
                    self.execute_confirmed_action(action, fetch_latest);
                    return true; // quit after teardown
                }
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.status_msg = "Cancelled".into();
                }
                _ => {}
            }
            return false;
        }

        // Branch field editing when focused
        if self.branch_focused {
            match code {
                KeyCode::Tab | KeyCode::Enter => {
                    self.branch_focused = false;
                }
                KeyCode::Esc => {
                    self.branch_focused = false;
                }
                KeyCode::Backspace => {
                    if let Some(ref mut b) = self.branch_input {
                        b.pop();
                        self.branch_edited = true;
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(ref mut b) = self.branch_input {
                        b.push(c);
                        self.branch_edited = true;
                    }
                }
                _ => {}
            }
            return false;
        }

        match code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.execute_confirmed_action(action, fetch_latest);
                self.mode = Mode::Normal;
                self.branch_input = None;
                self.branch_loading = false;
                self.branch_focused = false;
                self.branch_edited = false;
            }
            KeyCode::Char(' ') => {
                if matches!(action, ConfirmAction::LaunchIssue { .. }) {
                    self.mode = Mode::Confirm {
                        action,
                        fetch_latest: !fetch_latest,
                    };
                }
            }
            KeyCode::Tab => {
                if self.branch_input.is_some() {
                    self.branch_focused = true;
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
                self.status_msg = "Cancelled".into();
                self.branch_input = None;
                self.branch_loading = false;
                self.branch_focused = false;
                self.branch_edited = false;
            }
            _ => {}
        }
        false
    }

    fn execute_confirmed_action(&mut self, action: ConfirmAction, fetch_latest: bool) {
        match action {
            ConfirmAction::LaunchIssue { issue_num } => {
                if fetch_latest {
                    let config = Arc::clone(&self.config);
                    let log_tx = self.log_tx.clone();
                    tokio::spawn(async move {
                        let branch = config.default_branch();
                        let _ = log_tx.send(format!("[n] Fetching latest {branch} from origin..."));
                        let out = tokio::process::Command::new("git")
                            .args(["-C", &config.repo_root, "fetch", "origin", &branch])
                            .output()
                            .await;
                        match out {
                            Ok(o) if o.status.success() => {
                                let _ = log_tx.send(format!("[n] Fetched latest {branch}"));
                            }
                            Ok(o) => {
                                let stderr = String::from_utf8_lossy(&o.stderr);
                                let _ = log_tx.send(format!("[n] Fetch warning: {stderr}"));
                            }
                            Err(e) => {
                                let _ = log_tx.send(format!("[n] Fetch error: {e}"));
                            }
                        }
                    });
                }
                self.confirm_new_job(issue_num);
            }
            ConfirmAction::MergeAll => {
                if let Some(tx) = &self.cmd_tx {
                    let _ = tx.send("merge all".to_string());
                } else {
                    let c = Arc::clone(&self.config);
                    let lt = self.log_tx.clone();
                    let el = self.event_log.clone();
                    let sd = Arc::clone(&self.state_dir);
                    tokio::spawn(async move {
                        crate::monitor::check_and_merge_open_prs(&c, &lt, &el, &sd).await;
                    });
                }
                self.status_msg = "Merging open PRs...".into();
            }
            ConfirmAction::MergePr {
                pr_num,
                worker_name,
            } => {
                if let Some(tx) = &self.cmd_tx {
                    let _ = tx.send(format!("merge pr {pr_num}"));
                } else {
                    let repo = self.config.repo.clone();
                    let lt = self.log_tx.clone();
                    tokio::spawn(async move {
                        match crate::github::merge_pr(&repo, pr_num).await {
                            Ok(()) => {
                                let _ = lt.send(format!("[merge] PR #{pr_num} merged"));
                                let _ = lt.send(format!("__TOAST_SUCCESS_Merged PR #{pr_num}!__"));
                            }
                            Err(e) => {
                                let _ = lt.send(format!("[merge] PR #{pr_num} merge failed: {e}"));
                                let _ =
                                    lt.send(format!("__TOAST_ERROR_PR #{pr_num} merge failed__"));
                            }
                        }
                    });
                }
                self.status_msg = format!("Merging {worker_name} PR #{pr_num}...");
            }
            ConfirmAction::Interrupt { window_name } => {
                self.do_interrupt(&window_name);
            }
            ConfirmAction::CloseWorker {
                window_name,
                window_index,
                worktree,
            } => {
                self.status_msg = format!("Closing {window_name}...");
                let config = Arc::clone(&self.config);
                let log_tx = self.log_tx.clone();
                tokio::spawn(async move {
                    close_worker(&config, &log_tx, &window_name, window_index, &worktree).await;
                });
            }
            ConfirmAction::CloseFinished { workers } => {
                let config = Arc::clone(&self.config);
                let log_tx = self.log_tx.clone();
                let count = workers.len();
                tokio::spawn(async move {
                    for (name, idx, worktree) in &workers {
                        close_worker(&config, &log_tx, name, *idx, worktree).await;
                    }
                    let _ =
                        log_tx.send(format!("__TOAST_SUCCESS_Closed {count} finished workers__"));
                });
                self.status_msg = format!("Closing {count} finished workers...");
            }
            ConfirmAction::RunAction { name, command } => {
                self.run_action_command(&name, &command);
                self.status_msg = format!("Running: {name}");
            }
            ConfirmAction::QuitClean => {
                let config = Arc::clone(&self.config);
                let log_tx = self.log_tx.clone();
                let workers: Vec<(String, usize, String)> = self
                    .workers
                    .iter()
                    .filter(|w| w.window_index != usize::MAX)
                    .map(|w| {
                        let worktree = w
                            .window_name
                            .strip_prefix(self.config.window_prefix.as_str())
                            .and_then(|s| s.parse::<u64>().ok())
                            .map(|n| self.config.worktree_path(n))
                            .unwrap_or_default();
                        (w.window_name.clone(), w.window_index, worktree)
                    })
                    .collect();
                // Run synchronously-ish: we're about to exit anyway
                tokio::spawn(async move {
                    for (name, idx, worktree) in &workers {
                        close_worker(&config, &log_tx, name, *idx, worktree).await;
                    }
                    // Kill the entire tmux session
                    let _ = tokio::process::Command::new(&config.tmux)
                        .args(["kill-session", "-t", &config.session])
                        .output()
                        .await;
                });
            }
        }
    }

    fn handle_action_picker_key(&mut self, code: KeyCode) -> bool {
        let action_count = self.config.actions.len();
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Mode::ActionPicker { selected } = &mut self.mode {
                    if *selected + 1 < action_count {
                        *selected += 1;
                    }
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Mode::ActionPicker { selected } = &mut self.mode {
                    *selected = selected.saturating_sub(1);
                }
            }
            KeyCode::Enter => {
                if let Mode::ActionPicker { selected } = &self.mode {
                    let idx = *selected;
                    if let Some(action_def) = self.config.actions.get(idx) {
                        let name = action_def.name.clone();
                        let confirm = action_def.confirm;
                        match self.substitute_action_vars(&action_def.command.clone()) {
                            Ok(command) => {
                                if confirm {
                                    self.mode = Mode::Confirm {
                                        action: ConfirmAction::RunAction { name, command },
                                        fetch_latest: false,
                                    };
                                } else {
                                    self.mode = Mode::Normal;
                                    self.run_action_command(&name, &command);
                                }
                            }
                            Err(err) => {
                                self.mode = Mode::Normal;
                                self.push_toast(&err, ToastLevel::Error);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn handle_branch_conflict_key(&mut self, code: KeyCode) -> bool {
        const OPTIONS: usize = 3; // Reuse, Reset, Skip
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if let Mode::BranchConflict { issue_num, .. } = self.mode {
                    self.push_log(&format!("[resolve] Skipped #{issue_num}"));
                }
                self.mode = Mode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Mode::BranchConflict { selected, .. } = &mut self.mode {
                    if *selected + 1 < OPTIONS {
                        *selected += 1;
                    }
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Mode::BranchConflict { selected, .. } = &mut self.mode {
                    *selected = selected.saturating_sub(1);
                }
            }
            KeyCode::Enter => {
                if let Mode::BranchConflict {
                    issue_num,
                    selected,
                } = self.mode
                {
                    self.mode = Mode::Normal;
                    match selected {
                        0 => {
                            // Reuse existing branch
                            if let Some(tx) = &self.prompt_tx {
                                let _ = tx.send(format!("__RESOLVE_REUSE_{issue_num}__"));
                                self.push_log(&format!(
                                    "[resolve] Reusing existing branch for #{issue_num}"
                                ));
                                self.push_toast(
                                    &format!("Reusing branch for #{issue_num}..."),
                                    ToastLevel::Info,
                                );
                            }
                        }
                        1 => {
                            // Reset branch
                            if let Some(tx) = &self.prompt_tx {
                                let _ = tx.send(format!("__RESOLVE_RESET_{issue_num}__"));
                                self.push_log(&format!(
                                    "[resolve] Resetting branch for #{issue_num}"
                                ));
                                self.push_toast(
                                    &format!("Resetting branch for #{issue_num}..."),
                                    ToastLevel::Info,
                                );
                            }
                        }
                        _ => {
                            // Skip
                            self.push_log(&format!("[resolve] Skipped #{issue_num}"));
                        }
                    }
                }
            }
            _ => {}
        }
        false
    }

    pub fn autopilot_config_items(&self) -> Vec<(String, String)> {
        let rt = crate::config::RuntimeConfig::load(&self.state_dir.runtime_config())
            .unwrap_or_else(|| crate::config::RuntimeConfig::from_config(&self.config));
        vec![
            (
                "Autopilot".to_string(),
                if self.autopilot_enabled {
                    "ON".to_string()
                } else {
                    "OFF".to_string()
                },
            ),
            (
                "Batch Size".to_string(),
                rt.autopilot_batch_size.to_string(),
            ),
            (
                "Batch Delay".to_string(),
                format!("{}s", rt.autopilot_batch_delay_secs),
            ),
            (
                "Labels".to_string(),
                if rt.autopilot_labels.is_empty() {
                    "(all)".to_string()
                } else {
                    rt.autopilot_labels.join(", ")
                },
            ),
            (
                "Exclude Labels".to_string(),
                if rt.autopilot_exclude_labels.is_empty() {
                    "(none)".to_string()
                } else {
                    rt.autopilot_exclude_labels.join(", ")
                },
            ),
        ]
    }

    fn handle_autopilot_config_key(&mut self, code: KeyCode) -> bool {
        let item_count = 5usize;
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Mode::AutopilotConfig { selected } = &mut self.mode {
                    if *selected + 1 < item_count {
                        *selected += 1;
                    }
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Mode::AutopilotConfig { selected } = &mut self.mode {
                    *selected = selected.saturating_sub(1);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Mode::AutopilotConfig { selected } = &self.mode {
                    let idx = *selected;
                    let mut rt =
                        crate::config::RuntimeConfig::load(&self.state_dir.runtime_config())
                            .unwrap_or_else(|| {
                                crate::config::RuntimeConfig::from_config(&self.config)
                            });
                    match idx {
                        0 => {
                            // Toggle autopilot
                            self.autopilot_enabled = !self.autopilot_enabled;
                            rt.autopilot = self.autopilot_enabled;
                            if let Some(tx) = &self.autopilot_tx {
                                let _ = tx.send(self.autopilot_enabled);
                            }
                            if self.autopilot_enabled {
                                self.push_toast("Autopilot enabled", ToastLevel::Success);
                            } else {
                                self.push_toast(
                                    "Autopilot disabled (running workers continue)",
                                    ToastLevel::Info,
                                );
                                self.autopilot_status.clear();
                            }
                        }
                        1 => {
                            // Cycle batch size
                            rt.autopilot_batch_size = match rt.autopilot_batch_size {
                                3 => 5,
                                5 => 10,
                                10 => 15,
                                15 => 20,
                                _ => 3,
                            };
                        }
                        2 => {
                            // Cycle batch delay
                            rt.autopilot_batch_delay_secs = match rt.autopilot_batch_delay_secs {
                                30 => 60,
                                60 => 120,
                                120 => 300,
                                300 => 600,
                                _ => 30,
                            };
                        }
                        3 | 4 => {
                            // Labels are not cycleable — show hint
                            self.push_toast(
                                "Edit labels in cwo.toml (autopilot_labels / autopilot_exclude_labels)",
                                ToastLevel::Info,
                            );
                        }
                        _ => {}
                    }
                    rt.save(&self.state_dir.runtime_config());
                    self.push_toast("Autopilot config updated", ToastLevel::Info);
                }
            }
            _ => {}
        }
        false
    }

    fn substitute_action_vars(&self, template: &str) -> Result<String, String> {
        let worker = self.workers.get(self.selected);
        let mut result = template.replace("{repo}", &self.config.repo);

        if result.contains("{window_name}") {
            let val = worker
                .map(|w| w.window_name.as_str())
                .ok_or("No worker selected for {window_name}")?;
            result = result.replace("{window_name}", val);
        }
        if result.contains("{issue_num}") {
            let val = worker
                .and_then(|w| {
                    w.window_name
                        .strip_prefix(self.config.window_prefix.as_str())
                })
                .ok_or("No issue number for selected worker")?;
            result = result.replace("{issue_num}", val);
        }
        if result.contains("{pr_num}") {
            let val = worker
                .and_then(|w| w.pr.as_deref())
                .map(|pr| pr.trim_start_matches('#'))
                .ok_or("No PR for selected worker")?;
            result = result.replace("{pr_num}", val);
        }
        if result.contains("{branch}") {
            let val = worker
                .map(|w| w.branch_name.as_str())
                .filter(|b| !b.is_empty())
                .ok_or("No branch for selected worker")?;
            result = result.replace("{branch}", val);
        }
        if result.contains("{worktree}") {
            let val = worker
                .and_then(|w| {
                    w.window_name
                        .strip_prefix(self.config.window_prefix.as_str())
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(|n| self.config.worktree_path(n))
                })
                .ok_or("No worktree for selected worker")?;
            result = result.replace("{worktree}", &val);
        }
        Ok(result)
    }

    fn run_action_command(&self, name: &str, command: &str) {
        let name = name.to_string();
        let command = command.to_string();
        let log_tx = self.log_tx.clone();
        tokio::spawn(async move {
            let _ = log_tx.send(format!("[action] Running: {name}"));
            let output = tokio::process::Command::new("sh")
                .args(["-c", &command])
                .output()
                .await;
            match output {
                Ok(o) if o.status.success() => {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    let preview: String = stdout.trim().chars().take(60).collect();
                    let _ = log_tx.send(format!("[action] {name}: {preview}"));
                    let _ = log_tx.send(format!("__TOAST_SUCCESS_{name} done__"));
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    let preview: String = stderr.trim().chars().take(60).collect();
                    let _ = log_tx.send(format!("[action] {name} failed: {preview}"));
                    let _ = log_tx.send(format!("__TOAST_ERROR_{name} failed__"));
                }
                Err(e) => {
                    let _ = log_tx.send(format!("[action] {name} error: {e}"));
                    let _ = log_tx.send(format!("__TOAST_ERROR_{name}: {e}__"));
                }
            }
        });
    }

    fn handle_help_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
                self.mode = Mode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Mode::Help { scroll } = &mut self.mode {
                    *scroll = scroll.saturating_add(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Mode::Help { scroll } = &mut self.mode {
                    *scroll = scroll.saturating_sub(1);
                }
            }
            _ => {}
        }
        false
    }

    pub fn help_lines() -> Vec<&'static str> {
        vec![
            "CWO — Claude Worktree Orchestrator",
            "",
            "Orchestrates multiple Claude AI workers across git worktrees.",
            "Each worker runs in its own tmux window with an isolated worktree.",
            "",
            "━━━ KEY BINDINGS ━━━",
            "",
            "  j / k / ↑ / ↓    Navigate worker list",
            "  d / Enter         Detail view — pane output, git log, review notes",
            "  s                 Send a prompt to the selected worker's Claude",
            "  i                 Interrupt selected worker (sends Ctrl-C)",
            "  b                 Broadcast a message to all idle workers",
            "  m                 Check and merge all CLEAN PRs (oldest first)",
            "  M                 Merge the selected worker's PR",
            "  x                 Close selected worker (kill window + remove worktree)",
            "  X (shift)         Close all finished workers (done/shell/failed)",
            "  t                 Switch to selected worker's tmux window",
            "  v                 Open selected worker's PR in browser",
            "  p                 Smart prompt — Claude extracts tasks, files issues,",
            "                      creates worktrees, launches workers",
            "  P (shift)         Direct prompt — launches a worker immediately",
            "                      with your raw prompt. No GitHub issue created.",
            "  n                 New job — enter an existing GitHub issue number",
            "                      to spin up a worker for it",
            "  a                 Run a custom action on the selected worker",
            "                      (configured via [[actions]] in cwo.toml)",
            "  A (shift)         Autopilot config — toggle on/off, set batch size,",
            "                      delay, labels. Autonomously picks and works issues.",
            "  c                 Settings panel — toggle merge policy, auto-review,",
            "                      relaunch behavior, timeouts. Changes are live.",
            "  l                 Toggle the log panel",
            "  :                 Command mode (see commands below)",
            "  ?                 This help screen",
            "  q                 Quit menu (quit only or tear down everything)",
            "  Esc               Quick quit (TUI only, workers keep running)",
            "",
            "━━━ COMMANDS (:) ━━━",
            "",
            "  :help             Show this help",
            "  :stats            Session stats — merged, failed, avg merge time",
            "  :merge all        Check and merge all CLEAN PRs",
            "  :merge pr 42      Merge a specific PR by number",
            "  :rebase all       Fetch main and rebase all worker branches",
            "  :broadcast <msg>  Send <msg> to all idle Claude windows",
            "  :nudge all        Send 'continue with the task' to idle workers",
            "",
            "━━━ WORKER STATES ━━━",
            "",
            "  🟢 active         Claude is working (spinner detected)",
            "  🟡 idle           Claude waiting at prompt for input",
            "  🔴 shell          Claude exited — bare shell visible",
            "  💀 stale          No output change for stale_timeout_secs",
            "  ❌ failed         Exceeded max relaunch attempts",
            "  ✅ done           PR created, work complete",
            "  ⏳ queued         Window exists, Claude not yet launched",
            "  💤 sleeping       Rate limited, waiting",
            "  ⚠️  conflict       Rebase conflict detected on branch",
            "  🔍 probing        AI probe running in split pane",
            "  🔗 waiting        DAG task waiting on dependencies",
            "  👻 orphaned       Worktree exists but no tmux window",
            "",
            "━━━ MERGE POLICIES ━━━",
            "",
            "  auto              Merge CLEAN PRs immediately",
            "  review_then_merge Wait for APPROVED review, then merge",
            "  manual            Never auto-merge — just monitor and notify",
            "",
            "━━━ WORKFLOW ━━━",
            "",
            "  1. CWO reads your discussion issue for tasks (builder loop)",
            "  2. Claude extracts implementable tasks and files GitHub issues",
            "  3. A git worktree + tmux window is created per issue",
            "  4. Claude implements, commits, pushes, opens a PR",
            "  5. AI reviewer checks the PR (if auto_review = true)",
            "  6. CWO auto-merges when CLEAN (per merge_policy)",
            "  7. Remaining branches are rebased after each merge",
            "  8. Crashed workers are auto-relaunched (if auto_relaunch)",
            "",
            "  Or skip all that: press P for a direct prompt.",
            "",
            "━━━ TASK DAG ━━━",
            "",
            "  Define [[tasks]] in cwo.toml with name, prompt, depends_on.",
            "  Tasks launch automatically when dependencies complete.",
            "  Supports sequential, fan-out/fan-in, and full DAG patterns.",
            "",
            "  :dag reset       Reset DAG state (re-run all tasks)",
            "  :dag status      Show DAG task states",
            "",
            "━━━ EVENT LOG ━━━",
            "",
            "  All actions logged to {repo_root}/.claude/cwo-events.jsonl",
            "  View with: cat .claude/cwo-events.jsonl | jq",
            "",
        ]
    }

    pub fn settings_items(&self) -> Vec<(String, String)> {
        let rt = crate::config::RuntimeConfig::load(&self.state_dir.runtime_config())
            .unwrap_or_else(|| crate::config::RuntimeConfig::from_config(&self.config));
        vec![
            ("Merge Policy".to_string(), rt.merge_policy.clone()),
            (
                "Auto Review".to_string(),
                if rt.auto_review {
                    "on".to_string()
                } else {
                    "off".to_string()
                },
            ),
            (
                "Review Timeout".to_string(),
                if rt.review_timeout_secs == 0 {
                    "forever".to_string()
                } else {
                    format!("{}s", rt.review_timeout_secs)
                },
            ),
            (
                "Auto Relaunch".to_string(),
                if rt.auto_relaunch {
                    "on".to_string()
                } else {
                    "off".to_string()
                },
            ),
            (
                "Max Relaunch Attempts".to_string(),
                rt.max_relaunch_attempts.to_string(),
            ),
            (
                "Stale Timeout".to_string(),
                if rt.stale_timeout_secs == 0 {
                    "disabled".to_string()
                } else {
                    format!("{}s", rt.stale_timeout_secs)
                },
            ),
            ("Max Concurrent".to_string(), rt.max_concurrent.to_string()),
        ]
    }

    fn handle_settings_key(&mut self, code: KeyCode) -> bool {
        let item_count = 7usize;
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') => {
                self.mode = Mode::Normal;
                self.status_msg.clear();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Mode::Settings { selected } = &mut self.mode {
                    if *selected + 1 < item_count {
                        *selected += 1;
                    }
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Mode::Settings { selected } = &mut self.mode {
                    *selected = selected.saturating_sub(1);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Mode::Settings { selected } = &self.mode {
                    let idx = *selected;
                    let mut rt =
                        crate::config::RuntimeConfig::load(&self.state_dir.runtime_config())
                            .unwrap_or_else(|| {
                                crate::config::RuntimeConfig::from_config(&self.config)
                            });
                    match idx {
                        0 => {
                            rt.merge_policy = match rt.merge_policy.as_str() {
                                "auto" => "review_then_merge".to_string(),
                                "review_then_merge" => "manual".to_string(),
                                _ => "auto".to_string(),
                            };
                        }
                        1 => rt.auto_review = !rt.auto_review,
                        2 => {
                            rt.review_timeout_secs = match rt.review_timeout_secs {
                                300 => 600,
                                600 => 900,
                                900 => 0,
                                _ => 300,
                            };
                        }
                        3 => rt.auto_relaunch = !rt.auto_relaunch,
                        4 => {
                            rt.max_relaunch_attempts = match rt.max_relaunch_attempts {
                                1 => 2,
                                2 => 3,
                                3 => 5,
                                _ => 1,
                            };
                        }
                        5 => {
                            rt.stale_timeout_secs = match rt.stale_timeout_secs {
                                180 => 300,
                                300 => 600,
                                600 => 0,
                                _ => 180,
                            };
                        }
                        6 => {
                            rt.max_concurrent = match rt.max_concurrent {
                                1 => 2,
                                2 => 3,
                                3 => 5,
                                5 => 8,
                                8 => 10,
                                _ => 1,
                            };
                        }
                        _ => {}
                    }
                    rt.save(&self.state_dir.runtime_config());
                    self.push_toast("Settings updated", ToastLevel::Info);
                }
            }
            _ => {}
        }
        false
    }

    fn mode_history_key(&self) -> &'static str {
        match &self.mode {
            Mode::Send => "send",
            Mode::Broadcast => "broadcast",
            Mode::Command => "command",
            Mode::Prompt => "prompt",
            Mode::DirectPrompt => "direct",
            Mode::NewJob => "job",
            _ => "other",
        }
    }

    fn handle_input_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.status_msg.clear();
                self.history_idx = None;
            }
            KeyCode::Enter => {
                let text = self.input.clone();
                if !text.is_empty() {
                    let key = self.mode_history_key().to_string();
                    let history = self.input_histories.entry(key).or_default();
                    if history.last().map(|s| s.as_str()) != Some(&text) {
                        history.push(text.clone());
                    }
                }
                self.history_idx = None;
                match &self.mode {
                    Mode::Send => self.send_to_selected(&text),
                    Mode::Broadcast => self.broadcast(&text),
                    Mode::Command => self.execute_command(&text),
                    Mode::Prompt => self.send_prompt(&text),
                    Mode::DirectPrompt => self.send_direct_prompt(&text),
                    Mode::NewJob => self.send_new_job(&text),
                    Mode::Normal
                    | Mode::Confirm { .. }
                    | Mode::Detail { .. }
                    | Mode::Settings { .. }
                    | Mode::Help { .. }
                    | Mode::ActionPicker { .. }
                    | Mode::BranchConflict { .. }
                    | Mode::AutopilotConfig { .. } => {}
                }
                // Don't reset mode if handler transitioned to Confirm
                if !matches!(self.mode, Mode::Confirm { .. }) {
                    self.mode = Mode::Normal;
                }
                self.input.clear();
            }
            KeyCode::Up => {
                let key = self.mode_history_key().to_string();
                let history = self.input_histories.get(&key);
                if let Some(hist) = history {
                    if hist.is_empty() {
                        return false;
                    }
                    match self.history_idx {
                        None => {
                            self.input_saved = self.input.clone();
                            self.history_idx = Some(hist.len() - 1);
                            self.input = hist.last().unwrap().clone();
                        }
                        Some(0) => {}
                        Some(i) => {
                            self.history_idx = Some(i - 1);
                            self.input = hist[i - 1].clone();
                        }
                    }
                }
            }
            KeyCode::Down => {
                let key = self.mode_history_key().to_string();
                let hist_len = self.input_histories.get(&key).map(|h| h.len()).unwrap_or(0);
                match self.history_idx {
                    None => {}
                    Some(i) if i + 1 >= hist_len => {
                        self.history_idx = None;
                        self.input = self.input_saved.clone();
                    }
                    Some(i) => {
                        self.history_idx = Some(i + 1);
                        self.input = self.input_histories[&key][i + 1].clone();
                    }
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => {
                self.input.push(c);
            }
            _ => {}
        }
        false
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::ScrollDown => self.select_next(),
            MouseEventKind::ScrollUp => self.select_prev(),
            _ => {}
        }
    }

    fn select_next(&mut self) {
        if self.workers.is_empty() {
            return;
        }
        if self.selected + 1 < self.workers.len() {
            self.selected += 1;
        }
    }

    fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn do_interrupt(&mut self, window_name: &str) {
        if let Some(w) = self.workers.iter().find(|w| w.window_name == window_name) {
            let target = format!("{}:{}", self.config.session, w.window_index);
            let result = std::process::Command::new(&self.config.tmux)
                .args(["send-keys", "-t", &target, "C-c", ""])
                .output();
            match result {
                Ok(_) => self.status_msg = format!("Sent C-c to window {window_name}"),
                Err(e) => self.status_msg = format!("Error: {e}"),
            }
        }
    }

    fn send_to_selected(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(w) = self.workers.get(self.selected) {
            let target = format!("{}:{}", self.config.session, w.window_index);
            // Send text literally (-l) first, then Enter separately.
            // Without -l, tmux interprets key names in the text.
            // Sending Enter separately ensures Claude's TUI receives it
            // as a submit keystroke rather than a pasted newline.
            let text_result = std::process::Command::new(&self.config.tmux)
                .args(["send-keys", "-t", &target, "-l", text])
                .output();
            let enter_result = std::process::Command::new(&self.config.tmux)
                .args(["send-keys", "-t", &target, "Enter"])
                .output();
            match (text_result, enter_result) {
                (Ok(_), Ok(_)) => {
                    let msg = format!("Sent to window {}", w.window_name);
                    self.push_log(&format!("[s] {msg}"));
                    self.status_msg = msg;
                }
                (Err(e), _) | (_, Err(e)) => {
                    let msg = format!("Error sending to {}: {e}", w.window_name);
                    self.push_log(&format!("[s] {msg}"));
                    self.push_toast(&msg, ToastLevel::Error);
                    self.status_msg = msg;
                }
            }
        } else {
            self.push_log("[s] No worker selected");
        }
    }

    fn broadcast(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let targets: Vec<(usize, String)> = self
            .workers
            .iter()
            .filter(|w| {
                w.window_index != usize::MAX
                    && !matches!(w.status.as_str(), "no-window" | "queued")
                    && w.window_name.starts_with(&self.config.window_prefix)
            })
            .map(|w| (w.window_index, w.window_name.clone()))
            .collect();

        let count = targets.len();
        let mut errors = 0usize;
        for (idx, _name) in targets {
            let target = format!("{}:{}", self.config.session, idx);
            let text_ok = std::process::Command::new(&self.config.tmux)
                .args(["send-keys", "-t", &target, "-l", text])
                .output()
                .is_ok();
            let enter_ok = std::process::Command::new(&self.config.tmux)
                .args(["send-keys", "-t", &target, "Enter"])
                .output()
                .is_ok();
            if !text_ok || !enter_ok {
                errors += 1;
            }
        }
        if errors == 0 {
            self.status_msg = format!("Broadcast to {count} workers");
        } else {
            self.status_msg = format!("Broadcast to {count} workers ({errors} errors)");
        }
    }

    fn execute_command(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Handle TUI-local commands
        if text.trim() == "help" {
            self.mode = Mode::Help { scroll: 0 };
            return;
        }
        if text.trim() == "stats" {
            let stats = self.event_stats();
            let avg = match stats.avg_merge_secs() {
                Some(s) if s >= 60 => format!("{}m", s / 60),
                Some(s) => format!("{s}s"),
                None => "—".to_string(),
            };
            let msg = format!(
                "Merged: {} | Failed: {} | Avg merge: {}",
                stats.merged_count, stats.failed_count, avg
            );
            self.push_log(&format!("[stats] {msg}"));
            self.push_toast(&msg, ToastLevel::Info);
            self.status_msg = msg;
            return;
        }

        if text.trim() == "dag reset" {
            let state = crate::poller::DagState::default();
            crate::poller::save_dag_state(&state, &self.state_dir.dag_state());
            self.push_toast(
                "DAG state reset — tasks will re-launch",
                ToastLevel::Warning,
            );
            self.push_log("[dag] DAG state reset");
            self.status_msg = "DAG state reset".into();
            return;
        }
        if text.trim() == "dag status" {
            let state = crate::poller::load_dag_state(&self.state_dir.dag_state());
            let launched: Vec<&str> = state.launched.iter().map(|s| s.as_str()).collect();
            let completed: Vec<&str> = state.completed.iter().map(|s| s.as_str()).collect();
            let total = self.config.tasks.len();
            let msg = format!(
                "DAG: {}/{} complete | launched: [{}] | done: [{}]",
                completed.len(),
                total,
                launched.join(", "),
                completed.join(", ")
            );
            self.push_log(&format!("[dag] {msg}"));
            self.push_toast(&msg, ToastLevel::Info);
            self.status_msg = msg;
            return;
        }

        let preview: String = text.chars().take(40).collect();
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(text.to_string());
            self.status_msg = format!("Command sent: {preview}");
            self.push_toast(&format!("Command: {preview}"), ToastLevel::Info);
        } else {
            self.status_msg = "Builder not running (run_builder = false)".into();
        }
    }

    fn send_prompt(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(tx) = &self.prompt_tx {
            let _ = tx.send(text.to_string());
            self.push_log("[p] Sent prompt to builder");
            self.push_toast("Parsing with Claude...", ToastLevel::Info);
        } else {
            let msg = "Builder not running (run_builder = false)";
            self.status_msg = msg.into();
            self.push_log(&format!("[p] {msg}"));
            self.push_toast(msg, ToastLevel::Error);
        }
    }

    fn send_direct_prompt(&mut self, text: &str) {
        if text.is_empty() {
            self.push_log("[P] Empty prompt, ignoring");
            return;
        }
        if let Some(tx) = &self.prompt_tx {
            let msg = format!("__DIRECT_{}__", text);
            match tx.send(msg) {
                Ok(_) => {
                    let preview: String = text.chars().take(40).collect();
                    self.push_log(&format!("[P] Launching direct worker: {preview}"));
                    self.push_toast("Launching worker...", ToastLevel::Info);
                }
                Err(e) => {
                    let msg = format!("Failed to send direct prompt: {e}");
                    self.push_log(&msg);
                    self.push_toast(&msg, ToastLevel::Error);
                }
            }
        } else {
            let msg = "Builder not running (run_builder = false)";
            self.status_msg = msg.into();
            self.push_log(&format!("[P] {msg}"));
            self.push_toast(msg, ToastLevel::Error);
        }
    }

    fn send_new_job(&mut self, text: &str) {
        if text.is_empty() {
            self.push_log("[n] Empty input, ignoring");
            return;
        }
        match text.trim().parse::<u64>() {
            Ok(n) => {
                // Set up branch editing state
                self.branch_input = Some(self.config.branch_name(n));
                self.branch_loading = true;
                self.branch_focused = false;
                self.branch_edited = false;

                // Spawn async title fetch
                let repo = self.config.repo.clone();
                let log_tx = self.log_tx.clone();
                tokio::spawn(async move {
                    match crate::github::get_issue(&repo, n).await {
                        Ok((title, _body)) => {
                            let _ = log_tx.send(format!("__ISSUE_TITLE_{n}_{title}__"));
                        }
                        Err(_) => {
                            let _ = log_tx.send(format!("__ISSUE_TITLE_DONE_{n}__"));
                        }
                    }
                });

                self.mode = Mode::Confirm {
                    action: ConfirmAction::LaunchIssue { issue_num: n },
                    fetch_latest: true,
                };
            }
            Err(_) => {
                let msg = format!("Invalid issue number: {text}");
                self.push_log(&format!("[n] {msg}"));
                self.push_toast(&msg, ToastLevel::Error);
                self.status_msg = msg;
            }
        }
    }

    fn confirm_new_job(&mut self, issue_num: u64) {
        let msg = if self.branch_edited {
            if let Some(ref branch) = self.branch_input {
                format!("__NEWJOB_{issue_num}_BRANCH_{branch}__")
            } else {
                format!("__NEWJOB_{issue_num}__")
            }
        } else {
            format!("__NEWJOB_{issue_num}__")
        };
        if let Some(tx) = &self.prompt_tx {
            match tx.send(msg) {
                Ok(_) => {
                    self.push_log(&format!("[n] Sent new-job request for #{issue_num}"));
                    self.push_toast(
                        &format!("Launching worker for #{issue_num}..."),
                        ToastLevel::Info,
                    );
                    // Persist to config so restarts pick it up
                    if let Err(e) = Config::append_issue(&self.config.config_path, issue_num) {
                        self.push_log(&format!(
                            "[n] Warning: could not save #{issue_num} to config: {e}"
                        ));
                    }
                }
                Err(e) => {
                    let msg = format!("Failed to queue new-job #{issue_num}: {e}");
                    self.push_log(&msg);
                    self.push_toast(&msg, ToastLevel::Error);
                }
            }
        } else {
            let msg = "Builder not running (run_builder = false)";
            self.status_msg = msg.into();
            self.push_log(&format!("[n] {msg}"));
            self.push_toast(msg, ToastLevel::Error);
        }
    }

    pub fn active_count(&self) -> usize {
        self.workers.iter().filter(|w| w.status == "active").count()
    }

    pub fn idle_count(&self) -> usize {
        self.workers.iter().filter(|w| w.status == "idle").count()
    }

    pub fn queued_count(&self) -> usize {
        self.workers.iter().filter(|w| w.status == "queued").count()
    }

    pub fn last_refresh_secs(&self) -> u64 {
        self.last_refresh.elapsed().as_secs()
    }

    pub fn next_scan_remaining_secs(&self) -> Option<u64> {
        self.next_scan_at
            .map(|at| at.saturating_duration_since(Instant::now()).as_secs())
    }

    pub fn backoff_status(&self) -> String {
        if let Ok(content) = std::fs::read_to_string(self.state_dir.backoff()) {
            let ts: i64 = content.trim().parse().unwrap_or(0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let remaining = ts - now;
            if remaining > 0 {
                return format!("{remaining}s remaining");
            }
        }
        "none".to_string()
    }

    pub fn save_history(&self) {
        const MAX_PER_KEY: usize = 50;
        let trimmed: HashMap<String, Vec<String>> = self
            .input_histories
            .iter()
            .map(|(k, v)| {
                let start = v.len().saturating_sub(MAX_PER_KEY);
                (k.clone(), v[start..].to_vec())
            })
            .collect();
        if let Ok(json) = serde_json::to_string(&trimmed) {
            let _ = std::fs::write(self.state_dir.history(), json);
        }
    }
}

fn load_history(state_dir: &StateDir) -> HashMap<String, Vec<String>> {
    let Ok(content) = std::fs::read_to_string(state_dir.history()) else {
        return HashMap::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

async fn close_worker(
    config: &Config,
    log_tx: &mpsc::UnboundedSender<String>,
    window_name: &str,
    window_index: usize,
    worktree: &str,
) {
    let _ = log_tx.send(format!("[close] Closing {window_name}..."));

    // Kill tmux window
    if window_index != usize::MAX {
        let target = format!("{}:{}", config.session, window_index);
        let _ = tokio::process::Command::new(&config.tmux)
            .args(["kill-window", "-t", &target])
            .output()
            .await;
    }

    // Remove worktree
    if !worktree.is_empty() && std::path::Path::new(worktree).exists() {
        let _ = tokio::process::Command::new("git")
            .args([
                "-C",
                &config.repo_root,
                "worktree",
                "remove",
                "--force",
                worktree,
            ])
            .output()
            .await;
    }

    let _ = log_tx.send(format!("[close] Closed {window_name}"));
    let _ = log_tx.send(format!("__TOAST_SUCCESS_Closed {window_name}__"));
}
