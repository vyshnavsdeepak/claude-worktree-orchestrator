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

pub async fn get_pr_info(repo: &str, pr_num: u64) -> Result<PrInfo> {
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
    Ok(PrInfo {
        merge_state: out.trim().to_string(),
    })
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
