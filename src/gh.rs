//! GitHub through the `gh` CLI, always scoped to one repo with `-R owner/name`.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::Value;

use crate::model::{
    CheckResult, Comment, MergeMethod, MergeReadiness, PrDetail, PullRequest, ReviewKind,
    ReviewState,
};
use crate::store::Draft;

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
            "pr",
            "list",
            "-R",
            slug,
            "--state",
            "open",
            "--limit",
            "100",
            "--json",
            "number,title,author,baseRefName,headRefName,url,isDraft,reviewDecision,latestReviews,additions,deletions,updatedAt",
        ],
        None,
    )?;
    let items: Vec<GhPullRequest> = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow!("failed to parse gh pr list: {e}"))?;
    Ok(items.into_iter().map(PullRequest::from).collect())
}

pub fn pr_diff(slug: &str, number: u64) -> Result<String> {
    let n = number.to_string();
    let out = gh(&["pr", "diff", &n, "-R", slug], None)?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn pr_detail(slug: &str, number: u64) -> Result<PrDetail> {
    let n = number.to_string();
    let out = gh(
        &[
            "pr",
            "view",
            &n,
            "-R",
            slug,
            "--json",
            "body,statusCheckRollup,headRefOid,comments,reviews",
        ],
        None,
    )?;
    let v: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow!("failed to parse gh pr view: {e}"))?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let login = |v: &Value, k: &str| {
        v.get(k)
            .and_then(|a| a.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("ghost")
            .to_string()
    };
    let mut comments: Vec<Comment> = Vec::new();
    for c in v["comments"].as_array().into_iter().flatten() {
        comments.push(Comment {
            author: login(c, "author"),
            body: s(c, "body"),
            created: s(c, "createdAt"),
            review_state: None,
            path: None,
            line: None,
            left: false,
            reply: false,
        });
    }
    for r in v["reviews"].as_array().into_iter().flatten() {
        let state = s(r, "state");
        // Reviews that are only a container for inline comments add nothing on their own.
        if state == "COMMENTED" && s(r, "body").trim().is_empty() || state == "PENDING" {
            continue;
        }
        comments.push(Comment {
            author: login(r, "author"),
            body: s(r, "body"),
            created: s(r, "submittedAt"),
            review_state: Some(state),
            path: None,
            line: None,
            left: false,
            reply: false,
        });
    }
    let path = format!("repos/{slug}/pulls/{number}/comments?per_page=100");
    let inline = gh(&["api", &path, "--paginate", "--slurp"], None)?;
    let pages: Value = serde_json::from_slice(&inline.stdout)
        .map_err(|e| anyhow!("failed to parse review comments: {e}"))?;
    for c in pages
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p.as_array().into_iter().flatten())
    {
        let line = c["line"].as_u64().or_else(|| c["original_line"].as_u64());
        comments.push(Comment {
            author: login(c, "user"),
            body: s(c, "body"),
            created: s(c, "created_at"),
            review_state: None,
            path: Some(s(c, "path")),
            line: line.map(|l| l as u32),
            left: s(c, "side") == "LEFT",
            reply: !c["in_reply_to_id"].is_null(),
        });
    }
    comments.sort_by(|a, b| a.created.cmp(&b.created));
    Ok(PrDetail {
        body: s(&v, "body"),
        checks: checks(&v),
        head_sha: s(&v, "headRefOid"),
        comments,
    })
}

/// Posts a conversation comment and returns it as GitHub stored it.
pub fn add_comment(slug: &str, number: u64, body: &str) -> Result<Comment> {
    let api = format!("repos/{slug}/issues/{number}/comments");
    let payload = serde_json::json!({ "body": body }).to_string();
    let out = gh(
        &["api", &api, "--method", "POST", "--input", "-"],
        Some(&payload),
    )?;
    rest_comment(&serde_json::from_slice(&out.stdout)?)
}

/// A single inline comment on `path`, spanning `start..=end` on one side of the diff.
#[allow(clippy::too_many_arguments)]
pub fn add_line_comment(
    slug: &str,
    number: u64,
    commit: &str,
    path: &str,
    start: u32,
    end: u32,
    left: bool,
    body: &str,
) -> Result<Comment> {
    let side = if left { "LEFT" } else { "RIGHT" };
    let mut payload = serde_json::json!({ "body": body, "commit_id": commit, "path": path, "line": end, "side": side });
    if start != end {
        payload["start_line"] = start.into();
        payload["start_side"] = side.into();
    }
    let api = format!("repos/{slug}/pulls/{number}/comments");
    let out = gh(
        &["api", &api, "--method", "POST", "--input", "-"],
        Some(&payload.to_string()),
    )?;
    rest_comment(&serde_json::from_slice(&out.stdout)?)
}

/// A comment from the REST API (issue comments and pull request review comments share this).
fn rest_comment(c: &Value) -> Result<Comment> {
    let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    if c.get("id").is_none() {
        return Err(anyhow!("unexpected comment response from GitHub"));
    }
    let path = c.get("path").and_then(Value::as_str).map(str::to_string);
    Ok(Comment {
        author: c["user"]["login"].as_str().unwrap_or("you").to_string(),
        body: s("body"),
        created: s("created_at"),
        review_state: None,
        line: path
            .as_ref()
            .and(c["line"].as_u64().or_else(|| c["original_line"].as_u64()))
            .map(|l| l as u32),
        left: s("side") == "LEFT",
        reply: !c["in_reply_to_id"].is_null(),
        path,
    })
}

pub fn merge_readiness(slug: &str, number: u64) -> Result<MergeReadiness> {
    let n = number.to_string();
    let out = gh(
        &[
            "pr",
            "view",
            &n,
            "-R",
            slug,
            "--json",
            "reviewDecision,mergeable,mergeStateStatus,statusCheckRollup,isDraft",
        ],
        None,
    )?;
    let v: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow!("failed to parse gh pr view: {e}"))?;
    let repo_out = gh(
        &[
            "repo",
            "view",
            slug,
            "--json",
            "mergeCommitAllowed,squashMergeAllowed,rebaseMergeAllowed",
        ],
        None,
    )?;
    let rv: Value = serde_json::from_slice(&repo_out.stdout)
        .map_err(|e| anyhow!("failed to parse gh repo view: {e}"))?;
    let method = MergeMethod::pick(
        flag(&rv, "squashMergeAllowed"),
        flag(&rv, "mergeCommitAllowed"),
        flag(&rv, "rebaseMergeAllowed"),
    );
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

/// One review with its line comments, as GitHub's review API takes it.
pub fn submit_review(
    slug: &str,
    number: u64,
    kind: ReviewKind,
    body: &str,
    commit: &str,
    drafts: &[Draft],
) -> Result<()> {
    let event = match kind {
        ReviewKind::Comment => "COMMENT",
        ReviewKind::Approve => "APPROVE",
        ReviewKind::RequestChanges => "REQUEST_CHANGES",
    };
    let comments: Vec<Value> = drafts
        .iter()
        .map(|d| {
            let side = if d.left { "LEFT" } else { "RIGHT" };
            let mut c =
                serde_json::json!({ "path": d.path, "line": d.end, "side": side, "body": d.body });
            if d.start != d.end {
                c["start_line"] = d.start.into();
                c["start_side"] = side.into();
            }
            c
        })
        .collect();
    let mut payload = serde_json::json!({ "event": event, "comments": comments });
    if !body.is_empty() {
        payload["body"] = body.into();
    }
    if !commit.is_empty() {
        payload["commit_id"] = commit.into();
    }
    let api = format!("repos/{slug}/pulls/{number}/reviews");
    gh(
        &["api", &api, "--method", "POST", "--input", "-"],
        Some(&payload.to_string()),
    )?;
    Ok(())
}

fn flag(v: &Value, name: &str) -> bool {
    v.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn checks(v: &Value) -> Vec<(String, CheckResult)> {
    v.get("statusCheckRollup")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(check_line).collect())
        .unwrap_or_default()
}

fn check_line(v: &Value) -> (String, CheckResult) {
    let name = v
        .get("name")
        .or_else(|| v.get("context"))
        .and_then(Value::as_str)
        .unwrap_or("check")
        .to_string();
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
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            anyhow!("gh cli not found")
        } else {
            e.into()
        }
    })?;
    if let (Some(body), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(body.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(anyhow!(
            "gh {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
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
            author: i
                .author
                .map(|a| a.login)
                .unwrap_or_else(|| "unknown".into()),
            base_ref: i.base_ref_name,
            head_ref: i.head_ref_name,
            url: i.url,
            is_draft: i.is_draft,
            review: ReviewState::from_reviews(
                &i.review_decision,
                i.latest_reviews.iter().map(|r| r.state.as_str()),
            ),
            additions: i.additions,
            deletions: i.deletions,
            updated: i.updated_at.get(..10).unwrap_or("").to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hits GitHub: `cargo test -- --ignored`. A posted comment, read back through the REST
    /// shape the POST returns, matches what `pr_detail` later lists, so it isn't shown twice.
    #[test]
    #[ignore]
    fn rest_comments_match_listed_ones() {
        let out = gh(&["api", "repos/sharkdp/hexyl/pulls/300/comments"], None).unwrap();
        let rest: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
        let first = rest_comment(&rest[0]).unwrap();
        let listed = pr_detail("sharkdp/hexyl", 300).unwrap();
        assert!(listed.comments.iter().any(|c| c.author == first.author
            && c.body == first.body
            && c.created == first.created));
        assert!(first.path.is_some() && first.line.is_some());
    }
}
