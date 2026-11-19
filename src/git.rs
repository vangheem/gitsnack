//! Read-only git. On gitsnack's managed clones everything is relative to `origin/*`, so the
//! clone's own checkout (which the agent may move around) never affects what is shown. Local
//! working trees (`gitsnack diff`) show their uncommitted changes.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

use crate::model::CommitSummary;

pub fn fetch(dir: &Path) -> Result<()> {
    git_ok(dir, &["fetch", "--prune", "--quiet", "origin"])?;
    Ok(())
}

pub fn default_branch(dir: &Path) -> String {
    if let Ok(text) = git_ok(
        dir,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) && let Some((_, rest)) = text.trim().split_once('/')
    {
        return rest.to_string();
    }
    for name in ["main", "master"] {
        if git_ok(
            dir,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/remotes/origin/{name}"),
            ],
        )
        .is_ok()
        {
            return name.to_string();
        }
    }
    "main".to_string()
}

/// Remote branches other than the default one, most recently committed first.
pub fn list_branches(dir: &Path, default: &str) -> Result<Vec<String>> {
    let text = git_ok(
        dir,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/remotes/origin",
        ],
    )?;
    Ok(text
        .lines()
        .filter_map(|l| l.strip_prefix("origin/"))
        .filter(|b| *b != "HEAD" && *b != default && !b.is_empty())
        .map(str::to_string)
        .collect())
}

pub fn list_commits(dir: &Path, default: &str, limit: usize) -> Result<Vec<CommitSummary>> {
    log(dir, &format!("origin/{default}"), limit)
}

/// A local working tree's commits on its branch: those since it left the default branch, or
/// its newest few when it hasn't (on the default branch itself, or with no remote).
pub fn branch_commits(dir: &Path) -> Result<Vec<CommitSummary>> {
    if git_ok(dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() {
        return Ok(Vec::new());
    }
    let base = git_ok(
        dir,
        &[
            "merge-base",
            "HEAD",
            &format!("origin/{}", default_branch(dir)),
        ],
    )
    .map(|b| b.trim().to_string());
    let head = git_ok(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    match base {
        Ok(base) if base != head => log(dir, &format!("{base}..HEAD"), 200),
        _ => log(dir, "HEAD", 20),
    }
}

fn log(dir: &Path, rev: &str, limit: usize) -> Result<Vec<CommitSummary>> {
    let n = limit.clamp(1, 500).to_string();
    let text = git_ok(
        dir,
        &[
            "log",
            "-n",
            &n,
            "--pretty=format:%H%x1f%h%x1f%an%x1f%ad%x1f%s",
            "--date=short",
            rev,
        ],
    )?;
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

/// One commit's summary and full message body.
pub fn commit_info(dir: &Path, sha: &str) -> Result<(CommitSummary, String)> {
    let text = git_ok(
        dir,
        &[
            "show",
            "-s",
            "--format=%H%x1f%h%x1f%an%x1f%ad%x1f%s%x1f%b",
            "--date=short",
            sha,
        ],
    )?;
    let p: Vec<&str> = text.splitn(6, '\u{1f}').collect();
    if p.len() < 6 {
        anyhow::bail!("unexpected git show output for {sha}");
    }
    let summary = CommitSummary {
        sha: p[0].into(),
        short_sha: p[1].into(),
        author: p[2].into(),
        date: p[3].into(),
        subject: p[4].into(),
    };
    Ok((summary, p[5].trim().to_string()))
}

pub fn branch_diff(dir: &Path, default: &str, branch: &str) -> Result<String> {
    range_diff(
        dir,
        &format!("origin/{default}"),
        &format!("origin/{branch}"),
    )
}

/// A PR's diff from the local clone: fetches GitHub's `refs/pull/<n>/head` (forks included)
/// and the base branch, then diffs from their merge base, as GitHub's "Files changed" does.
/// Unlike GitHub's diff API this has no size limit.
pub fn pr_diff(dir: &Path, number: u64, base: &str) -> Result<String> {
    let pr_ref = format!("refs/remotes/origin/pr/{number}");
    let base_ref = format!("refs/remotes/origin/{base}");
    git_ok(
        dir,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "origin",
            &format!("+refs/pull/{number}/head:{pr_ref}"),
            &format!("+refs/heads/{base}:{base_ref}"),
        ],
    )?;
    range_diff(dir, &base_ref, &pr_ref)
}

pub fn range_diff(dir: &Path, base: &str, head: &str) -> Result<String> {
    git_ok(
        dir,
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "-U3",
            &format!("{base}...{head}"),
        ],
    )
}

pub fn commit_diff(dir: &Path, sha: &str) -> Result<String> {
    git_ok(
        dir,
        &[
            "show",
            "--no-ext-diff",
            "--no-color",
            "--format=",
            "--find-renames",
            "-U3",
            sha,
        ],
    )
}

/// The root of the working tree containing `dir`.
pub fn toplevel(dir: &Path) -> Result<String> {
    Ok(git_ok(dir, &["rev-parse", "--show-toplevel"])?
        .trim()
        .to_string())
}

/// A working tree's uncommitted changes, staged or not, against HEAD, plus untracked files.
pub fn local_diff(dir: &Path) -> Result<String> {
    // An unborn branch has no HEAD: diff against the empty tree instead.
    let base = if git_ok(dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
        "HEAD"
    } else {
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
    };
    let mut patch = git_ok(
        dir,
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "-U3",
            base,
        ],
    )?;
    let untracked = git_ok(dir, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in untracked.split('\0').filter(|p| !p.is_empty()) {
        // `--no-index` exits 1 when the files differ, which they always do here.
        let out = git(
            dir,
            &[
                "diff",
                "--no-ext-diff",
                "--no-color",
                "--no-index",
                "--",
                "/dev/null",
                path,
            ],
        )?;
        patch.push_str(&String::from_utf8_lossy(&out.stdout));
    }
    Ok(patch)
}

fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()?)
}

fn git_ok(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git(dir, args)?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_diff_includes_untracked_files() {
        let dir = std::env::temp_dir().join(format!("gitsnack-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git_ok(&dir, &["init", "-q"]).unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git_ok(&dir, &["add", "a.txt"]).unwrap();
        git_ok(
            &dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "a",
            ],
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        let patch = local_diff(&dir).unwrap();
        let commits = branch_commits(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].subject, "a");
        assert!(patch.contains("+two"), "{patch}");
        assert!(patch.contains("+++ b/b.txt"), "{patch}");
    }
}
