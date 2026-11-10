//! Data shared by the git, gh and UI layers. Ported from gitcrack's model.

#[derive(Debug, Clone, PartialEq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub base_ref: String,
    pub head_ref: String,
    pub url: String,
    pub is_draft: bool,
    pub review: ReviewState,
    pub additions: u64,
    pub deletions: u64,
    pub updated: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrDetail {
    pub body: String,
    pub checks: Vec<(String, CheckResult)>,
}

#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub sha: String,
    pub short_sha: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Reviewed,
    Unreviewed,
}

impl ReviewState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::ChangesRequested => "changes requested",
            Self::Reviewed => "reviewed",
            Self::Unreviewed => "unreviewed",
        }
    }

    pub fn from_reviews<'a>(decision: &str, states: impl IntoIterator<Item = &'a str>) -> Self {
        match decision {
            "APPROVED" => return Self::Approved,
            "CHANGES_REQUESTED" => return Self::ChangesRequested,
            _ => {}
        }
        let mut commented = false;
        for state in states {
            match state {
                "APPROVED" => return Self::Approved,
                "CHANGES_REQUESTED" => return Self::ChangesRequested,
                "COMMENTED" => commented = true,
                _ => {}
            }
        }
        if commented { Self::Reviewed } else { Self::Unreviewed }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewKind {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewKind {
    pub fn flag(self) -> &'static str {
        match self {
            Self::Comment => "--comment",
            Self::Approve => "--approve",
            Self::RequestChanges => "--request-changes",
        }
    }

    pub fn needs_body(self) -> bool {
        !matches!(self, Self::Approve)
    }

    pub fn submitted(self) -> &'static str {
        match self {
            Self::Comment => "Comment posted",
            Self::Approve => "Approved",
            Self::RequestChanges => "Changes requested",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    Pass,
    Fail,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::Squash => "squash",
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        }
    }

    pub fn flag(self) -> &'static str {
        match self {
            Self::Squash => "--squash",
            Self::Merge => "--merge",
            Self::Rebase => "--rebase",
        }
    }

    pub fn pick(squash: bool, merge: bool, rebase: bool) -> Option<Self> {
        if squash {
            Some(Self::Squash)
        } else if merge {
            Some(Self::Merge)
        } else if rebase {
            Some(Self::Rebase)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeReadiness {
    pub approved: bool,
    pub ci_ok: bool,
    pub ci_label: String,
    pub merge_ok: bool,
    pub merge_label: String,
    pub method: Option<MergeMethod>,
    pub problems: Vec<String>,
}

impl MergeReadiness {
    pub fn can_merge(&self) -> bool {
        self.approved && self.ci_ok && self.merge_ok && self.method.is_some()
    }

    pub fn assess(
        approved: bool,
        mergeable: &str,
        merge_state: &str,
        draft: bool,
        checks: &[(String, CheckResult)],
        method: Option<MergeMethod>,
    ) -> Self {
        let failing: Vec<_> =
            checks.iter().filter(|(_, r)| *r == CheckResult::Fail).map(|(n, _)| format!("{n} failing")).collect();
        let pending: Vec<_> =
            checks.iter().filter(|(_, r)| *r == CheckResult::Pending).map(|(n, _)| format!("{n} pending")).collect();
        let ci_ok = failing.is_empty() && pending.is_empty();
        let ci_label = if checks.is_empty() {
            "no checks".to_string()
        } else if !failing.is_empty() {
            format!("failing ({})", failing.len())
        } else if !pending.is_empty() {
            format!("pending ({})", pending.len())
        } else {
            "passing".to_string()
        };
        let merge_label = merge_block(mergeable, merge_state, draft, method).unwrap_or_else(|| "allowed".to_string());
        let merge_ok = merge_label == "allowed";
        let mut problems = Vec::new();
        if !approved {
            problems.push("not approved".to_string());
        }
        problems.extend(failing);
        problems.extend(pending);
        problems.truncate(6);
        Self { approved, ci_ok, ci_label, merge_ok, merge_label, method, problems }
    }
}

fn merge_block(mergeable: &str, merge_state: &str, draft: bool, method: Option<MergeMethod>) -> Option<String> {
    if draft || merge_state == "DRAFT" {
        return Some("draft".to_string());
    }
    if mergeable == "CONFLICTING" || merge_state == "DIRTY" {
        return Some("conflicts".to_string());
    }
    if merge_state == "BEHIND" {
        return Some("behind base".to_string());
    }
    if merge_state == "BLOCKED" {
        return Some("blocked by branch protection".to_string());
    }
    if mergeable == "UNKNOWN" || merge_state == "UNKNOWN" {
        return Some("mergeability unknown".to_string());
    }
    if method.is_none() {
        return Some("no merge method allowed".to_string());
    }
    None
}
