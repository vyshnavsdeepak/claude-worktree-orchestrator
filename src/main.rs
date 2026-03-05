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

fn parse_args() -> (String, bool) {
    let mut config_path = "cwo.toml".to_string();
    let mut no_builder = false;
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

    (config_path, no_builder)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (config_path, no_builder) = parse_args();

    let mut config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {e}");
            print_example_config();
            std::process::exit(1);
        }
    };

    if no_builder {
        config.run_builder = false;
    }

    if config.run_builder && config.repo_root.is_empty() {
        eprintln!("Error: repo_root must be set in cwo.toml for builder mode.");
        eprintln!("Use --no-builder to run in TUI-only mode.");
        std::process::exit(1);
    }

    // Auto-disable builder when issues list is provided (no discussion needed)
    if !config.issues.is_empty() {
        config.run_builder = false;
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
