//! Typed message protocol for inter-component channels.
//!
//! `AppCommand` flows on the prompt channel (app/autopilot/dashboard → main.rs dispatcher).
//! `LogMessage` flows on the log channel (all modules → app.rs UI).
//!
//! Replaces a previous string-based protocol with magic prefixes like
//! `__NEWJOB_42_BRANCH_feat/foo_BASE_main_PLAN__` and `__TOAST_INFO_msg__`.

use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// Commands sent from the UI/autopilot/dashboard to the main dispatcher to
/// launch work.
#[derive(Debug)]
pub enum AppCommand {
    NewJob {
        issue_num: u64,
        branch_override: Option<String>,
        base_branch: Option<String>,
        plan_mode: bool,
    },
    ResolveReuse {
        issue_num: u64,
    },
    ResolveReset {
        issue_num: u64,
    },
    Direct {
        prompt: String,
    },
    /// Free-form smart prompt — routed to task-extraction flow.
    SmartPrompt {
        text: String,
    },
}

/// Messages sent on the log channel. Includes plain log lines as well as
/// typed UI updates (toasts, status, data refreshes).
#[derive(Debug)]
pub enum LogMessage {
    Log(String),
    Toast { level: ToastLevel, msg: String },
    NextScan { secs: u64 },
    BranchRename { issue_num: u64, name: String },
    BranchRenameDone { issue_num: u64 },
    IssueTitle { issue_num: u64, title: String },
    IssueTitleDone { issue_num: u64 },
    RepoIssueCounts { open: u64, closed: u64 },
    SelfUpdateOk,
    SelfUpdateFail { reason: String },
    BranchConflict { issue_num: u64 },
    StartupPending { issues: Vec<u64> },
    StartupIssueState { issue_num: u64, state: String },
    AutopilotStatus(String),
    AutopilotMerged { pr_num: u64, title: String },
    AutopilotMergeQueue(MergeQueueUpdate),
    AutopilotUpcoming(UpcomingUpdate),
}

#[derive(Debug)]
pub enum MergeQueueUpdate {
    Clear,
    Set {
        pr_num: u64,
        title: String,
        status: String,
    },
}

#[derive(Debug)]
pub enum UpcomingUpdate {
    Clear,
    Set {
        issue_num: u64,
        title: String,
        priority: String,
        complexity: String,
        reason: String,
    },
}

// ─── Shared send helpers ────────────────────────────────────────────────────

pub fn log(tx: &mpsc::UnboundedSender<LogMessage>, msg: impl Into<String>) {
    let _ = tx.send(LogMessage::Log(msg.into()));
}

pub fn toast(tx: &mpsc::UnboundedSender<LogMessage>, level: ToastLevel, msg: impl Into<String>) {
    let _ = tx.send(LogMessage::Toast {
        level,
        msg: msg.into(),
    });
}
