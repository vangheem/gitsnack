//! GitHub through the `gh` CLI, always scoped to one repo with `-R owner/name`.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::Value;

use crate::model::{CheckResult, MergeMethod, MergeReadiness, PrDetail, PullRequest, ReviewKind, ReviewState};

pub fn clone(slug: &str, dir: &Path) -> Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dir = dir.to_string_lossy();
    gh(&["repo", "clone", slug, &dir], None)?;
    Ok(())
}

pub fn list_prs(slug: &str) -> Result<Vec<PullRequest>> {
    let out = gh(
        &[
            "pr", "list", "-R", slug, "--state", "open", "--limit", "100", "--json",
            "number,title,author,baseRefName,headRefName,url,isDraft,reviewDecision,latestReviews,additions,deletions,updatedAt",
        ],
        None,
    )?;
    let items: Vec<GhPullRequest> =
        serde_json::from_slice(&out.stdout).map_err(|e| anyhow!("failed to parse gh pr list: {e}"))?;
    Ok(items.into_iter().map(PullRequest::from).collect())
}

pub fn pr_diff(slug: &str, number: u64) -> Result<String> {
    let n = number.to_string();
    let out = gh(&["pr", "diff", &n, "-R", slug], None)?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn pr_detail(slug: &str, number: u64) -> Result<PrDetail> {
    let n = number.to_string();
    let out = gh(&["pr", "view", &n, "-R", slug, "--json", "body,statusCheckRollup"], None)?;
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| anyhow!("failed to parse gh pr view: {e}"))?;
    Ok(PrDetail {
        body: v.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
        checks: checks(&v),
    })
}

pub fn merge_readiness(slug: &str, number: u64) -> Result<MergeReadiness> {
    let n = number.to_string();
    let out = gh(
        &["pr", "view", &n, "-R", slug, "--json", "reviewDecision,mergeable,mergeStateStatus,statusCheckRollup,isDraft"],
        None,
    )?;
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| anyhow!("failed to parse gh pr view: {e}"))?;
    let repo_out =
        gh(&["repo", "view", slug, "--json", "mergeCommitAllowed,squashMergeAllowed,rebaseMergeAllowed"], None)?;
    let rv: Value =
        serde_json::from_slice(&repo_out.stdout).map_err(|e| anyhow!("failed to parse gh repo view: {e}"))?;
    let method = MergeMethod::pick(flag(&rv, "squashMergeAllowed"), flag(&rv, "mergeCommitAllowed"), flag(&rv, "rebaseMergeAllowed"));
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(MergeReadiness::assess(
        s("reviewDecision") == "APPROVED",
        &s("mergeable"),
        &s("mergeStateStatus"),
        v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        &checks(&v),
        method,
    ))
}

pub fn set_draft(slug: &str, number: u64, draft: bool) -> Result<()> {
    let n = number.to_string();
    let mut args = vec!["pr", "ready", n.as_str(), "-R", slug];
    if draft {
        args.push("--undo");
    }
    gh(&args, None)?;
    Ok(())
}

pub fn merge_pr(slug: &str, number: u64, method: MergeMethod) -> Result<()> {
    let n = number.to_string();
    gh(&["pr", "merge", &n, "-R", slug, method.flag()], None)?;
    Ok(())
}

pub fn review_pr(slug: &str, number: u64, kind: ReviewKind, body: &str) -> Result<()> {
    let n = number.to_string();
    let mut args = vec!["pr", "review", n.as_str(), "-R", slug, kind.flag()];
    if !body.is_empty() {
        args.extend(["--body-file", "-"]);
    }
    gh(&args, (!body.is_empty()).then_some(body))?;
    Ok(())
}

fn flag(v: &Value, name: &str) -> bool {
    v.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn checks(v: &Value) -> Vec<(String, CheckResult)> {
    v.get("statusCheckRollup").and_then(Value::as_array).map(|items| items.iter().map(check_line).collect()).unwrap_or_default()
}

fn check_line(v: &Value) -> (String, CheckResult) {
    let name = v.get("name").or_else(|| v.get("context")).and_then(Value::as_str).unwrap_or("check").to_string();
    let get = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    let (status, conclusion, state) = (get("status"), get("conclusion"), get("state"));
    let result = if !conclusion.is_empty() {
        match conclusion {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => CheckResult::Pass,
            _ if status != "COMPLETED" => CheckResult::Pending,
            _ => CheckResult::Fail,
        }
    } else if !state.is_empty() {
        match state {
            "SUCCESS" => CheckResult::Pass,
            "PENDING" | "EXPECTED" => CheckResult::Pending,
            _ => CheckResult::Fail,
        }
    } else if status == "COMPLETED" {
        CheckResult::Pass
    } else {
        CheckResult::Pending
    };
    (name, result)
}

fn gh(args: &[&str], stdin: Option<&str>) -> Result<Output> {
    let mut cmd = Command::new("gh");
    cmd.args(args)
        .env("GH_FORCE_TTY", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env("GIT_PAGER", "cat")
        .env("GH_EDITOR", ":")
        .env("GIT_EDITOR", ":")
        .env("EDITOR", ":")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound { anyhow!("gh cli not found") } else { e.into() }
    })?;
    if let (Some(body), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(body.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(anyhow!("gh {} failed: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPullRequest {
    number: u64,
    title: String,
    #[serde(default)]
    author: Option<GhAuthor>,
    base_ref_name: String,
    head_ref_name: String,
    url: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    review_decision: String,
    #[serde(default)]
    latest_reviews: Vec<GhReview>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(default)]
    updated_at: String,
}

#[derive(Deserialize)]
struct GhReview {
    #[serde(default)]
    state: String,
}

#[derive(Deserialize)]
struct GhAuthor {
    login: String,
}

impl From<GhPullRequest> for PullRequest {
    fn from(i: GhPullRequest) -> Self {
        Self {
            number: i.number,
            title: i.title,
            author: i.author.map(|a| a.login).unwrap_or_else(|| "unknown".into()),
            base_ref: i.base_ref_name,
            head_ref: i.head_ref_name,
            url: i.url,
            is_draft: i.is_draft,
            review: ReviewState::from_reviews(&i.review_decision, i.latest_reviews.iter().map(|r| r.state.as_str())),
            additions: i.additions,
            deletions: i.deletions,
            updated: i.updated_at.get(..10).unwrap_or("").to_string(),
        }
    }
}
