use anyhow::{bail, Context, Result};
use tokio::process::Command;

async fn run_gh(args: &[&str]) -> Result<String> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .await
        .context("gh command failed")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("{}", stderr);
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub async fn get_discussion(repo: &str, issue_num: u64) -> Result<String> {
    let num = issue_num.to_string();
    let jq = r#""=== DISCUSSION ===\nTitle: " + .title + "\n\n" + .body + "\n\n=== COMMENTS ===\n" + (.comments | map(.author.login + ": " + .body) | join("\n---\n"))"#;
    run_gh(&[
        "issue",
        "view",
        &num,
        "--repo",
        repo,
        "--comments",
        "--json",
        "title,body,comments",
        "-q",
        jq,
    ])
    .await
}

pub async fn list_open_issues(repo: &str) -> Result<Vec<(u64, String)>> {
    let out = run_gh(&[
        "issue",
        "list",
        "--repo",
        repo,
        "--state",
        "open",
        "--json",
        "number,title",
        "-q",
        r##".[] | "#\(.number): \(.title)""##,
    ])
    .await?;

    let mut result = Vec::new();
    for line in out.lines() {
        if let Some(rest) = line.trim().strip_prefix('#') {
            let mut parts = rest.splitn(2, ": ");
            if let (Some(num_str), Some(title)) = (parts.next(), parts.next()) {
                if let Ok(num) = num_str.parse::<u64>() {
                    result.push((num, title.to_string()));
                }
            }
        }
    }
    Ok(result)
}

pub async fn list_open_issues_with_labels(
    repo: &str,
    labels: &[String],
    exclude_labels: &[String],
) -> Result<Vec<(u64, String, String)>> {
    let mut args = vec![
        "issue",
        "list",
        "--repo",
        repo,
        "--state",
        "open",
        "--json",
        "number,title,body,labels",
        "--limit",
        "100",
    ];
    let label_str = labels.join(",");
    if !labels.is_empty() {
        args.push("--label");
        args.push(&label_str);
    }
    let jq = r##".[] | "\(.number)\t\(.title)\t\(.labels | map(.name) | join(","))""##;
    args.push("-q");
    args.push(jq);
    let out = run_gh(&args).await?;

    let exclude_set: std::collections::HashSet<&str> =
        exclude_labels.iter().map(|s| s.as_str()).collect();

    let mut result = Vec::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let num: u64 = match parts[0].parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let title = parts[1].to_string();
        let issue_labels: Vec<&str> = if parts.len() > 2 && !parts[2].is_empty() {
            parts[2].split(',').collect()
        } else {
            vec![]
        };
        if issue_labels.iter().any(|l| exclude_set.contains(l)) {
            continue;
        }
        // Fetch body separately for analysis (gh list doesn't include it reliably in jq)
        result.push((num, title, String::new()));
    }
    Ok(result)
}

/// Get open and closed issue counts for a repo.
pub async fn issue_counts(repo: &str) -> Result<(u64, u64)> {
    let open_out = run_gh(&[
        "issue", "list", "--repo", repo, "--state", "open", "--json", "number", "-q", "length",
    ])
    .await
    .unwrap_or_default();
    let closed_out = run_gh(&[
        "issue", "list", "--repo", repo, "--state", "closed", "--limit", "1000", "--json",
        "number", "-q", "length",
    ])
    .await
    .unwrap_or_default();
    let open = open_out.trim().parse::<u64>().unwrap_or(0);
    let closed = closed_out.trim().parse::<u64>().unwrap_or(0);
    Ok((open, closed))
}

pub async fn create_issue(repo: &str, title: &str, body: &str) -> Result<u64> {
    let out = run_gh(&[
        "issue", "create", "--repo", repo, "--title", title, "--body", body,
    ])
    .await?;

    // Output is a URL like https://github.com/owner/repo/issues/123
    let num_str = out.trim().rsplit('/').next().context("parse issue URL")?;
    num_str.parse::<u64>().context("parse issue number")
}

pub async fn post_comment(repo: &str, issue_num: u64, body: &str) -> Result<()> {
    let num = issue_num.to_string();
    run_gh(&["issue", "comment", &num, "--repo", repo, "--body", body]).await?;
    Ok(())
}

pub async fn list_prs_for_issue(repo: &str, issue_num: u64) -> Result<Vec<u64>> {
    let jq = format!(r##".[] | select(.body | test("#{issue_num}")) | .number"##);
    let out = run_gh(&[
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "all",
        "--json",
        "number,body",
        "-q",
        &jq,
    ])
    .await
    .unwrap_or_default();

    let nums = out
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .collect();
    Ok(nums)
}

pub async fn get_issue(repo: &str, issue_num: u64) -> Result<(String, String)> {
    let n = issue_num.to_string();
    let title = run_gh(&[
        "issue", "view", &n, "--repo", repo, "--json", "title", "-q", ".title",
    ])
    .await?;
    let body = run_gh(&[
        "issue", "view", &n, "--repo", repo, "--json", "body", "-q", ".body",
    ])
    .await?;
    Ok((title.trim().to_string(), body.trim().to_string()))
}

pub async fn get_issue_state(repo: &str, issue_num: u64) -> Result<String> {
    let num = issue_num.to_string();
    let out = run_gh(&[
        "issue", "view", &num, "--repo", repo, "--json", "state", "-q", ".state",
    ])
    .await?;
    Ok(out.trim().to_string())
}

pub async fn merged_prs_since(repo: &str, since: &str) -> Result<Vec<(u64, String)>> {
    let jq = format!(r##".[] | select(.mergedAt > "{since}") | "#\(.number) \(.title)""##);
    let out = run_gh(&[
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "merged",
        "--json",
        "number,title,mergedAt",
        "-q",
        &jq,
    ])
    .await
    .unwrap_or_default();

    let mut result = Vec::new();
    for line in out.lines() {
        if let Some(rest) = line.trim().strip_prefix('#') {
            let mut parts = rest.splitn(2, ' ');
            if let (Some(num_str), Some(title)) = (parts.next(), parts.next()) {
                if let Ok(num) = num_str.parse::<u64>() {
                    result.push((num, title.to_string()));
                }
            }
        }
    }
    Ok(result)
}

pub struct PrInfo {
    /// CLEAN | BEHIND | BLOCKED | DIRTY | UNKNOWN
    pub merge_state: String,
}

/// Get the merge-state of an open PR.
/// `head_branch` is used to detect when GitHub reports UNSTABLE but the branch
/// is actually behind main (diverged), in which case we return BEHIND instead.
pub async fn get_pr_info(repo: &str, pr_num: u64, head_branch: &str) -> Result<PrInfo> {
    let num = pr_num.to_string();
    let out = run_gh(&[
        "pr",
        "view",
        &num,
        "--repo",
        repo,
        "--json",
        "mergeStateStatus",
        "-q",
        ".mergeStateStatus",
    ])
    .await?;
    let reported = out.trim().to_string();

    // GitHub may report UNSTABLE even when the branch is diverged from main.
    // In that case a merge attempt fails with "out of date", so we check the
    // compare endpoint and override to BEHIND when the branch is actually behind.
    let merge_state = if reported == "UNSTABLE" && !head_branch.is_empty() {
        let compare = run_gh(&[
            "api",
            &format!("repos/{repo}/compare/main...{head_branch}"),
            "-q",
            ".behind_by",
        ])
        .await
        .unwrap_or_default();
        let behind_by: u64 = compare.trim().parse().unwrap_or(0);
        if behind_by > 0 {
            "BEHIND".to_string()
        } else {
            reported
        }
    } else {
        reported
    };

    Ok(PrInfo { merge_state })
}

/// Fetch review comments, unresolved threads, and CI check status for a BLOCKED PR.
pub async fn get_pr_review_context(repo: &str, pr_num: u64) -> Result<String> {
    let num = pr_num.to_string();
    let jq = r#""URL: " + .url +
        "\n\nReviews (" + (.reviews | length | tostring) + "):\n" +
        (.reviews | map("  [" + .state + "] " + .author.login + ": " + (.body // "(no comment)")) | join("\n")) +
        "\n\nUnresolved threads:\n" +
        ([.reviewThreads[] | select(.isResolved == false) | .comments[0] | "  " + .author.login + ": " + .body] | join("\n")) +
        "\n\nCI checks (non-passing):\n" +
        (.statusCheckRollup | map(select(.conclusion != "SUCCESS" and .conclusion != null and .conclusion != "NEUTRAL" and .conclusion != "SKIPPED")) | map("  " + .name + ": " + .conclusion) | join("\n"))"#;
    let view = run_gh(&[
        "pr",
        "view",
        &num,
        "--repo",
        repo,
        "--json",
        "url,reviews,reviewThreads,statusCheckRollup",
        "-q",
        jq,
    ])
    .await
    .unwrap_or_else(|_| "(could not fetch PR details)".to_string());
    Ok(view)
}

pub async fn list_open_prs_with_titles(repo: &str) -> Result<Vec<(u64, String, String)>> {
    let out = run_gh(&[
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "open",
        "--json",
        "number,headRefName,title",
        "-q",
        r#".[] | "\(.number)\t\(.headRefName)\t\(.title)""#,
    ])
    .await
    .unwrap_or_default();

    let mut result = Vec::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() >= 3 {
            if let Ok(num) = parts[0].parse::<u64>() {
                result.push((num, parts[1].to_string(), parts[2].to_string()));
            }
        }
    }
    Ok(result)
}

pub async fn list_open_prs(repo: &str) -> Result<Vec<(u64, String)>> {
    let out = run_gh(&[
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "open",
        "--json",
        "number,headRefName",
        "-q",
        r#".[] | "\(.number) \(.headRefName)""#,
    ])
    .await
    .unwrap_or_default();

    let mut result = Vec::new();
    for line in out.lines() {
        let mut parts = line.trim().splitn(2, ' ');
        if let (Some(num_str), Some(branch)) = (parts.next(), parts.next()) {
            if let Ok(num) = num_str.parse::<u64>() {
                result.push((num, branch.to_string()));
            }
        }
    }
    Ok(result)
}

/// Return the set of recently merged PR numbers (up to 100).
pub async fn list_merged_pr_numbers(repo: &str) -> Result<Vec<u64>> {
    let out = run_gh(&[
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "merged",
        "--limit",
        "100",
        "--json",
        "number",
        "-q",
        ".[].number",
    ])
    .await
    .unwrap_or_default();
    Ok(out
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .collect())
}

pub async fn merge_pr(repo: &str, pr_num: u64) -> Result<()> {
    let num = pr_num.to_string();
    run_gh(&[
        "pr",
        "merge",
        &num,
        "--repo",
        repo,
        "--squash",
        "--delete-branch",
    ])
    .await?;
    Ok(())
}

/// Get the latest review state for a PR: "APPROVED", "CHANGES_REQUESTED", or None.
pub async fn get_latest_review_state(repo: &str, pr_num: u64) -> Result<Option<String>> {
    let num = pr_num.to_string();
    let out = run_gh(&[
        "pr",
        "view",
        &num,
        "--repo",
        repo,
        "--json",
        "reviews",
        "-q",
        "[.reviews[] | select(.state == \"APPROVED\" or .state == \"CHANGES_REQUESTED\")] | last | .state",
    ])
    .await
    .unwrap_or_default();
    let state = out.trim().to_string();
    if state.is_empty() || state == "null" {
        Ok(None)
    } else {
        Ok(Some(state))
    }
}

pub async fn invoke_claude(prompt: &str) -> Result<String> {
    let out = Command::new("claude")
        .args(["--dangerously-skip-permissions", "--print", prompt])
        .env_remove("CLAUDECODE")
        .output()
        .await
        .context("Failed to spawn claude")?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        bail!("{}{}", stdout, stderr);
    }
    Ok(stdout)
}
