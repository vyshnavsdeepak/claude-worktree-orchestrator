mod app;
mod autopilot;
mod builder;
mod config;
mod dag;
#[cfg(feature = "dashboard")]
mod dashboard;
mod events;
mod github;
mod monitor;
mod poller;
mod prompt;
mod state;
mod ui;

use std::io::IsTerminal;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use app::App;
use config::{Config, EXAMPLE_CONFIG};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use events::EventLog;
use monitor::BackoffState;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::{mpsc, watch, Mutex};

fn print_example_config() {
    eprintln!("No config file found. Create cwo.toml with:\n");
    eprintln!("{EXAMPLE_CONFIG}");
    eprintln!("Then run: cwo --config cwo.toml");
}

enum Command {
    Run(RunArgs),
    Init { interactive: bool },
}

struct RunArgs {
    config_path: String,
    no_builder: bool,
    in_tmux_reexec: bool,
}

fn parse_args() -> Command {
    let mut config_path = "cwo.toml".to_string();
    let mut no_builder = false;
    let mut in_tmux_reexec = false;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "init" => {
                let interactive = args.any(|a| a == "-i" || a == "--interactive");
                return Command::Init { interactive };
            }
            "--config" | "-c" => {
                if let Some(v) = args.next() {
                    config_path = v;
                }
            }
            "--no-builder" => {
                no_builder = true;
            }
            "--_in-tmux" => {
                in_tmux_reexec = true;
            }
            "--help" | "-h" => {
                eprintln!("Usage: cwo [init] [--config <path>] [--no-builder]");
                eprintln!();
                eprintln!("Commands:");
                eprintln!("  init              Generate cwo.toml (auto-detect)");
                eprintln!("  init -i           Generate cwo.toml (interactive prompts)");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --config <path>   Path to cwo.toml (default: ./cwo.toml)");
                eprintln!(
                    "  --no-builder      TUI-only: watch workers without running the builder loop"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    Command::Run(RunArgs {
        config_path,
        no_builder,
        in_tmux_reexec,
    })
}

fn prompt_with_default(label: &str, default: &str) -> String {
    eprint!("  {label} [{default}]: ");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap_or(0);
    let input = input.trim();
    if input.is_empty() {
        default.to_string()
    } else {
        input.to_string()
    }
}

fn detect_repo_root() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| std::env::current_dir().unwrap().display().to_string())
}

fn detect_repo() -> String {
    std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .and_then(|url| parse_github_repo(&url))
        .unwrap_or_else(|| "owner/repo".to_string())
}

fn detect_shell_prompts() -> String {
    let user = std::env::var("USER").unwrap_or_default();
    if user.is_empty() {
        r#"["$ ", ">> "]"#.to_string()
    } else {
        format!(r#"["{user}@", "$ ", ">> "]"#)
    }
}

fn cmd_init(interactive: bool) -> anyhow::Result<()> {
    let path = "cwo.toml";
    if std::path::Path::new(path).exists() {
        eprintln!("cwo.toml already exists. Delete it first or edit it directly.");
        std::process::exit(1);
    }

    let d_repo_root = detect_repo_root();
    let d_repo = detect_repo();
    let d_session = d_repo.split('/').next_back().unwrap_or("cwo").to_string();
    let d_tmux = which_tmux();
    let d_max = "3";

    let (repo_root, repo, session, tmux, max_concurrent, issues) = if interactive {
        eprintln!("CWO Init (press Enter to accept defaults)\n");
        let repo = prompt_with_default("GitHub repo (owner/name)", &d_repo);
        let repo_root = prompt_with_default("Repo root", &d_repo_root);
        let session = prompt_with_default(
            "Tmux session name",
            repo.split('/').next_back().unwrap_or(&d_session),
        );
        let tmux = prompt_with_default("Tmux binary", &d_tmux);
        let max_concurrent = prompt_with_default("Max concurrent workers", d_max);
        let issues = prompt_with_default("Issue numbers (comma-separated, or empty)", "");
        eprintln!();
        (repo_root, repo, session, tmux, max_concurrent, issues)
    } else {
        (
            d_repo_root,
            d_repo,
            d_session,
            d_tmux,
            d_max.to_string(),
            String::new(),
        )
    };

    let shell_prompt = detect_shell_prompts();

    let issues_line = if issues.trim().is_empty() {
        "# issues = [123, 456, 789]".to_string()
    } else {
        let nums: Vec<&str> = issues
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        format!("issues = [{}]", nums.join(", "))
    };

    let config = format!(
        r#"# CWO config for {repo}

session = "{session}"
repo = "{repo}"
repo_root = "{repo_root}"
tmux = "{tmux}"

worktree_dir = ".claude/worktrees"
branch_prefix = "fix/issue-"
window_prefix = "issue-"
shell_prompts = {shell_prompt}

max_concurrent = {max_concurrent}
claude_flags = "--dangerously-skip-permissions"

# ─── Issue mode ──────────────────────────────────────────────────
# Launch workers for specific GitHub issues:
{issues_line}

# ─── Builder mode ────────────────────────────────────────────────
# Automatically extract tasks from a discussion issue:
# discussion_issue = 1
# merge_policy = "auto"  # "auto" | "review_then_merge" | "manual"
# auto_review = true
# builder_sleep_secs = 300

# ─── Autopilot mode ──────────────────────────────────────────────
# Autonomously picks open GitHub issues, prioritizes them, and
# launches workers in batches with conflict minimization.
# autopilot = true
# autopilot_batch_size = 10
# autopilot_batch_delay_secs = 60
# autopilot_labels = ["bug", "good first issue"]
# autopilot_exclude_labels = ["wontfix", "discussion"]

# ─── Task DAG ────────────────────────────────────────────────────
# Pre-defined tasks with dependency ordering:
# [[tasks]]
# name = "feature-a"
# prompt = "Implement feature A..."
#
# [[tasks]]
# name = "feature-b"
# prompt = "Implement feature B..."
# depends_on = ["feature-a"]
"#
    );

    std::fs::write(path, &config)?;
    eprintln!("Created cwo.toml");
    eprintln!();
    eprintln!("  repo:    {repo}");
    eprintln!("  root:    {repo_root}");
    eprintln!("  session: {session}");
    eprintln!("  tmux:    {tmux}");
    if !issues.trim().is_empty() {
        eprintln!("  issues:  {issues}");
    }
    eprintln!();
    if issues.trim().is_empty() {
        eprintln!("Edit cwo.toml to add issues or tasks, then run: cwo");
    } else {
        eprintln!("Run: cwo");
    }

    Ok(())
}

fn parse_github_repo(url: &str) -> Option<String> {
    // Handle SSH: git@github.com:owner/repo.git or git@custom-alias:owner/repo.git
    if url.starts_with("git@") {
        if let Some(colon_pos) = url.find(':') {
            let rest = &url[colon_pos + 1..];
            let repo = rest.trim_end_matches(".git");
            if repo.contains('/') {
                return Some(repo.to_string());
            }
        }
    }
    // Handle HTTPS: https://github.com/owner/repo.git
    if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        return Some(rest.trim_end_matches(".git").to_string());
    }
    None
}

fn which_tmux() -> String {
    // Try common paths
    for path in [
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
        "/usr/bin/tmux",
    ] {
        if std::path::Path::new(path).exists() {
            return path.to_string();
        }
    }
    // Fallback to which
    std::process::Command::new("which")
        .arg("tmux")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "/usr/bin/tmux".to_string())
}

fn reexec_in_tmux(config: &Config, cli: &RunArgs) -> anyhow::Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| "cwo".into());
    let session_name = config.session.as_str();

    let mut cmd_args = vec![
        "--config".to_string(),
        cli.config_path.clone(),
        "--_in-tmux".to_string(),
    ];
    if cli.no_builder {
        cmd_args.push("--no-builder".to_string());
    }

    let exe_str = exe.display().to_string();
    let full_cmd = std::iter::once(exe_str.as_str())
        .chain(cmd_args.iter().map(|s: &String| s.as_str()))
        .collect::<Vec<_>>()
        .join(" ");

    let status = std::process::Command::new(&config.tmux)
        .args(["new-session", "-d", "-s", session_name, &full_cmd])
        .status();

    match status {
        Ok(s) if s.success() => {
            eprintln!("[cwo] Launched in tmux session '{session_name}'");
            eprintln!(
                "[cwo] Attach with: {} attach -t {session_name}",
                config.tmux
            );
            Ok(())
        }
        Ok(s) => {
            // Session may already exist — try sending to a new window instead
            let status2 = std::process::Command::new(&config.tmux)
                .args(["new-window", "-t", session_name, &full_cmd])
                .status();
            match status2 {
                Ok(s2) if s2.success() => {
                    eprintln!("[cwo] Launched in new window in tmux session '{session_name}'");
                    Ok(())
                }
                _ => {
                    anyhow::bail!("Failed to launch in tmux (exit code: {s})");
                }
            }
        }
        Err(e) => {
            anyhow::bail!("Failed to run tmux: {e}");
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd = parse_args();
    let cli = match cmd {
        Command::Init { interactive } => return cmd_init(interactive),
        Command::Run(args) => args,
    };

    let mut config = match Config::load(&cli.config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {e}");
            print_example_config();
            std::process::exit(1);
        }
    };

    // Re-exec inside tmux when no TTY is available (e.g. called from another Claude, CI, scripts)
    if !std::io::stdin().is_terminal() && !cli.in_tmux_reexec {
        return reexec_in_tmux(&config, &cli);
    }

    if cli.no_builder || !config.issues.is_empty() || !config.tasks.is_empty() {
        config.run_builder = false;
    }

    if config.run_builder && config.repo_root.is_empty() {
        eprintln!("Error: repo_root must be set in cwo.toml for builder mode.");
        eprintln!("Use --no-builder to run in TUI-only mode.");
        std::process::exit(1);
    }

    if config.run_builder && config.discussion_issue.is_none() {
        eprintln!("Error: discussion_issue must be set for builder mode.");
        eprintln!("Use --no-builder for direct-prompt-only usage.");
        std::process::exit(1);
    }

    let state_dir = state::StateDir::new(&config.config_path);
    if let Err(e) = state_dir.ensure() {
        eprintln!("Error creating state directory: {e}");
        std::process::exit(1);
    }
    let state_dir = Arc::new(state_dir);

    let event_log = EventLog::new(&config.repo_root);
    let config = Arc::new(config);
    let is_polling = Arc::new(AtomicBool::new(false));
    let (log_tx, log_rx) = mpsc::unbounded_channel::<String>();
    let (worker_tx, worker_rx) = watch::channel(Vec::new());

    // Spawn background poller
    {
        let config = Arc::clone(&config);
        let is_polling = Arc::clone(&is_polling);
        let log_tx = log_tx.clone();
        let state_dir = Arc::clone(&state_dir);
        tokio::spawn(async move {
            poller::run(config, worker_tx, log_tx, is_polling, state_dir).await;
        });
    }

    // Builder loop (only when run_builder = true)
    let cmd_tx = if config.run_builder {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<String>();

        let config_clone = Arc::clone(&config);
        let sd = Arc::clone(&state_dir);
        let backoff = Arc::new(Mutex::new(BackoffState::new(&sd)));
        let log_tx_builder = log_tx.clone();
        let event_log_builder = event_log.clone();
        tokio::spawn(async move {
            builder::run(
                config_clone,
                log_tx_builder,
                backoff,
                cmd_rx,
                event_log_builder,
                sd,
            )
            .await;
        });

        Some(cmd_tx)
    } else {
        None
    };

    // Prompt handler — always active (direct prompts, new jobs, smart prompts)
    let prompt_tx = {
        let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<String>();

        let c = Arc::clone(&config);
        let log_tx_prompt = log_tx.clone();
        let event_log_prompt = event_log.clone();
        let sd = Arc::clone(&state_dir);
        tokio::spawn(async move {
            while let Some(msg) = prompt_rx.recv().await {
                let c2 = Arc::clone(&c);
                let tx2 = log_tx_prompt.clone();
                let el2 = event_log_prompt.clone();
                let sd2 = Arc::clone(&sd);
                if let Some(body) = msg
                    .strip_prefix("__NEWJOB_")
                    .and_then(|s| s.strip_suffix("__"))
                {
                    // Strip optional _PLAN suffix
                    let (body, plan_mode) = if let Some(b) = body.strip_suffix("_PLAN") {
                        (b, true)
                    } else {
                        (body, false)
                    };
                    // Parse optional _BASE_ suffix
                    let (body, base_branch) = if let Some(pos) = body.find("_BASE_") {
                        let base = body[pos + 6..].to_string();
                        (
                            &body[..pos],
                            if base.is_empty() { None } else { Some(base) },
                        )
                    } else {
                        (body, None)
                    };
                    // Parse optional branch override: "{num}_BRANCH_{branch}" or just "{num}"
                    let (n, branch_override) = if let Some(branch_pos) = body.find("_BRANCH_") {
                        let num_str = &body[..branch_pos];
                        let branch = body[branch_pos + 8..].to_string();
                        (
                            num_str.parse::<u64>().ok(),
                            if branch.is_empty() {
                                None
                            } else {
                                Some(branch)
                            },
                        )
                    } else {
                        (body.parse::<u64>().ok(), None)
                    };
                    if let Some(n) = n {
                        tokio::spawn(async move {
                            prompt::run_new_job(
                                c2,
                                n,
                                tx2,
                                el2,
                                sd2,
                                branch_override,
                                plan_mode,
                                base_branch,
                            )
                            .await
                        });
                    }
                } else if let Some(n) = msg
                    .strip_prefix("__RESOLVE_REUSE_")
                    .and_then(|s| s.strip_suffix("__"))
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    tokio::spawn(async move { prompt::resolve_reuse(c2, n, tx2, el2, sd2).await });
                } else if let Some(n) = msg
                    .strip_prefix("__RESOLVE_RESET_")
                    .and_then(|s| s.strip_suffix("__"))
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    tokio::spawn(async move { prompt::resolve_reset(c2, n, tx2, el2, sd2).await });
                } else if let Some(prompt_text) = msg
                    .strip_prefix("__DIRECT_")
                    .and_then(|s| s.strip_suffix("__"))
                {
                    let prompt_text = prompt_text.to_string();
                    tokio::spawn(
                        async move { prompt::run_direct(c2, prompt_text, tx2, el2).await },
                    );
                } else {
                    tokio::spawn(async move { prompt::run(c2, msg, tx2, el2, sd2).await });
                }
            }
        });

        Some(prompt_tx)
    };

    // Autopilot toggle channel + task
    let (autopilot_tx, autopilot_rx) = watch::channel(config.autopilot);
    {
        let c = Arc::clone(&config);
        let ap_worker_rx = worker_rx.clone();
        let ap_log_tx = log_tx.clone();
        let ap_prompt_tx = prompt_tx.as_ref().unwrap().clone();
        let sd = Arc::clone(&state_dir);
        tokio::spawn(async move {
            autopilot::run(c, ap_worker_rx, ap_log_tx, ap_prompt_tx, sd, autopilot_rx).await;
        });
    }

    // DAG scheduler — launches when [[tasks]] are defined in config
    if !config.tasks.is_empty() {
        let c = Arc::clone(&config);
        let dag_worker_rx = worker_rx.clone();
        let dag_log_tx = log_tx.clone();
        let dag_event_log = event_log.clone();
        let sd = Arc::clone(&state_dir);
        tokio::spawn(async move {
            dag::run(c, dag_worker_rx, dag_log_tx, dag_event_log, sd).await;
        });
    }

    // Issue list launcher — spin up workers for each issue in config.issues (skip existing)
    if !config.issues.is_empty() {
        let issues = config.issues.clone();
        let issue_log_tx = log_tx.clone();
        let issue_config = Arc::clone(&config);
        tokio::spawn(async move {
            let existing = monitor::list_windows(&issue_config).await;
            let existing_names: std::collections::HashSet<String> =
                existing.into_iter().map(|(_, name)| name).collect();
            let to_launch: Vec<u64> = issues
                .into_iter()
                .filter(|n| !existing_names.contains(&issue_config.window_name(*n)))
                .collect();
            if to_launch.is_empty() {
                let _ = issue_log_tx.send("[issues] All issues already have workers".to_string());
            } else {
                let _ = issue_log_tx.send(format!(
                    "[issues] {} issues pending launch: {:?}",
                    to_launch.len(),
                    to_launch
                ));
                let nums: Vec<String> = to_launch.iter().map(|n| n.to_string()).collect();
                let _ = issue_log_tx.send(format!("__STARTUP_PENDING_{}__", nums.join(",")));
            }
        });
    }

    // Dashboard server (feature-gated)
    #[cfg(feature = "dashboard")]
    if let Some(port) = config.dashboard_port {
        let ctx = std::sync::Arc::new(dashboard::DashboardContext {
            config: Arc::clone(&config),
            worker_rx: worker_rx.clone(),
            event_log: event_log.clone(),
            state_dir: Arc::clone(&state_dir),
            prompt_tx: prompt_tx.clone(),
        });
        tokio::spawn(dashboard::start(ctx, port));
        let _ = log_tx.send(format!("[dashboard] Listening on port {port}"));
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(
        Arc::clone(&config),
        Arc::clone(&state_dir),
        worker_rx,
        Some(log_rx),
        is_polling,
        cmd_tx,
        prompt_tx,
        log_tx,
        event_log,
        Some(autopilot_tx),
    );

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let quit = app.handle_key(key.code, key.modifiers);
                    if quit {
                        break;
                    }
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse(mouse);
                }
                _ => {}
            }
        }

        app.tick();

        if app.needs_reexec {
            app.save_history();
            disable_raw_mode()?;
            execute!(
                terminal.backend_mut(),
                LeaveAlternateScreen,
                DisableMouseCapture
            )?;
            terminal.show_cursor()?;

            // Re-exec the updated binary with the same args
            let exe = std::env::current_exe().unwrap_or_else(|_| "cwo".into());
            let args: Vec<String> = std::env::args().skip(1).collect();
            let err = std::os::unix::process::CommandExt::exec(
                std::process::Command::new(&exe).args(&args),
            );
            // exec only returns on error
            eprintln!("Failed to re-exec: {err}");
            std::process::exit(1);
        }
    }

    app.save_history();

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
