mod app;
mod builder;
mod config;
mod dag;
mod events;
mod github;
mod monitor;
mod poller;
mod prompt;
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

struct CliArgs {
    config_path: String,
    no_builder: bool,
    in_tmux_reexec: bool,
}

fn parse_args() -> CliArgs {
    let mut config_path = "cwo.toml".to_string();
    let mut no_builder = false;
    let mut in_tmux_reexec = false;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
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
                eprintln!("Usage: cwo [--config <path>] [--no-builder]");
                eprintln!();
                eprintln!("  --config <path>   Path to cwo.toml (default: ./cwo.toml)");
                eprintln!(
                    "  --no-builder      TUI-only: watch workers without running the builder loop"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    CliArgs {
        config_path,
        no_builder,
        in_tmux_reexec,
    }
}

fn reexec_in_tmux(config: &Config, cli: &CliArgs) -> anyhow::Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| "cwo".into());
    let session_name = "cwo";

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
        .chain(cmd_args.iter().map(|s| s.as_str()))
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
    let cli = parse_args();

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
        tokio::spawn(async move {
            poller::run(config, worker_tx, log_tx, is_polling).await;
        });
    }

    // Builder loop (only when run_builder = true)
    let cmd_tx = if config.run_builder {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<String>();

        let config_clone = Arc::clone(&config);
        let backoff = Arc::new(Mutex::new(BackoffState::new()));
        let log_tx_builder = log_tx.clone();
        let event_log_builder = event_log.clone();
        tokio::spawn(async move {
            builder::run(
                config_clone,
                log_tx_builder,
                backoff,
                cmd_rx,
                event_log_builder,
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
        tokio::spawn(async move {
            while let Some(msg) = prompt_rx.recv().await {
                let c2 = Arc::clone(&c);
                let tx2 = log_tx_prompt.clone();
                let el2 = event_log_prompt.clone();
                if let Some(n) = msg
                    .strip_prefix("__NEWJOB_")
                    .and_then(|s| s.strip_suffix("__"))
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    tokio::spawn(async move { prompt::run_new_job(c2, n, tx2, el2).await });
                } else if let Some(prompt_text) = msg
                    .strip_prefix("__DIRECT_")
                    .and_then(|s| s.strip_suffix("__"))
                {
                    let prompt_text = prompt_text.to_string();
                    tokio::spawn(
                        async move { prompt::run_direct(c2, prompt_text, tx2, el2).await },
                    );
                } else {
                    tokio::spawn(async move { prompt::run(c2, msg, tx2, el2).await });
                }
            }
        });

        Some(prompt_tx)
    };

    // DAG scheduler — launches when [[tasks]] are defined in config
    if !config.tasks.is_empty() {
        let c = Arc::clone(&config);
        let dag_worker_rx = worker_rx.clone();
        let dag_log_tx = log_tx.clone();
        let dag_event_log = event_log.clone();
        tokio::spawn(async move {
            dag::run(c, dag_worker_rx, dag_log_tx, dag_event_log).await;
        });
    }

    // Issue list launcher — spin up workers for each issue in config.issues
    if !config.issues.is_empty() {
        let issues = config.issues.clone();
        let tx = prompt_tx.as_ref().unwrap().clone();
        let issue_log_tx = log_tx.clone();
        tokio::spawn(async move {
            let _ = issue_log_tx.send(format!(
                "[issues] Launching workers for {} issues: {:?}",
                issues.len(),
                issues
            ));
            for n in issues {
                let _ = tx.send(format!("__NEWJOB_{n}__"));
                // Stagger launches to avoid GitHub API rate limits
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(
        Arc::clone(&config),
        worker_rx,
        Some(log_rx),
        is_polling,
        cmd_tx,
        prompt_tx,
        log_tx,
        event_log,
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
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
