//! Read-only git on gitsnack's managed clones. Everything is relative to `origin/*`, so the
//! clone's own checkout (which the agent may move around) never affects what is shown.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

use crate::model::CommitSummary;

pub fn fetch(dir: &Path) -> Result<()> {
    git_ok(dir, &["fetch", "--prune", "--quiet", "origin"])?;
    Ok(())
}

pub fn default_branch(dir: &Path) -> String {
    if let Ok(text) = git_ok(dir, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"])
        && let Some((_, rest)) = text.trim().split_once('/') {
            return rest.to_string();
        }
    for name in ["main", "master"] {
        if git_ok(dir, &["show-ref", "--verify", "--quiet", &format!("refs/remotes/origin/{name}")]).is_ok() {
            return name.to_string();
        }
    }
    "main".to_string()
}

/// Remote branches other than the default one, most recently committed first.
pub fn list_branches(dir: &Path, default: &str) -> Result<Vec<String>> {
    let text = git_ok(dir, &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/remotes/origin"])?;
    Ok(text
        .lines()
        .filter_map(|l| l.strip_prefix("origin/"))
        .filter(|b| *b != "HEAD" && *b != default && !b.is_empty())
        .map(str::to_string)
        .collect())
}

pub fn list_commits(dir: &Path, default: &str, limit: usize) -> Result<Vec<CommitSummary>> {
    let n = limit.clamp(1, 500).to_string();
    let rev = format!("origin/{default}");
    let text = git_ok(dir, &["log", "-n", &n, "--pretty=format:%H%x1f%h%x1f%an%x1f%ad%x1f%s", "--date=short", &rev])?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let p: Vec<&str> = line.split('\u{1f}').collect();
            (p.len() >= 5).then(|| CommitSummary {
                sha: p[0].into(),
                short_sha: p[1].into(),
                author: p[2].into(),
                date: p[3].into(),
                subject: p[4..].join(" "),
            })
        })
        .collect())
}

pub fn branch_diff(dir: &Path, default: &str, branch: &str) -> Result<String> {
    range_diff(dir, &format!("origin/{default}"), &format!("origin/{branch}"))
}

pub fn range_diff(dir: &Path, base: &str, head: &str) -> Result<String> {
    git_ok(dir, &["diff", "--no-ext-diff", "--no-color", "--find-renames", "-U3", &format!("{base}...{head}")])
}

pub fn commit_diff(dir: &Path, sha: &str) -> Result<String> {
    git_ok(dir, &["show", "--no-ext-diff", "--no-color", "--format=", "--find-renames", "-U3", sha])
}

fn git_ok(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
