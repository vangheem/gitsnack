//! gitsnack: tracked repos and their PRs on the left, the diff in the middle, and an
//! opencode agent (or the review composer) on the right.

mod appbundle;
mod gh;
mod git;
mod model;
mod opencode;
mod store;
mod update;

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use rune::{
    AppSpec, Chrome, Cx, El, KeyEvent, ScrollSpec, TextEdit, Tx, Waker, col, el, row, text,
};
use rune_kit::diff::{
    Annotation, DiffMode, DiffState, DiffView, FileDiff, LineRange, LineRef, Side, parse,
};
use rune_kit::shell::{
    self, PaneState, Sidebar, SidebarRow, SidebarSection, SidebarToggle, SplitView, Toolbar,
};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, Checkbox, Combobox, ComboboxOption, ComboboxState,
    ControlSize, Dialog, FileNode, FileTree, IconButton, IconButtonVariant, Kit, Markdown,
    SegmentedControl, Spinner, TextArea, TextInput, Tree, TreeItem, TreeModel,
};

use model::{
    Check, CheckResult, Comment, CommitSummary, MergeReadiness, PrDetail, PullRequest, ReviewKind,
    ReviewState,
};
use opencode::{Client, Entry, Snapshot};
use store::{Draft, ModelRef, Store};

/// A target's patch, its PR or commit detail, and a commit's summary and message.
type Loaded = (String, Option<PrDetail>, Option<(CommitSummary, String)>);
type OnRow = Box<dyn Fn(&mut App, &mut Tx)>;
type Job = Box<dyn FnOnce(&mut App, &mut Tx) + Send>;

#[derive(Clone, PartialEq)]
enum Target {
    Pr(String, u64),
    Branch(String, String),
    Commit(String, String),
    Range(String, String, String),
}

#[derive(Clone, Copy, PartialEq)]
enum Browse {
    Branches,
    Commits,
}

#[derive(Default)]
struct Repo {
    prs: Vec<PullRequest>,
    branches: Vec<String>,
    commits: Vec<CommitSummary>,
    default_branch: String,
    /// Combined CI state of the default branch's newest commits, by SHA.
    ci: HashMap<String, CheckResult>,
    loading: bool,
    error: Option<String>,
    collapsed: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Agent,
    Comments,
    Review,
}

enum Confirm {
    Merge(Option<MergeReadiness>),
    Draft(bool),
    Checks,
    InstallApp,
    Update,
}

struct App {
    store: Store,
    jobs: (Sender<Job>, Receiver<Job>),
    waker: Option<Waker>,
    oc: Option<Client>,
    oc_error: Option<String>,
    oc_connecting: bool,
    sidebar: PaneState,
    inspector: PaneState,
    repos: BTreeMap<String, Repo>,
    browse: Option<(String, Browse)>,
    range_base: Option<String>,
    target: Option<Target>,
    diff: DiffState,
    files: TreeModel<FileNode<usize>>,
    diff_loading: bool,
    diff_error: Option<String>,
    detail: Option<PrDetail>,
    panel: Panel,
    chats: HashMap<String, Snapshot>,
    chat_inflight: bool,
    next_chat_poll: f64,
    composer: TextEdit,
    /// The open commit's summary and message body.
    commit: Option<(CommitSummary, String)>,
    models: Vec<ComboboxOption>,
    models_loading: bool,
    model_combo: ComboboxState,
    review: TextEdit,
    comment: TextEdit,
    /// The inline composer opened from a diff line's "+".
    composing: Option<(LineRange, TextEdit)>,
    busy: bool,
    confirm: Option<Confirm>,
    add_repo: Option<TextEdit>,
    toast: Option<(String, bool)>,
    toast_until: Option<f64>,
    next_checks_poll: f64,
    /// A newer release's tag, when there is one the user hasn't skipped.
    update: Option<String>,
    /// "Later": hide the update row until the next launch.
    update_hidden: bool,
    updating: bool,
    next_update_check: f64,
    /// Comments posted from here that GitHub's reads may not return yet.
    posted: Vec<Comment>,
    next_repo_refresh: Option<f64>,
}

const REPO_REFRESH_SECS: f64 = 300.0;

const PRESETS: &[(&str, &str)] = &[
    (
        "Review",
        "Review this PR thoroughly. Focus on correctness, design and edge cases. Group findings by severity, with file:line references, and say what you would change.",
    ),
    (
        "Summarize",
        "Summarize what this PR changes and why. Give an architectural overview and point out the critical code paths I should read closely.",
    ),
    (
        "Risks",
        "What could this PR break? Look for regressions, security issues, performance problems, migrations and backwards-compatibility risks.",
    ),
    (
        "Back-compat",
        "Review this PR for backward compatibility only. Check public APIs, wire and file formats, database schemas and migrations, config and CLI flags, defaults, and behavior existing callers rely on. For each break, say who is affected, how bad it is, and how to keep compatibility (shims, deprecation paths, versioning, migrations).",
    ),
    (
        "Tests",
        "Assess how well this change is tested. What is missing, and which cases would you add?",
    ),
];

fn init() -> App {
    let store = Store::load();
    let repos = store
        .repos
        .iter()
        .map(|r| {
            (
                r.clone(),
                Repo {
                    collapsed: true,
                    ..Repo::default()
                },
            )
        })
        .collect();
    App {
        store,
        jobs: channel(),
        waker: None,
        oc: None,
        oc_error: None,
        oc_connecting: false,
        sidebar: PaneState::new(300.0).range(200.0, 520.0),
        inspector: PaneState::new(460.0).range(320.0, 900.0),
        repos,
        browse: None,
        range_base: None,
        target: None,
        diff: DiffState::new(Vec::new()),
        files: TreeModel::new(),
        diff_loading: false,
        diff_error: None,
        detail: None,
        panel: Panel::Agent,
        chats: HashMap::new(),
        chat_inflight: false,
        next_chat_poll: 0.0,
        composer: TextEdit::new(""),
        commit: None,
        models: Vec::new(),
        models_loading: false,
        model_combo: ComboboxState::default(),
        review: TextEdit::new(""),
        comment: TextEdit::new(""),
        composing: None,
        busy: false,
        confirm: None,
        add_repo: None,
        toast: None,
        toast_until: None,
        next_checks_poll: 0.0,
        update: None,
        update_hidden: false,
        updating: false,
        next_update_check: 0.0,
        posted: Vec::new(),
        next_repo_refresh: None,
    }
}

impl App {
    /// Runs `work` on a thread and `done` with its result on the UI thread.
    fn bg<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut App, T, &mut Tx) + Send + 'static,
    ) {
        let (sender, waker) = (self.jobs.0.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let out = work();
            let _ = sender.send(Box::new(move |s: &mut App, tx: &mut Tx| done(s, out, tx)));
            if let Some(w) = waker {
                w();
            }
        });
    }

    fn notify(&mut self, msg: impl Into<String>, error: bool) {
        self.toast = Some((msg.into(), error));
        self.toast_until = None;
    }

    fn save(&mut self) {
        if let Err(e) = self.store.save() {
            self.notify(format!("Couldn't save state: {e}"), true);
        }
    }

    fn slug(&self) -> Option<String> {
        match self.target.as_ref()? {
            Target::Pr(s, _)
            | Target::Branch(s, _)
            | Target::Commit(s, _)
            | Target::Range(s, _, _) => Some(s.clone()),
        }
    }

    fn pr(&self) -> Option<&PullRequest> {
        let Some(Target::Pr(slug, n)) = &self.target else {
            return None;
        };
        self.repos.get(slug)?.prs.iter().find(|p| p.number == *n)
    }

    fn set_diff(&mut self, files: Vec<FileDiff>) {
        let mut tree = FileTree::new().build(
            files
                .iter()
                .enumerate()
                .map(|(i, f)| (f.path().to_string(), i)),
        );
        tree.expand_all();
        self.files = tree;
        self.diff = DiffState::new(files);
    }

    /// Places comment threads, pending review comments and the open composer under their lines.
    /// Ids: `c:1,2` (comment indexes), `d:0` (draft index), `compose`.
    fn annotate(&mut self) {
        let mut by_line: BTreeMap<(usize, bool, u32), Vec<usize>> = BTreeMap::new();
        if let Some(d) = &self.detail {
            for (i, c) in d.comments.iter().enumerate() {
                let (Some(path), Some(line)) = (&c.path, c.line) else {
                    continue;
                };
                if let Some(file) = self.diff.file_index(path) {
                    by_line.entry((file, c.left, line)).or_default().push(i);
                }
            }
        }
        let at =
            |left: bool, line: u32| LineRef::new(if left { Side::Old } else { Side::New }, line);
        let mut notes: Vec<Annotation> = by_line
            .into_iter()
            .map(|((file, left, line), ids)| {
                let id = ids
                    .iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                Annotation::new(file, at(left, line), format!("c:{id}"))
            })
            .collect();
        for (i, d) in self.drafts().iter().enumerate() {
            if let Some(file) = self.diff.file_index(&d.path) {
                notes.push(Annotation::new(file, at(d.left, d.end), format!("d:{i}")));
            }
        }
        if let Some((r, _)) = &self.composing {
            notes.push(Annotation::new(r.file, r.end, "compose"));
        }
        self.diff.set_annotations(notes);
    }

    fn drafts(&self) -> &[Draft] {
        self.pr_key()
            .and_then(|k| self.store.drafts.get(&k))
            .map_or(&[], Vec::as_slice)
    }

    fn open_composer(&mut self, lines: LineRange) {
        let mut edit = TextEdit::new("");
        edit.focused = true;
        self.composing = Some((lines, edit));
        self.annotate();
    }

    fn close_composer(&mut self) {
        self.composing = None;
        self.annotate();
    }

    /// Adds the composer's text to the pending review as a comment on its lines.
    fn add_draft(&mut self) {
        let (Some(key), Some((r, edit))) = (self.pr_key(), self.composing.clone()) else {
            return;
        };
        let body = edit.text.trim().to_string();
        let Some(file) = self.diff.files().get(r.file) else {
            return;
        };
        if body.is_empty() {
            return;
        }
        let (a, b) = (r.start.line.min(r.end.line), r.start.line.max(r.end.line));
        let draft = Draft {
            path: file.path().to_string(),
            start: a,
            end: b,
            left: r.end.side == Side::Old,
            body,
        };
        self.store.drafts.entry(key).or_default().push(draft);
        self.save();
        self.composing = None;
        self.annotate();
    }

    fn remove_draft(&mut self, i: usize) {
        let Some(key) = self.pr_key() else { return };
        if let Some(list) = self.store.drafts.get_mut(&key) {
            if i < list.len() {
                list.remove(i);
            }
            if list.is_empty() {
                self.store.drafts.remove(&key);
            }
        }
        self.save();
        self.annotate();
    }

    /// Sends the composer's text and line reference to the agent instead.
    fn composer_to_agent(&mut self) {
        let Some((r, edit)) = self.composing.take() else {
            return;
        };
        self.ask_about(r);
        let extra = edit.text.trim();
        if !extra.is_empty() {
            let t = format!("{}{extra}", self.composer.text);
            self.composer = TextEdit::new(t);
            self.composer.focused = true;
        }
        self.annotate();
    }

    /// Re-reads the PR's comments. GitHub's reads lag its writes by a moment, so after posting,
    /// `after` delays the read and comments posted from here that it doesn't have yet are kept.
    fn reload_detail(&mut self, after: Duration) {
        if let Some(Target::Commit(slug, sha)) = self.target.clone() {
            let target = self.target.clone();
            self.bg(
                move || gh::commit_checks(&slug, &sha),
                move |s, res, _| {
                    if s.target != target {
                        return;
                    }
                    match res {
                        Ok(checks) => {
                            if let Some(d) = &mut s.detail {
                                d.checks = checks;
                            }
                        }
                        Err(e) => s.notify(e.to_string(), true),
                    }
                },
            );
            return;
        }
        let Some(Target::Pr(slug, n)) = self.target.clone() else {
            return;
        };
        let target = self.target.clone();
        self.bg(
            move || {
                std::thread::sleep(after);
                gh::pr_detail(&slug, n)
            },
            move |s, res, _| {
                if s.target != target {
                    return;
                }
                match res {
                    Ok(mut d) => {
                        let fresh = std::mem::take(&mut s.posted);
                        for c in fresh {
                            if d.comments.iter().any(|x| {
                                x.author == c.author && x.body == c.body && x.created == c.created
                            }) {
                                continue;
                            }
                            s.posted.push(c.clone());
                            d.comments.push(c);
                        }
                        d.comments.sort_by(|a, b| a.created.cmp(&b.created));
                        s.detail = Some(d);
                        s.annotate();
                    }
                    Err(e) => s.notify(e.to_string(), true),
                }
            },
        );
    }

    /// Shows a comment GitHub just accepted, before its reads catch up.
    fn show_posted(&mut self, c: Comment) {
        if let Some(d) = &mut self.detail {
            d.comments.push(c.clone());
            d.comments.sort_by(|a, b| a.created.cmp(&b.created));
        }
        self.posted.push(c);
        self.annotate();
        self.reload_detail(Duration::from_secs(3));
    }

    /// Posts the composer as a PR comment, or on the selected diff lines when there are some.
    fn post_comment(&mut self) {
        let Some(Target::Pr(slug, n)) = self.target.clone() else {
            return;
        };
        let body = self.comment.text.trim().to_string();
        if body.is_empty() || self.busy {
            return;
        }
        let line = self.diff.selection().and_then(|r| {
            let f = self.diff.files().get(r.file)?;
            let commit = self.detail.as_ref()?.head_sha.clone();
            let (a, b) = (r.start.line.min(r.end.line), r.start.line.max(r.end.line));
            Some((commit, f.path().to_string(), a, b, r.end.side == Side::Old))
        });
        self.busy = true;
        self.bg(
            move || match line {
                Some((commit, path, a, b, left)) => {
                    gh::add_line_comment(&slug, n, &commit, &path, a, b, left, &body)
                }
                None => gh::add_comment(&slug, n, &body),
            },
            |s, res, _| {
                s.busy = false;
                match res {
                    Ok(c) => {
                        s.comment = TextEdit::new("");
                        s.diff.set_selection(None);
                        s.notify("Comment posted", false);
                        s.show_posted(c);
                    }
                    Err(e) => s.notify(e.to_string(), true),
                }
            },
        );
    }

    fn pr_key(&self) -> Option<String> {
        let Some(Target::Pr(slug, n)) = &self.target else {
            return None;
        };
        Some(store::session_key(slug, *n))
    }

    fn is_viewed(&self, file: usize) -> bool {
        let (Some(key), Some(f)) = (self.pr_key(), self.diff.files().get(file)) else {
            return false;
        };
        self.store
            .viewed
            .get(&key)
            .and_then(|m| m.get(f.path()))
            .is_some_and(|h| *h == file_hash(f))
    }

    fn toggle_viewed(&mut self, file: usize) {
        let (Some(key), Some(f)) = (self.pr_key(), self.diff.files().get(file)) else {
            return;
        };
        let (path, hash) = (f.path().to_string(), file_hash(f));
        let viewed = !self.is_viewed(file);
        let map = self.store.viewed.entry(key.clone()).or_default();
        if viewed {
            map.insert(path, hash);
        } else {
            map.remove(&path);
        }
        if map.is_empty() {
            self.store.viewed.remove(&key);
        }
        self.diff.set_collapsed(file, viewed);
        self.save();
    }

    /// Which agent session the open target uses: one per PR, and one per commit.
    fn agent_key(&self) -> Option<String> {
        match self.target.as_ref()? {
            Target::Pr(slug, n) => Some(store::session_key(slug, *n)),
            Target::Commit(slug, sha) => Some(format!("{slug}@{sha}")),
            _ => None,
        }
    }

    fn session_id(&self) -> Option<String> {
        self.store.sessions.get(&self.agent_key()?).cloned()
    }

    /// Clones (first time), fetches, and lists PRs, branches and commits for a repo.
    fn refresh_repo(&mut self, slug: &str) {
        let Some(r) = self.repos.get_mut(slug) else {
            return;
        };
        r.loading = true;
        r.error = None;
        let slug = slug.to_string();
        self.bg(
            {
                let slug = slug.clone();
                move || -> anyhow::Result<_> {
                    let dir = store::clone_dir(&slug);
                    if !dir.join(".git").exists() {
                        gh::clone(&slug, &dir)?;
                    } else {
                        git::fetch(&dir)?;
                    }
                    let default = git::default_branch(&dir);
                    let prs = gh::list_prs(&slug)?;
                    let branches = git::list_branches(&dir, &default).unwrap_or_default();
                    let commits = git::list_commits(&dir, &default, 200).unwrap_or_default();
                    let ci = gh::branch_ci(&slug, &default).unwrap_or_default();
                    Ok((prs, branches, commits, default, ci))
                }
            },
            move |s, res, _| {
                let Some(r) = s.repos.get_mut(&slug) else {
                    return;
                };
                r.loading = false;
                match res {
                    Ok((prs, branches, commits, default, ci)) => {
                        r.ci = ci;
                        r.prs = prs;
                        r.branches = branches;
                        r.commits = commits;
                        r.default_branch = default;
                    }
                    Err(e) => r.error = Some(e.to_string()),
                }
            },
        );
    }

    fn open(&mut self, target: Target, tx: &mut Tx) {
        self.browse = None;
        self.target = Some(target.clone());
        self.set_diff(Vec::new());
        self.composing = None;
        self.posted.clear();
        self.diff_loading = true;
        self.diff_error = None;
        self.detail = None;
        self.commit = None;
        self.next_chat_poll = 0.0;
        if !matches!(target, Target::Pr(..)) {
            self.panel = Panel::Agent;
        }
        tx.wake_after(Duration::from_millis(1));
        let default = self
            .slug()
            .and_then(|s| self.repos.get(&s))
            .map(|r| r.default_branch.clone())
            .unwrap_or_default();
        let t = target.clone();
        self.bg(
            move || -> anyhow::Result<Loaded> {
                Ok(match &t {
                    Target::Pr(slug, n) => {
                        (gh::pr_diff(slug, *n)?, gh::pr_detail(slug, *n).ok(), None)
                    }
                    Target::Branch(slug, b) => (
                        git::branch_diff(&store::clone_dir(slug), &default, b)?,
                        None,
                        None,
                    ),
                    Target::Commit(slug, sha) => {
                        let dir = store::clone_dir(slug);
                        let detail = gh::commit_checks(slug, sha).ok().map(|checks| PrDetail {
                            checks,
                            head_sha: sha.clone(),
                            ..PrDetail::default()
                        });
                        (
                            git::commit_diff(&dir, sha)?,
                            detail,
                            git::commit_info(&dir, sha).ok(),
                        )
                    }
                    Target::Range(slug, a, b) => {
                        (git::range_diff(&store::clone_dir(slug), a, b)?, None, None)
                    }
                })
            },
            move |s, res, _| {
                if s.target.as_ref() != Some(&target) {
                    return;
                }
                s.diff_loading = false;
                match res {
                    Ok((patch, detail, commit)) => {
                        s.commit = commit;
                        let mode = s.diff.mode();
                        s.set_diff(parse(&patch));
                        s.diff.set_mode(mode);
                        for i in 0..s.diff.files().len() {
                            if s.is_viewed(i) {
                                s.diff.set_collapsed(i, true);
                            }
                        }
                        s.detail = detail;
                        s.annotate();
                    }
                    Err(e) => s.diff_error = Some(e.to_string()),
                }
            },
        );
    }

    fn refresh_target(&mut self, tx: &mut Tx) {
        if let Some(t) = self.target.clone() {
            self.open(t, tx);
        }
    }

    fn add_repo(&mut self) {
        let Some(edit) = self.add_repo.take() else {
            return;
        };
        let Some(slug) = store::normalize_slug(&edit.text) else {
            self.notify("Enter a repo as owner/name or a GitHub URL", true);
            self.add_repo = Some(edit);
            return;
        };
        if !self.store.repos.contains(&slug) {
            self.store.repos.push(slug.clone());
            self.save();
            self.repos.insert(slug.clone(), Repo::default());
        }
        self.refresh_repo(&slug);
    }

    fn remove_repo(&mut self, slug: &str) {
        self.store.repos.retain(|r| r != slug);
        self.store
            .sessions
            .retain(|k, _| !k.starts_with(&format!("{slug}#")));
        self.repos.remove(slug);
        if self.slug().as_deref() == Some(slug) {
            self.target = None;
        }
        self.save();
        self.notify(
            format!(
                "Stopped tracking {slug}. Its clone stays in {}",
                store::clone_dir(slug).display()
            ),
            false,
        );
    }

    /// The opencode client once connected; starts connecting in the background otherwise.
    fn client(&mut self) -> Option<Client> {
        if self.oc.is_none() && !self.oc_connecting {
            self.oc_connecting = true;
            self.bg(Client::connect, |s, res, tx| {
                s.oc_connecting = false;
                match res {
                    Ok(c) => {
                        s.oc = Some(c);
                        s.oc_error = None;
                        s.next_chat_poll = 0.0;
                        tx.wake_after(Duration::from_millis(1));
                    }
                    Err(e) => s.oc_error = Some(e.to_string()),
                }
            });
        }
        self.oc.clone()
    }

    /// The title and first-prompt context for a new agent session on the open target.
    fn agent_context(&self) -> Option<(String, String, String)> {
        match self.target.clone()? {
            Target::Pr(slug, n) => {
                let pr = self.pr()?;
                Some((
                    slug.clone(),
                    format!("Review {slug}#{n}: {}", pr.title),
                    preamble(&slug, pr),
                ))
            }
            Target::Commit(slug, _) => {
                let branch = self
                    .repos
                    .get(&slug)
                    .map(|r| r.default_branch.clone())
                    .unwrap_or_else(|| "main".into());
                let (summary, _) = self.commit.as_ref()?;
                let checks = self
                    .detail
                    .as_ref()
                    .map(|d| d.checks.as_slice())
                    .unwrap_or_default();
                let title = format!("CI {slug}@{}: {}", summary.short_sha, summary.subject);
                Some((
                    slug.clone(),
                    title,
                    commit_preamble(&slug, &branch, summary, checks),
                ))
            }
            _ => None,
        }
    }

    /// Sends a prompt to the open target's session, creating the session (with context) first.
    fn ask(&mut self, prompt: String) {
        let (Some(key), Some((slug, title, context))) = (self.agent_key(), self.agent_context())
        else {
            return;
        };
        let oc = self.client();
        let existing = self.session_id();
        let model = self.store.model.clone();
        self.busy = true;
        self.bg(
            move || -> anyhow::Result<(String, bool)> {
                let oc = match oc {
                    Some(c) => c,
                    None => Client::connect()?,
                };
                let dir = store::clone_dir(&slug);
                let (id, fresh) = match existing.filter(|id| oc.session_exists(id)) {
                    Some(id) => {
                        if let Some(m) = &model {
                            oc.set_model(&id, m)?;
                        }
                        (id, false)
                    }
                    None => (oc.create_session(&title, &dir, model.as_ref())?, true),
                };
                let text = if fresh {
                    format!("{context}\n\n{prompt}")
                } else {
                    prompt
                };
                oc.prompt(&id, &text)?;
                Ok((id, fresh))
            },
            move |s, res, tx| {
                s.busy = false;
                match res {
                    Ok((id, fresh)) => {
                        if fresh {
                            s.store.sessions.insert(key, id);
                            s.save();
                        }
                        s.next_chat_poll = 0.0;
                        tx.wake_after(Duration::from_millis(300));
                    }
                    Err(e) => s.notify(e.to_string(), true),
                }
            },
        );
    }

    fn load_models(&mut self) {
        let Some(oc) = self.oc.clone() else { return };
        self.models_loading = true;
        self.bg(
            move || oc.models(),
            |s, res, _| {
                s.models_loading = false;
                match res {
                    Ok(list) => {
                        s.models = list
                            .into_iter()
                            .map(|(id, name, provider)| {
                                let mut o = ComboboxOption::new(id, name);
                                o.detail = provider;
                                o
                            })
                            .collect();
                    }
                    Err(e) => s.notify(format!("Couldn't list opencode models: {e}"), true),
                }
            },
        );
    }

    fn choose_model(&mut self, key: &str) {
        let Some(m) = ModelRef::parse(key) else {
            return;
        };
        self.store.model = Some(m.clone());
        self.save();
        let name = self
            .models
            .iter()
            .find(|o| o.id == key)
            .map_or(key.to_string(), |o| o.label.clone());
        self.notify(format!("Agent model: {name}"), false);
        // Switch the open PR's session now, so its next turn already uses it.
        if self.session_id().is_some() {
            self.session_call(move |oc, id| oc.set_model(id, &m));
        }
    }

    fn send_composer(&mut self) {
        let text = self.composer.text.trim().to_string();
        if text.is_empty() || self.busy {
            return;
        }
        self.composer = TextEdit::new("");
        self.ask(text);
    }

    fn session_call(
        &mut self,
        f: impl FnOnce(&Client, &str) -> anyhow::Result<()> + Send + 'static,
    ) {
        let (Some(oc), Some(id)) = (self.client(), self.session_id()) else {
            return;
        };
        self.bg(
            move || f(&oc, &id),
            |s, res, _| {
                s.next_chat_poll = 0.0;
                if let Err(e) = res {
                    s.notify(e.to_string(), true);
                }
            },
        );
    }

    fn open_terminal(&mut self) {
        let (Some(slug), Some(id)) = (self.slug(), self.session_id()) else {
            return;
        };
        let dir = store::clone_dir(&slug);
        let cmd = format!("cd '{}' && opencode -s {id}", dir.display());
        let script = format!(
            "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
            cmd.replace('"', "\\\"")
        );
        if let Err(e) = std::process::Command::new("osascript")
            .args(["-e", &script])
            .spawn()
        {
            self.notify(format!("Couldn't open Terminal: {e}"), true);
        }
    }

    /// Puts a reference to the selected diff lines into the agent composer.
    fn ask_about(&mut self, lines: LineRange) {
        let Some(file) = self.diff.files().get(lines.file) else {
            return;
        };
        let side = if lines.start.side == Side::Old {
            " (old side)"
        } else {
            ""
        };
        let (a, b) = (
            lines.start.line.min(lines.end.line),
            lines.start.line.max(lines.end.line),
        );
        let at = if a == b {
            format!("{}:{a}", file.path())
        } else {
            format!("{}:{a}-{b}", file.path())
        };
        let mut t = self.composer.text.trim_end().to_string();
        if !t.is_empty() {
            t.push('\n');
        }
        t.push_str(&format!("About `{at}`{side}: "));
        self.composer = TextEdit::new(t);
        self.composer.focused = true;
        self.panel = Panel::Agent;
        self.inspector.set_open(true);
    }

    fn submit_review(&mut self, kind: ReviewKind) {
        let Some(Target::Pr(slug, n)) = self.target.clone() else {
            return;
        };
        let body = self.review.text.trim().to_string();
        let drafts = self.drafts().to_vec();
        if kind.needs_body() && body.is_empty() && drafts.is_empty() {
            self.notify("Write a review body or add line comments first", true);
            return;
        }
        let commit = self
            .detail
            .as_ref()
            .map(|d| d.head_sha.clone())
            .unwrap_or_default();
        let key = self.pr_key();
        self.busy = true;
        self.bg(
            move || {
                if drafts.is_empty() {
                    gh::review_pr(&slug, n, kind, &body)
                } else {
                    gh::submit_review(&slug, n, kind, &body, &commit, &drafts)
                }
                .map(|_| slug)
            },
            move |s, res, _| {
                s.busy = false;
                match res {
                    Ok(slug) => {
                        s.review = TextEdit::new("");
                        if let Some(k) = key {
                            s.store.drafts.remove(&k);
                            s.save();
                        }
                        s.notify(kind.submitted(), false);
                        s.refresh_repo(&slug);
                        s.reload_detail(Duration::from_secs(3));
                    }
                    Err(e) => s.notify(e.to_string(), true),
                }
            },
        );
    }

    fn check_for_update(&mut self) {
        self.bg(update::latest_release, |s, res, _| {
            // Offline or rate limited: say nothing, and try again at the next check.
            let Ok(tag) = res else { return };
            let newer = update::is_newer(&tag, update::current());
            let skipped = s.store.skipped_version.as_deref() == Some(tag.as_str());
            s.update = (newer && !skipped).then_some(tag);
        });
    }

    /// `brew upgrade` gitsnack, then start the new version and quit this one.
    fn run_update(&mut self) {
        let Some(prefix) = update::brew_prefix() else {
            return;
        };
        self.updating = true;
        self.bg(
            move || update::brew_upgrade(&prefix).and_then(|_| update::relaunch()),
            |s, res, tx| {
                s.updating = false;
                match res {
                    Ok(()) => tx.quit(),
                    Err(e) => {
                        s.confirm = None;
                        s.notify(format!("Update failed: {e:#}"), true);
                    }
                }
            },
        );
    }

    fn install_app(&mut self) {
        self.confirm = None;
        self.bg(appbundle::install, |s, res, _| match res {
            Ok(app) => s.notify(
                format!(
                    "Added {}. Open gitsnack from Spotlight or Launchpad.",
                    app.display()
                ),
                false,
            ),
            Err(e) => s.notify(format!("Couldn't add gitsnack.app: {e:#}"), true),
        });
    }

    fn check_merge(&mut self) {
        let Some(Target::Pr(slug, n)) = self.target.clone() else {
            return;
        };
        self.confirm = Some(Confirm::Merge(None));
        self.bg(
            move || gh::merge_readiness(&slug, n),
            |s, res, _| match res {
                Ok(r) => s.confirm = Some(Confirm::Merge(Some(r))),
                Err(e) => {
                    s.confirm = None;
                    s.notify(e.to_string(), true);
                }
            },
        );
    }

    fn merge(&mut self) {
        let Some(Confirm::Merge(Some(r))) = self.confirm.take() else {
            return;
        };
        let (Some(Target::Pr(slug, n)), Some(method)) = (self.target.clone(), r.method) else {
            return;
        };
        self.busy = true;
        self.bg(
            move || gh::merge_pr(&slug, n, method).map(|_| slug),
            move |s, res, _| {
                s.busy = false;
                match res {
                    Ok(slug) => {
                        s.notify(format!("Merged #{n} ({})", method.label()), false);
                        s.target = None;
                        s.refresh_repo(&slug);
                    }
                    Err(e) => s.notify(e.to_string(), true),
                }
            },
        );
    }

    fn set_draft(&mut self, draft: bool) {
        self.confirm = None;
        let Some(Target::Pr(slug, n)) = self.target.clone() else {
            return;
        };
        self.busy = true;
        self.bg(
            move || gh::set_draft(&slug, n, draft).map(|_| slug),
            move |s, res, _| {
                s.busy = false;
                match res {
                    Ok(slug) => {
                        s.notify(
                            if draft {
                                "Converted to draft"
                            } else {
                                "Marked ready for review"
                            },
                            false,
                        );
                        s.refresh_repo(&slug);
                    }
                    Err(e) => s.notify(e.to_string(), true),
                }
            },
        );
    }
}

/// Identifies a file's change: any new push that alters this file's diff changes the hash.
fn file_hash(f: &FileDiff) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
        }
        h = (h ^ 0xff).wrapping_mul(0x100000001b3);
    };
    eat(f.path().as_bytes());
    for hunk in &f.hunks {
        for line in &hunk.lines {
            eat(format!("{:?}", line.kind).as_bytes());
            eat(line.text.as_bytes());
        }
    }
    format!("{h:016x}")
}

/// Context for an agent working on a commit, its CI in particular. One paragraph, so the
/// transcript can hide it like the PR context.
fn commit_preamble(slug: &str, branch: &str, c: &CommitSummary, checks: &[Check]) -> String {
    let failing: Vec<String> = checks
        .iter()
        .filter(|c| c.result == CheckResult::Fail)
        .map(|c| {
            if c.url.is_empty() {
                c.name.clone()
            } else {
                format!("{} ({})", c.name, c.url)
            }
        })
        .collect();
    let pending = checks
        .iter()
        .filter(|c| c.result == CheckResult::Pending)
        .count();
    let ci = if checks.is_empty() {
        "no CI checks reported".to_string()
    } else if failing.is_empty() {
        format!("no failing checks ({pending} pending)")
    } else {
        format!("failing checks: {}", failing.join("; "))
    };
    format!(
        "Context: I'm looking at commit {sha} (\"{subject}\" by {author}, {date}) on the {branch} branch of {slug}, \
         https://github.com/{slug}/commit/{sha}. CI on it: {ci}. Your working directory is a disposable clone of the \
         repo kept by my review app; start with `git fetch origin && git checkout --detach {sha}`. Read CI logs with \
         `gh run list --commit {sha}` and `gh run view <run-id> --log-failed`. Never push to {branch} or any other \
         existing branch.",
        sha = c.sha,
        subject = c.subject,
        author = c.author,
        date = c.date,
    )
}

/// Presets for an agent on a commit.
fn commit_presets(s: &App) -> Vec<(&'static str, String)> {
    let Some(Target::Commit(slug, sha)) = &s.target else {
        return Vec::new();
    };
    let branch = s
        .repos
        .get(slug)
        .map(|r| r.default_branch.clone())
        .unwrap_or_else(|| "main".into());
    let short = &sha[..sha.len().min(8)];
    vec![
        (
            "Fix CI",
            format!(
                "Fix the failing CI on this commit. 1) Read the failing jobs' logs and find the root cause. \
                 2) If it's a real problem in the code or config, create a branch `gitsnack/fix-ci-{short}` from this \
                 commit, make the smallest fix, and run the relevant checks locally (build, tests, linters) until they \
                 pass. 3) Commit, push that branch, and open a draft PR against {branch} with `gh pr create --draft`, \
                 explaining the root cause and the fix. If the failure is flaky or an infrastructure problem rather \
                 than the code, don't change anything: explain why and suggest `gh run rerun --failed <run-id>`, but \
                 don't re-run it yourself."
            ),
        ),
        (
            "Explain failure",
            "Explain why CI failed on this commit. Read the failing jobs' logs, find the root cause and point to the \
             responsible code with file:line references. Say whether it's a real bug, a flaky test or an \
             infrastructure problem. Don't change anything."
                .to_string(),
        ),
        (
            "Review",
            "Review this commit: correctness, design, risks, and anything that should be followed up. Group findings \
             by severity with file:line references."
                .to_string(),
        ),
    ]
}

fn preamble(slug: &str, pr: &PullRequest) -> String {
    format!(
        "Context: I'm reviewing GitHub PR #{n} in {slug}: \"{title}\" by @{author} ({head} → {base}), {url}.\n\
         Your working directory is a disposable clone of the repo kept by my review app. Before reading code, run \
         `git fetch origin && gh pr checkout {n} --force` so you are on the PR's branch; `git diff origin/{base}...HEAD` \
         shows the change. Do not push, comment on, approve or otherwise modify the PR on GitHub: I'll do that myself.",
        n = pr.number,
        title = pr.title,
        author = pr.author,
        head = pr.head_ref,
        base = pr.base_ref,
        url = pr.url,
    )
}

fn setup(s: &mut App, waker: Waker) {
    s.waker = Some(waker);
    // Started from a terminal on a Mac without the app installed: offer to add it.
    if appbundle::supported()
        && !appbundle::launched_from_app()
        && !appbundle::installed()
        && !s.store.skip_app_prompt
        && rune::tooling::headless_run().is_none()
    {
        s.confirm = Some(Confirm::InstallApp);
    }
    s.client();
    for slug in s.store.repos.clone() {
        s.refresh_repo(&slug);
    }
}

fn poll(s: &mut App, now: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    while let Ok(job) = s.jobs.1.try_recv() {
        job(s, tx);
        changed = true;
    }
    if s.oc.is_some() && s.models.is_empty() && !s.models_loading {
        s.load_models();
    }
    // Repos refresh in the background every five minutes (setup did the first load).
    match s.next_repo_refresh {
        None => {
            s.next_repo_refresh = Some(now + REPO_REFRESH_SECS);
            tx.wake_after(Duration::from_secs_f64(REPO_REFRESH_SECS));
        }
        Some(due) if now >= due => {
            for slug in s.store.repos.clone() {
                if s.repos.get(&slug).is_some_and(|r| !r.loading) {
                    s.refresh_repo(&slug);
                }
            }
            s.next_repo_refresh = Some(now + REPO_REFRESH_SECS);
            tx.wake_after(Duration::from_secs_f64(REPO_REFRESH_SECS));
        }
        _ => {}
    }
    // While the checks dialog shows pending checks, re-read them every 15 seconds.
    let pending = s
        .detail
        .as_ref()
        .is_some_and(|d| d.checks.iter().any(|c| c.result == CheckResult::Pending));
    if matches!(s.confirm, Some(Confirm::Checks)) && pending {
        if now >= s.next_checks_poll {
            if s.next_checks_poll > 0.0 {
                s.reload_detail(Duration::ZERO);
            }
            s.next_checks_poll = now + 15.0;
            tx.wake_after(Duration::from_secs(15));
        }
    } else {
        s.next_checks_poll = 0.0;
    }
    // Look for a newer release at launch and every few hours (not in headless renders).
    if now >= s.next_update_check && rune::tooling::headless_run().is_none() {
        s.next_update_check = now + update::CHECK_EVERY_SECS;
        tx.wake_after(Duration::from_secs_f64(update::CHECK_EVERY_SECS));
        s.check_for_update();
    }
    // Status messages linger a few seconds (errors longer), then go.
    if s.toast.is_some() {
        match s.toast_until {
            None => {
                let secs = if s.toast.as_ref().is_some_and(|t| t.1) {
                    8.0
                } else {
                    4.0
                };
                s.toast_until = Some(now + secs);
                tx.wake_after(Duration::from_secs_f64(secs));
            }
            Some(t) if now >= t => {
                s.toast = None;
                s.toast_until = None;
                changed = true;
            }
            _ => {}
        }
    }
    // Follow the open PR's session: quickly while the agent works, slowly otherwise so turns
    // taken from the TUI show up too.
    if let Some(id) = s.session_id()
        && s.panel == Panel::Agent
        && !s.chat_inflight
        && now >= s.next_chat_poll
        && let Some(oc) = s.client()
    {
        s.chat_inflight = true;
        s.bg(
            {
                let id = id.clone();
                move || oc.snapshot(&id)
            },
            move |s, res, tx| {
                s.chat_inflight = false;
                let running = match res {
                    Ok(snap) => {
                        let running = snap.running;
                        if s.chats.get(&id) != Some(&snap) {
                            s.chats.insert(id, snap);
                        }
                        running
                    }
                    Err(_) => false,
                };
                let wait = if running { 0.8 } else { 5.0 };
                s.next_chat_poll = tx.clock() + wait;
                tx.wake_after(Duration::from_secs_f64(wait));
            },
        );
    }
    changed
}

fn on_key(s: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if shell::is_sidebar_toggle(ev) {
        s.sidebar.toggle();
        return true;
    }
    if ev.is_command('r') {
        for slug in s.store.repos.clone() {
            s.refresh_repo(&slug);
        }
        s.refresh_target(tx);
        return true;
    }
    if ev.is_command('i') {
        s.inspector.toggle();
        return true;
    }
    if tx.focused().is_none() || s.target.is_some() {
        s.diff.key(ev, tx)
    } else {
        false
    }
}

// ---------------------------------------------------------------------------------------- view

fn view(s: &App, cx: &mut Cx<App>) -> El {
    let k = rune_theme_opencode::kit(cx.system_dark()).clone();
    let mut layer = Vec::new();
    let main = main_pane(s, &k);
    let title = match &s.target {
        Some(Target::Pr(slug, n)) => format!("{slug} #{n}"),
        Some(Target::Branch(slug, b)) => format!("{slug} · {b}"),
        Some(Target::Commit(slug, sha)) => format!("{slug} · {}", &sha[..sha.len().min(10)]),
        Some(Target::Range(slug, a, b)) => {
            format!("{slug} · {}…{}", &a[..a.len().min(8)], &b[..b.len().min(8)])
        }
        None => "gitsnack".into(),
    };
    let toolbar = Toolbar::new()
        .leading(
            text(title)
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .truncate(),
        )
        .trailing(toolbar_actions(s, &k));
    let split = SplitView::new(main)
        .sidebar(&s.sidebar, |s: &mut App| &mut s.sidebar, sidebar(s, &k))
        .inspector(
            &s.inspector,
            |s: &mut App| &mut s.inspector,
            inspector(s, &k, &mut layer),
        )
        .toolbar(toolbar)
        .titlebar_inset(cx.titlebar_inset())
        .frame_inset(8.0)
        .toolbar_in_panel()
        .sidebar_toggle(SidebarToggle::new().in_header())
        .reduced_motion(cx.is_reduced_motion())
        .build(&k);
    dialogs(s, &k, &mut layer);
    let mut root = col().w_full().h_full().child(split);
    if let Some((msg, err)) = &s.toast {
        root = root.child(toast(&k, msg, *err));
    }
    rune_kit::root(root, layer)
}

fn toast(k: &Kit, msg: &str, error: bool) -> El {
    let c = if error { &k.c.danger } else { &k.c.success };
    let card = row()
        .key("toast")
        .max_w(420.0)
        .px(14.0)
        .py(10.0)
        .gap(10.0)
        .items_center()
        .radius(8.0)
        .fill(k.c.layer_01)
        .border(1.0, c.border)
        .child(el().size(8.0, 8.0).radius(4.0).fill(c.icon))
        .child(
            text(msg.to_string())
                .font(k.ty.ui)
                .color(k.c.text_base)
                .wrap()
                .shrink(),
        )
        .pin(Some(16.0), None, None, Some(16.0))
        .on_press(|s: &mut App| s.toast = None);
    k.elevate(card, &k.elevation.overlay)
}

fn toolbar_actions(s: &App, k: &Kit) -> El {
    let split = s.diff.mode() == DiffMode::Split;
    let mut r = row().gap(6.0).items_center();
    if s.busy || s.diff_loading {
        r = r.child(Spinner::new().build(k));
    }
    if let Some(pr) = s.pr() {
        let url = pr.url.clone();
        let draft = pr.is_draft;
        r = r
            .child(
                Button::new("GitHub")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press_tx(move |_: &mut App, tx| {
                        let _ = tx.open_url(&url);
                    })
                    .build(k),
            )
            .child(
                Button::new(if draft { "Mark ready" } else { "To draft" })
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut App| s.confirm = Some(Confirm::Draft(!draft)))
                    .build(k),
            )
            .child(
                Button::new("Merge…")
                    .variant(if pr.review == ReviewState::Approved {
                        ButtonVariant::Contrast
                    } else {
                        ButtonVariant::Ghost
                    })
                    .on_press(|s: &mut App| s.check_merge())
                    .build(k),
            );
    }
    if s.target.is_some() {
        r = r.child(
            SegmentedControl::new(&["Unified", "Split"], Some(split as usize))
                .width(Some(150.0))
                .on_pick(|s: &mut App, i, tx: &mut Tx| {
                    tx.motion(rune::tok::motion::SNAPPY);
                    s.diff.set_mode(if i == 1 {
                        DiffMode::Split
                    } else {
                        DiffMode::Unified
                    });
                })
                .build(k),
        );
    }
    let open = s.inspector.is_open();
    r.child(
        Button::new("Refresh")
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .shortcut("⌘R")
            .on_press_tx(|s: &mut App, tx| {
                for slug in s.store.repos.clone() {
                    s.refresh_repo(&slug);
                }
                s.refresh_target(tx);
            })
            .build(k),
    )
    .child(
        IconButton::new(&shell::INSPECTOR)
            .variant(IconButtonVariant::GhostMuted)
            .expanded(open)
            .label(if open { "Hide panel" } else { "Show panel" })
            .shortcut("⌘I")
            .on_press(|s: &mut App| s.inspector.toggle())
            .build(k)
            .key("inspector-toggle"),
    )
}

/// A PR's review state as a small coloured label for the sidebar.
fn review_status(k: &Kit, r: ReviewState) -> El {
    let (label, c) = match r {
        ReviewState::Approved => ("Approved", &k.c.success),
        ReviewState::ChangesRequested => ("Changes", &k.c.danger),
        ReviewState::Reviewed => ("Commented", &k.c.info),
        ReviewState::Unreviewed => ("Needs review", &k.c.warning),
    };
    row()
        .px(6.0)
        .py(1.0)
        .radius(4.0)
        .fill(c.bg)
        .child(text(label).font(k.ty.text_xsmall_medium).color(c.fg))
}

fn sidebar(s: &App, k: &Kit) -> El {
    let mut sb = Sidebar::new().label("Repositories").header(
        SidebarRow::new("Add repository")
            .icon(&rune_kit::glyphs::SPARKLE)
            .height(32.0)
            .on_select(|s: &mut App, _| s.add_repo = Some(TextEdit::new("")))
            .build(k)
            .key("add-repo"),
    );
    for (slug, repo) in &s.repos {
        let mut rows = Vec::new();
        if repo.loading && repo.prs.is_empty() {
            rows.push(
                SidebarRow::new("Loading…")
                    .select_only()
                    .build(k)
                    .key(format!("{slug}/loading")),
            );
        }
        if let Some(e) = &repo.error {
            rows.push(
                text(e.clone())
                    .font(k.ty.small)
                    .color(k.c.danger.fg)
                    .wrap()
                    .key(format!("{slug}/err")),
            );
        }
        for pr in &repo.prs {
            let target = Target::Pr(slug.clone(), pr.number);
            let has_session = s
                .store
                .sessions
                .contains_key(&store::session_key(slug, pr.number));
            let mut detail = format!("#{} · {}", pr.number, pr.author);
            if pr.is_draft {
                detail.push_str(" · draft");
            }
            if has_session {
                detail.push_str(" · ✦");
            }
            let t = target.clone();
            let mut r = SidebarRow::new(pr.title.clone())
                .subtitle(detail)
                .height(44.0)
                .selected(s.target.as_ref() == Some(&target))
                .on_select(move |s: &mut App, tx| s.open(t.clone(), tx));
            r = r.trailing(review_status(k, pr.review));
            rows.push(r.build(k).key(format!("{slug}/pr/{}", pr.number)));
        }
        for (label, kind) in [("Branches", Browse::Branches), ("Commits", Browse::Commits)] {
            let slug2 = slug.clone();
            rows.push(
                SidebarRow::new(label)
                    .height(30.0)
                    .selected(s.browse.as_ref() == Some(&(slug.clone(), kind)))
                    .on_select(move |s: &mut App, _| {
                        s.browse = Some((slug2.clone(), kind));
                        s.range_base = None;
                    })
                    .build(k)
                    .key(format!("{slug}/{label}")),
            );
        }
        let (slug_t, slug_r) = (slug.clone(), slug.clone());
        sb = sb.section(
            SidebarSection::new(slug.clone())
                .key(slug.clone())
                .count(repo.prs.len())
                .collapsed(repo.collapsed)
                .on_toggle(move |s: &mut App, _| {
                    if let Some(r) = s.repos.get_mut(&slug_t) {
                        r.collapsed ^= true;
                    }
                })
                .action(
                    Button::new("Remove")
                        .variant(ButtonVariant::GhostFaint)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut App| s.remove_repo(&slug_r))
                        .build(k),
                )
                .rows(rows),
        );
    }
    if let (Some(tag), false) = (&s.update, s.update_hidden) {
        sb = sb.footer(
            SidebarRow::new(format!("Update available: {tag}"))
                .icon(&rune_kit::glyphs::SPARKLE)
                .height(36.0)
                .on_select(|s: &mut App, _| s.confirm = Some(Confirm::Update))
                .build(k)
                .key("update-available"),
        );
    }
    sb.build(k)
}

fn main_pane(s: &App, k: &Kit) -> El {
    if let Some((slug, kind)) = &s.browse {
        return browse_list(s, k, slug, *kind);
    }
    let Some(target) = &s.target else {
        let msg = if s.repos.is_empty() {
            "Add a repository to start reviewing."
        } else {
            "Pick a pull request, branch or commit."
        };
        return col()
            .w_full()
            .h_full()
            .center()
            .child(text(msg).font(k.ty.body).color(k.c.text_muted));
    };
    let mut c = col().w_full().h_full().min_h(0.0);
    if let Target::Pr(..) = target
        && let Some(pr) = s.pr()
    {
        c = c.child(pr_header(s, k, pr));
    }
    if let (Target::Commit(..), Some((summary, body))) = (target, &s.commit) {
        c = c.child(commit_header(s, k, summary, body));
    }
    if let Some(e) = &s.diff_error {
        return c.child(
            text(e.clone())
                .font(k.ty.ui)
                .color(k.c.danger.fg)
                .wrap()
                .padding(16.0),
        );
    }
    if s.diff_loading {
        return c.child(
            col()
                .w_full()
                .grow(1.0)
                .center()
                .child(Spinner::new().build(k)),
        );
    }
    if s.diff.files().is_empty() {
        return c.child(
            col()
                .w_full()
                .grow(1.0)
                .center()
                .child(text("No changes").font(k.ty.ui).color(k.c.text_muted)),
        );
    }
    let mut dv = DiffView::new(|s: &App| &s.diff, |s: &mut App| &mut s.diff);
    if matches!(target, Target::Pr(..)) {
        let kc = k.clone();
        dv = dv
            .on_comment(|s: &mut App, lines, _| s.open_composer(lines))
            .header_actions(move |s: &App, i| {
                Checkbox::new(s.is_viewed(i))
                    .label("Viewed")
                    .on_toggle(move |s: &mut App| s.toggle_viewed(i))
                    .build(&kc)
            });
    }
    let kc = k.clone();
    dv = dv.annotation(move |s: &App, a: &Annotation| annotation(s, &kc, a));
    dv = dv.on_visible_file(|s: &mut App, i| {
        if let Some(path) = s.diff.files().get(i).map(|f| f.path().to_string()) {
            s.files.select(&path);
        }
    });
    let body = row()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(file_tree(s, k))
        .child(col().grow(1.0).h_full().min_w(0.0).child(dv.build(s, k)));
    c.child(body)
}

fn file_tree(s: &App, k: &Kit) -> El {
    let stats: Vec<(u32, u32, bool)> = s
        .diff
        .files()
        .iter()
        .enumerate()
        .map(|(i, f)| (f.additions() as u32, f.deletions() as u32, s.is_viewed(i)))
        .collect();
    let check = k.c.success.icon;
    let ty = k.ty.small_medium;
    let tree = Tree::new(
        |s: &App| &s.files,
        |s: &mut App| &mut s.files,
        |n: &FileNode<usize>| n.name.as_str(),
    )
    .name("Changed files")
    .row(move |n, r| {
        let item = TreeItem::file(n, r);
        match n.data.and_then(|i| stats.get(i)) {
            Some(&(a, d, viewed)) => {
                let item = item.changes(a, d);
                if viewed {
                    item.trailing(text("✓").font(ty).color(check))
                } else {
                    item
                }
            }
            None => item,
        }
    })
    .on_select(|s: &mut App, path: &str, tx: &mut Tx| {
        if let Some(i) = s.diff.file_index(path) {
            s.diff.scroll_to_file(i, tx);
        }
    })
    .build(s, k)
    .grow(1.0)
    .min_h(0.0);
    col()
        .key("file-tree")
        .w(260.0)
        .h_full()
        .padding(8.0)
        .gap(6.0)
        .border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_muted)
        .child(
            text(format!("{} changed files", s.diff.files().len()))
                .font(k.ty.small_medium)
                .color(k.c.text_muted)
                .px(6.0)
                .py(4.0),
        )
        .child(tree)
}

fn annotation(s: &App, k: &Kit, a: &Annotation) -> El {
    let frame = |inner: El| col().w_full().px(12.0).py(6.0).child(inner);
    let card = || {
        col()
            .w_full()
            .gap(8.0)
            .padding(10.0)
            .radius(8.0)
            .fill(k.c.layer_01)
            .border(1.0, k.c.border_muted)
    };
    if a.id == "compose" {
        let Some((_, edit)) = &s.composing else {
            return col();
        };
        let empty = edit.text.trim().is_empty();
        let actions = row()
            .w_full()
            .gap(8.0)
            .child(row().grow(1.0))
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut App| s.close_composer())
                    .build(k),
            )
            .child(
                Button::new("Ask agent")
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut App| s.composer_to_agent())
                    .build(k),
            )
            .child(
                Button::new("Add review comment")
                    .variant(ButtonVariant::Contrast)
                    .control_size(ControlSize::Small)
                    .disabled(empty)
                    .on_press(|s: &mut App| s.add_draft())
                    .build(k),
            );
        let input = TextArea::new(edit)
            .placeholder("Leave a comment on these lines… (⌘Enter to add to the review)")
            .auto_grow(3, 12)
            .width(None)
            .on_edit(|s: &mut App| &mut s.composing.as_mut().expect("composer open").1)
            .on_key(|s: &mut App, ev: &KeyEvent, _: &mut Tx| {
                if ev.key == rune::Key::Escape {
                    s.close_composer();
                    return true;
                }
                if ev.key == rune::Key::Enter && ev.mods.cmd {
                    s.add_draft();
                    return true;
                }
                false
            })
            .build(k);
        return frame(
            card()
                .border(1.0, k.c.border_focus)
                .child(input)
                .child(actions),
        );
    }
    if let Some(i) =
        a.id.strip_prefix("d:")
            .and_then(|i| i.parse::<usize>().ok())
    {
        let Some(d) = s.drafts().get(i) else {
            return col();
        };
        let head = row()
            .w_full()
            .gap(8.0)
            .items_center()
            .child(Badge::new("Pending").variant(BadgeVariant::Accent).build(k))
            .child(
                text("Your review comment")
                    .font(k.ty.small_medium)
                    .color(k.c.text_muted),
            )
            .child(row().grow(1.0))
            .child(
                Button::new("Delete")
                    .variant(ButtonVariant::GhostFaint)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut App| s.remove_draft(i))
                    .build(k),
            );
        return frame(
            card()
                .child(head)
                .child(Markdown::new(d.body.clone()).selectable(true).build(k)),
        );
    }
    let Some(d) = &s.detail else { return col() };
    let mut thread = card();
    for id in
        a.id.trim_start_matches("c:")
            .split(',')
            .filter_map(|i| i.parse::<usize>().ok())
    {
        if let Some(c) = d.comments.get(id) {
            thread = thread.child(comment_card(k, c, false));
        }
    }
    frame(thread)
}

fn comment_card(k: &Kit, c: &Comment, show_path: bool) -> El {
    let mut head = row()
        .w_full()
        .gap(8.0)
        .items_center()
        .child(
            text(format!("@{}", c.author))
                .font(k.ty.small_medium)
                .color(k.c.text_base),
        )
        .child(
            text(c.created.get(..10).unwrap_or("").to_string())
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    if let Some(state) = &c.review_state {
        let (label, v) = match state.as_str() {
            "APPROVED" => ("approved", BadgeVariant::Accent),
            "CHANGES_REQUESTED" => ("changes requested", BadgeVariant::Neutral),
            _ => ("reviewed", BadgeVariant::Subtle),
        };
        head = head.child(Badge::new(label).variant(v).build(k));
    }
    if show_path && let (Some(path), Some(line)) = (&c.path, c.line) {
        head = head.child(
            text(format!("{path}:{line}"))
                .font(k.ty.mono)
                .color(k.c.text_muted)
                .truncate()
                .shrink(),
        );
    }
    let mut card = col().w_full().gap(4.0).child(head);
    if !c.body.trim().is_empty() {
        card = card.child(Markdown::new(c.body.clone()).selectable(true).build(k));
    }
    if c.reply {
        card = card
            .pl(12.0)
            .border_sides([0.0, 0.0, 0.0, 2.0], k.c.border_muted);
    }
    card
}

/// "CI: 2 failing ›", opening the checks dialog; none until checks are known.
fn ci_chip(s: &App, k: &Kit) -> Option<El> {
    let d = s.detail.as_ref()?;
    let count = |r: CheckResult| d.checks.iter().filter(|c| c.result == r).count();
    let (fail, pend) = (count(CheckResult::Fail), count(CheckResult::Pending));
    if d.checks.is_empty() {
        return Some(text("No CI checks").font(k.ty.small).color(k.c.text_faint));
    }
    let (label, ink) = if fail > 0 {
        (format!("CI: {fail} failing"), k.c.danger.fg)
    } else if pend > 0 {
        (format!("CI: {pend} pending"), k.c.warning.fg)
    } else {
        ("CI passing".to_string(), k.c.success.fg)
    };
    let chip = row()
        .key("ci-status")
        .gap(4.0)
        .px(6.0)
        .py(2.0)
        .radius(4.0)
        .items_center()
        .cursor(rune::Cursor::Pointer)
        .role("button")
        .label("Show CI checks")
        .child(text(label).font(k.ty.small_medium).color(ink))
        .child(text("›").font(k.ty.small_medium).color(ink))
        .on_press(|s: &mut App| s.confirm = Some(Confirm::Checks));
    Some(k.interactive(chip))
}

fn commit_header(s: &App, k: &Kit, c: &CommitSummary, body: &str) -> El {
    let (adds, dels) = s
        .diff
        .files()
        .iter()
        .fold((0, 0), |(a, d), f| (a + f.additions(), d + f.deletions()));
    let mut meta = row()
        .gap(8.0)
        .items_center()
        .child(
            text(c.short_sha.clone())
                .font(k.ty.mono)
                .color(k.c.text_muted),
        )
        .child(
            text(c.author.clone())
                .font(k.ty.small_medium)
                .color(k.c.text_base),
        )
        .child(text(c.date.clone()).font(k.ty.small).color(k.c.text_muted))
        .child(
            text(format!("+{adds} −{dels} · {} files", s.diff.files().len()))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    match ci_chip(s, k) {
        Some(chip) => meta = meta.child(chip),
        None => meta = meta.child(text("Loading CI…").font(k.ty.small).color(k.c.text_faint)),
    }
    let mut h = col()
        .w_full()
        .px(16.0)
        .py(12.0)
        .gap(6.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_muted)
        .child(
            text(c.subject.clone())
                .font(k.ty.body_medium)
                .color(k.c.text_base)
                .wrap(),
        )
        .child(meta);
    if !body.is_empty() {
        h = h.child(
            col()
                .w_full()
                .max_h(140.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(Markdown::new(body.to_string()).selectable(true).build(k)),
        );
    }
    h
}

fn pr_header(s: &App, k: &Kit, pr: &PullRequest) -> El {
    let (adds, dels) = s
        .diff
        .files()
        .iter()
        .fold((0, 0), |(a, d), f| (a + f.additions(), d + f.deletions()));
    let mut meta = row()
        .gap(8.0)
        .items_center()
        .child(
            text(format!("@{}", pr.author))
                .font(k.ty.small_medium)
                .color(k.c.text_base),
        )
        .child(
            text(format!("{} → {}", pr.head_ref, pr.base_ref))
                .font(k.ty.small)
                .color(k.c.text_muted),
        )
        .child(
            text(format!("+{adds} −{dels} · {} files", s.diff.files().len()))
                .font(k.ty.small)
                .color(k.c.text_faint),
        )
        .child({
            let n = s.diff.files().len();
            let done = (0..n).filter(|i| s.is_viewed(*i)).count();
            let ink = if n > 0 && done == n {
                k.c.success.fg
            } else {
                k.c.text_muted
            };
            text(format!("{done}/{n} viewed"))
                .font(k.ty.small_medium)
                .color(ink)
        })
        .child(
            Badge::new(pr.review.label())
                .variant(if pr.review == ReviewState::Approved {
                    BadgeVariant::Accent
                } else {
                    BadgeVariant::Neutral
                })
                .build(k),
        );
    if pr.is_draft {
        meta = meta.child(Badge::new("draft").variant(BadgeVariant::Subtle).build(k));
    }
    if let Some(chip) = ci_chip(s, k) {
        meta = meta.child(chip);
    }
    let mut h = col()
        .w_full()
        .px(16.0)
        .py(12.0)
        .gap(6.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_muted)
        .child(
            text(pr.title.clone())
                .font(k.ty.body_medium)
                .color(k.c.text_base)
                .wrap(),
        )
        .child(meta);
    if let Some(body) = s
        .detail
        .as_ref()
        .map(|d| d.body.trim())
        .filter(|b| !b.is_empty())
    {
        h = h.child(
            col()
                .w_full()
                .max_h(180.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(Markdown::new(body.to_string()).selectable(true).build(k)),
        );
    }
    h
}

fn browse_list(s: &App, k: &Kit, slug: &str, kind: Browse) -> El {
    let Some(repo) = s.repos.get(slug) else {
        return col();
    };
    let mut list = col().w_full().gap(2.0).padding(12.0);
    let line =
        |label: String, detail: String, extra: Option<El>, dot: Option<rune::Color>, on: OnRow| {
            let mut r = row()
                .w_full()
                .px(10.0)
                .py(8.0)
                .gap(10.0)
                .radius(6.0)
                .items_center();
            if let Some(c) = dot {
                r = r.child(el().size(8.0, 8.0).radius(4.0).fill(c));
            }
            let mut r = r
                .child(
                    text(label)
                        .font(k.ty.ui)
                        .color(k.c.text_base)
                        .truncate()
                        .grow(1.0),
                )
                .child(text(detail).font(k.ty.small).color(k.c.text_faint));
            if let Some(e) = extra {
                r = r.child(e);
            }
            k.interactive(r.on_press_tx(move |s: &mut App, tx| on(s, tx)))
        };
    match kind {
        Browse::Branches => {
            list = list.child(
                text(format!(
                    "Branches compared with origin/{}",
                    repo.default_branch
                ))
                .font(k.ty.ui_medium)
                .color(k.c.text_muted)
                .py(6.0),
            );
            for b in &repo.branches {
                let t = Target::Branch(slug.into(), b.clone());
                list = list.child(
                    line(
                        b.clone(),
                        String::new(),
                        None,
                        None,
                        Box::new(move |s, tx| s.open(t.clone(), tx)),
                    )
                    .key(format!("br/{b}")),
                );
            }
        }
        Browse::Commits => {
            let hint = match &s.range_base {
                Some(b) => format!("Range base {} — pick the head commit", &b[..8.min(b.len())]),
                None => format!(
                    "Commits on origin/{} — “Base” starts a range",
                    repo.default_branch
                ),
            };
            list = list.child(
                text(hint)
                    .font(k.ty.ui_medium)
                    .color(k.c.text_muted)
                    .py(6.0),
            );
            for c in &repo.commits {
                let (slug2, sha, sha2) = (slug.to_string(), c.sha.clone(), c.sha.clone());
                let is_base = s.range_base.as_deref() == Some(c.sha.as_str());
                let base = Button::new(if is_base { "Base ✓" } else { "Base" })
                    .variant(ButtonVariant::GhostFaint)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut App| {
                        s.range_base = if s.range_base.as_deref() == Some(sha2.as_str()) {
                            None
                        } else {
                            Some(sha2.clone())
                        };
                    })
                    .build(k);
                list = list.child(
                    line(
                        c.subject.clone(),
                        format!("{} · {} · {}", c.short_sha, c.author, c.date),
                        Some(base),
                        repo.ci.get(&c.sha).map(|r| match r {
                            CheckResult::Pass => k.c.success.icon,
                            CheckResult::Pending => k.c.warning.icon,
                            CheckResult::Fail => k.c.danger.icon,
                        }),
                        Box::new(move |s, tx| {
                            let t = match s.range_base.take() {
                                Some(b) if b != sha => Target::Range(slug2.clone(), b, sha.clone()),
                                _ => Target::Commit(slug2.clone(), sha.clone()),
                            };
                            s.open(t, tx);
                        }),
                    )
                    .key(format!("c/{}", c.sha)),
                );
            }
        }
    }
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(list)
}

fn inspector(s: &App, k: &Kit, layer: &mut Vec<El>) -> El {
    if matches!(s.target, Some(Target::Commit(..))) {
        let head = row()
            .w_full()
            .px(16.0)
            .py(12.0)
            .child(text("Agent").font(k.ty.ui_medium).color(k.c.text_base));
        return col()
            .w_full()
            .h_full()
            .min_h(0.0)
            .child(head)
            .child(agent_panel(s, k, layer));
    }
    if !matches!(s.target, Some(Target::Pr(..))) {
        return col().w_full().h_full().padding(16.0).center().child(
            text("Open a pull request or a commit to work with the agent on it.")
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .wrap(),
        );
    }
    let count = s.detail.as_ref().map_or(0, |d| d.comments.len());
    let comments_label = if count > 0 {
        format!("Comments ({count})")
    } else {
        "Comments".to_string()
    };
    let labels = ["Agent".to_string(), comments_label, "Review".to_string()];
    let current = match s.panel {
        Panel::Agent => 0,
        Panel::Comments => 1,
        Panel::Review => 2,
    };
    let tabs = SegmentedControl::new(&labels, Some(current))
        .width(Some(340.0))
        .on_pick(|s: &mut App, i, _: &mut Tx| {
            s.panel = [Panel::Agent, Panel::Comments, Panel::Review][i.min(2)];
            s.next_chat_poll = 0.0;
        })
        .build(k);
    let body = match s.panel {
        Panel::Agent => agent_panel(s, k, layer),
        Panel::Comments => comments_panel(s, k),
        Panel::Review => review_panel(s, k),
    };
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(row().w_full().px(12.0).py(8.0).child(tabs))
        .child(body)
}

fn agent_panel(s: &App, k: &Kit, layer: &mut Vec<El>) -> El {
    let id = s.session_id();
    let snap = id.as_ref().and_then(|id| s.chats.get(id));
    let running = snap.is_some_and(|c| c.running);

    let mut transcript = col().w_full().gap(10.0).padding(12.0);
    if let Some(e) = &s.oc_error {
        transcript = transcript.child(
            text(format!("opencode unavailable: {e}"))
                .font(k.ty.small)
                .color(k.c.danger.fg)
                .wrap(),
        );
    }
    match snap {
        None if id.is_none() => {
            transcript = transcript.child(
                text(if matches!(s.target, Some(Target::Commit(..))) {
                    "No agent session for this commit yet. Pick a preset or ask anything; gitsnack starts an opencode session in its clone of the repo with this commit and its CI results as context."
                } else {
                    "No agent session for this PR yet. Pick a preset or ask anything; gitsnack starts an opencode session in its clone of the repo and tells the agent to check out this PR."
                })
                    .font(k.ty.ui)
                    .color(k.c.text_muted)
                    .wrap(),
            );
        }
        None => transcript = transcript.child(Spinner::new().build(k)),
        Some(snap) => {
            for (i, e) in snap.entries.iter().enumerate() {
                // Hide the context preamble of the first prompt.
                let e = match e {
                    Entry::User(t) if i == 0 && t.starts_with("Context: ") => {
                        Entry::User(t.split_once("\n\n").map(|x| x.1).unwrap_or(t).to_string())
                    }
                    e => e.clone(),
                };
                transcript = transcript.child(entry(k, &e).key(format!("e{i}")));
            }
            for p in &snap.permissions {
                transcript = transcript.child(permission(k, p));
            }
            if running {
                transcript = transcript.child(
                    row()
                        .gap(8.0)
                        .items_center()
                        .child(Spinner::new().build(k))
                        .child(text("Working…").font(k.ty.small).color(k.c.text_faint)),
                );
            }
        }
    }

    let mut presets = row().gap(6.0).w_full().flex_wrap();
    let list: Vec<(&str, String)> = if matches!(s.target, Some(Target::Commit(..))) {
        commit_presets(s)
    } else {
        PRESETS.iter().map(|(l, p)| (*l, p.to_string())).collect()
    };
    for (label, prompt) in list {
        let primary = label == "Fix CI";
        presets = presets.child(
            Button::new(label)
                .variant(if primary {
                    ButtonVariant::Contrast
                } else {
                    ButtonVariant::Outline
                })
                .control_size(ControlSize::Small)
                .disabled(s.busy || running)
                .on_press(move |s: &mut App| s.ask(prompt.clone()))
                .build(k),
        );
    }
    let current = s.store.model.as_ref().map(ModelRef::key);
    let model = Combobox::new("model", &s.model_combo, &s.models, |s: &mut App| {
        &mut s.model_combo
    })
    .label("Model")
    .current(current.as_deref())
    .placeholder(if s.models.is_empty() {
        "Loading models…"
    } else {
        "opencode default model"
    })
    .search_placeholder("Search models")
    .width(240.0)
    .max_height(320.0)
    .placement(rune_kit::Placement::TopStart)
    .disabled(s.models.is_empty())
    .on_choose(|s: &mut App, id: &str, _: &mut Tx| s.choose_model(id))
    .build(k, layer);
    let model_row = row()
        .w_full()
        .gap(8.0)
        .items_center()
        .child(text("Model").font(k.ty.small_medium).color(k.c.text_muted))
        .child(model);
    let mut actions = row().gap(6.0).items_center().child(row().grow(1.0));
    if id.is_some() {
        actions = actions.child(
            Button::new("Open in terminal")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut App| s.open_terminal())
                .build(k),
        );
    }
    if running {
        actions = actions.child(
            Button::new("Stop")
                .variant(ButtonVariant::Danger)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut App| s.session_call(|oc, id| oc.interrupt(id)))
                .build(k),
        );
    }
    actions = actions.child(
        Button::new("Send")
            .variant(ButtonVariant::Contrast)
            .control_size(ControlSize::Small)
            .disabled(s.busy || s.composer.text.trim().is_empty())
            .on_press(|s: &mut App| s.send_composer())
            .build(k),
    );
    let composer = col()
        .w_full()
        .gap(8.0)
        .padding(12.0)
        .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_muted)
        .child(model_row)
        .child(presets)
        .child(
            TextArea::new(&s.composer)
                .placeholder(if matches!(s.target, Some(Target::Commit(..))) {
                    "Ask the agent about this commit… (Enter to send, Shift-Enter for a newline)"
                } else {
                    "Ask the agent about this PR… (Enter to send, Shift-Enter for a newline)"
                })
                .auto_grow(3, 12)
                .width(None)
                .on_edit(|s: &mut App| &mut s.composer)
                .on_submit(|s: &mut App, _| s.send_composer())
                .build(k),
        )
        .child(actions);
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec { stick_bottom: true })
                .child(transcript),
        )
        .child(composer)
}

fn entry(k: &Kit, e: &Entry) -> El {
    match e {
        Entry::User(t) => row().w_full().child(row().grow(1.0)).child(
            col()
                .max_w(380.0)
                .px(12.0)
                .py(8.0)
                .radius(10.0)
                .fill(k.c.bg_accent)
                .child(
                    text(t.clone())
                        .font(k.ty.body)
                        .color(k.c.text_base)
                        .wrap()
                        .selectable(true),
                ),
        ),
        Entry::Text(t) => col()
            .w_full()
            .child(Markdown::new(t.clone()).selectable(true).build(k)),
        Entry::Reasoning(t) => text(t.clone())
            .font(k.ty.small)
            .color(k.c.text_faint)
            .wrap()
            .max_lines(3),
        Entry::Tool {
            name,
            summary,
            status,
        } => {
            let ink = if status == "error" {
                k.c.danger.fg
            } else {
                k.c.text_muted
            };
            row()
                .w_full()
                .gap(6.0)
                .child(text(format!("▸ {name}")).font(k.ty.small_medium).color(ink))
                .child(
                    text(summary.clone())
                        .font(k.ty.mono)
                        .color(k.c.text_faint)
                        .truncate()
                        .grow(1.0),
                )
        }
        Entry::Shell { command, status } => text(format!("$ {command}  ({status})"))
            .font(k.ty.mono)
            .color(k.c.text_muted)
            .truncate(),
        Entry::Error(e) => text(e.clone()).font(k.ty.small).color(k.c.danger.fg).wrap(),
    }
}

fn permission(k: &Kit, p: &opencode::Permission) -> El {
    let btn = |label: &'static str, decision: &'static str, v: ButtonVariant| {
        let id = p.id.clone();
        Button::new(label)
            .variant(v)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut App| {
                let id = id.clone();
                s.session_call(move |oc, sid| oc.reply_permission(sid, &id, decision));
            })
            .build(k)
    };
    let what = if p.message.is_empty() {
        format!("{}: {}", p.action, p.resources.join(", "))
    } else {
        p.message.clone()
    };
    col()
        .w_full()
        .padding(10.0)
        .gap(8.0)
        .radius(8.0)
        .fill(k.c.warning.bg)
        .child(
            text(format!("Permission requested — {what}"))
                .font(k.ty.ui)
                .color(k.c.warning.fg)
                .wrap(),
        )
        .child(
            row()
                .gap(6.0)
                .child(btn("Allow once", "once", ButtonVariant::Contrast))
                .child(btn("Always", "always", ButtonVariant::Outline))
                .child(btn("Reject", "reject", ButtonVariant::Ghost)),
        )
        .key(format!("perm/{}", p.id))
}

fn comments_panel(s: &App, k: &Kit) -> El {
    let mut list = col().w_full().gap(14.0).padding(12.0);
    match &s.detail {
        None => list = list.child(Spinner::new().build(k)),
        Some(d) if d.comments.is_empty() => {
            list = list.child(text("No comments yet.").font(k.ty.ui).color(k.c.text_muted));
        }
        Some(d) => {
            for (i, c) in d.comments.iter().enumerate() {
                let mut card = col()
                    .w_full()
                    .padding(10.0)
                    .radius(8.0)
                    .fill(k.c.layer_01)
                    .child(comment_card(k, c, true));
                if let (Some(path), Some(line)) = (c.path.clone(), c.line) {
                    let left = c.left;
                    card = k.interactive(card.on_press_tx(move |s: &mut App, tx| {
                        if let Some(f) = s.diff.file_index(&path) {
                            s.diff.set_collapsed(f, false);
                            s.diff.scroll_to_line(
                                f,
                                LineRef::new(if left { Side::Old } else { Side::New }, line),
                                tx,
                            );
                        }
                    }));
                }
                list = list.child(card.key(format!("c{i}")));
            }
        }
    }
    let target = s.diff.selection().and_then(|r| {
        let f = s.diff.files().get(r.file)?;
        let (a, b) = (r.start.line.min(r.end.line), r.start.line.max(r.end.line));
        Some(if a == b {
            format!("{}:{a}", f.path())
        } else {
            format!("{}:{a}-{b}", f.path())
        })
    });
    let hint = match &target {
        Some(t) => format!("Commenting on {t}"),
        None => {
            "Commenting on the PR. Click line numbers in the diff to comment on lines.".to_string()
        }
    };
    let mut actions = row().w_full().gap(8.0).items_center().child(
        text(hint)
            .font(k.ty.small)
            .color(k.c.text_muted)
            .wrap()
            .grow(1.0),
    );
    if target.is_some() {
        actions = actions.child(
            Button::new("Clear lines")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut App| s.diff.set_selection(None))
                .build(k),
        );
    }
    actions = actions.child(
        Button::new("Comment")
            .variant(ButtonVariant::Contrast)
            .control_size(ControlSize::Small)
            .disabled(s.busy || s.comment.text.trim().is_empty())
            .on_press(|s: &mut App| s.post_comment())
            .build(k),
    );
    let composer = col()
        .w_full()
        .gap(8.0)
        .padding(12.0)
        .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_muted)
        .child(
            TextArea::new(&s.comment)
                .placeholder("Leave a comment (markdown)…")
                .auto_grow(3, 12)
                .width(None)
                .on_edit(|s: &mut App| &mut s.comment)
                .build(k),
        )
        .child(actions);
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec { stick_bottom: true })
                .child(list),
        )
        .child(composer)
}

fn review_panel(s: &App, k: &Kit) -> El {
    let drafts = s.drafts();
    let btn = |label: &'static str, kind: ReviewKind, v: ButtonVariant| {
        Button::new(label)
            .variant(v)
            .disabled(s.busy)
            .on_press(move |s: &mut App| s.submit_review(kind))
            .build(k)
    };
    let mut pending = col().w_full().gap(6.0);
    if drafts.is_empty() {
        pending = pending.child(
            text("No line comments yet. Hover a line in the diff and click “+” to add one to this review.")
                .font(k.ty.small)
                .color(k.c.text_faint)
                .wrap(),
        );
    }
    for (i, d) in drafts.iter().enumerate() {
        let at = if d.start == d.end {
            format!("{}:{}", d.path, d.end)
        } else {
            format!("{}:{}-{}", d.path, d.start, d.end)
        };
        let (path, line, left) = (d.path.clone(), d.end, d.left);
        let item = row()
            .w_full()
            .gap(8.0)
            .px(10.0)
            .py(8.0)
            .radius(6.0)
            .fill(k.c.layer_01)
            .items_center()
            .child(
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .gap(2.0)
                    .child(text(at).font(k.ty.mono).color(k.c.text_muted).truncate())
                    .child(
                        text(d.body.clone())
                            .font(k.ty.ui)
                            .color(k.c.text_base)
                            .max_lines(2),
                    ),
            )
            .child(
                Button::new("Delete")
                    .variant(ButtonVariant::GhostFaint)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut App| s.remove_draft(i))
                    .build(k),
            )
            .on_press_tx(move |s: &mut App, tx| {
                if let Some(f) = s.diff.file_index(&path) {
                    s.diff.set_collapsed(f, false);
                    s.diff.scroll_to_line(
                        f,
                        LineRef::new(if left { Side::Old } else { Side::New }, line),
                        tx,
                    );
                }
            });
        pending = pending.child(k.interactive(item).key(format!("draft{i}")));
    }
    let title = if drafts.is_empty() {
        "Line comments".to_string()
    } else {
        format!("Line comments ({} pending)", drafts.len())
    };
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            col()
                .w_full()
                .padding(12.0)
                .gap(10.0)
                .child(text(title).font(k.ty.ui_medium).color(k.c.text_muted))
                .child(pending)
                .child(
                    text("Review summary (markdown)")
                        .font(k.ty.ui_medium)
                        .color(k.c.text_muted)
                        .pt(6.0),
                )
                .child(
                    TextArea::new(&s.review)
                        .placeholder("Leave a review…")
                        .rows(10)
                        .width(None)
                        .on_edit(|s: &mut App| &mut s.review)
                        .build(k),
                )
                .child(
                    row()
                        .gap(8.0)
                        .child(btn("Comment", ReviewKind::Comment, ButtonVariant::Outline))
                        .child(btn("Approve", ReviewKind::Approve, ButtonVariant::Submit))
                        .child(btn(
                            "Request changes",
                            ReviewKind::RequestChanges,
                            ButtonVariant::Danger,
                        )),
                ),
        )
}

/// Seconds since the epoch for GitHub's `2026-11-10T21:57:56Z` timestamps.
fn epoch(ts: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400 + hh * 3600 + mm * 60 + ss)
}

fn duration(c: &Check) -> String {
    let (Some(a), Some(b)) = (epoch(&c.started), epoch(&c.completed)) else {
        return String::new();
    };
    let secs = (b - a).max(0);
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn checks_dialog(s: &App, k: &Kit, layer: &mut Vec<El>) {
    let checks = s
        .detail
        .as_ref()
        .map(|d| d.checks.as_slice())
        .unwrap_or_default();
    let mut order: Vec<&Check> = checks.iter().collect();
    let rank = |c: &Check| match c.result {
        CheckResult::Fail => 0,
        CheckResult::Pending => 1,
        CheckResult::Pass => 2,
    };
    order.sort_by(|a, b| (rank(a), &a.workflow, &a.name).cmp(&(rank(b), &b.workflow, &b.name)));
    let count = |r: CheckResult| checks.iter().filter(|c| c.result == r).count();
    let summary = format!(
        "{} failing · {} pending · {} passed",
        count(CheckResult::Fail),
        count(CheckResult::Pending),
        count(CheckResult::Pass)
    );
    let mut list = col().w_full().gap(2.0);
    for (i, c) in order.iter().enumerate() {
        let dot = match c.result {
            CheckResult::Fail => k.c.danger.icon,
            CheckResult::Pending => k.c.warning.icon,
            CheckResult::Pass => k.c.success.icon,
        };
        let mut sub = vec![c.state.clone()];
        if !c.workflow.is_empty() {
            sub.insert(0, c.workflow.clone());
        }
        let took = duration(c);
        if !took.is_empty() {
            sub.push(took);
        }
        let info = col()
            .grow(1.0)
            .min_w(0.0)
            .gap(2.0)
            .child(
                text(c.name.clone())
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base)
                    .truncate(),
            )
            .child(
                text(sub.join(" · "))
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .truncate(),
            );
        let mut item = row()
            .w_full()
            .gap(10.0)
            .px(10.0)
            .py(8.0)
            .radius(6.0)
            .items_center()
            .child(el().size(8.0, 8.0).radius(4.0).fill(dot))
            .child(info);
        if !c.url.is_empty() {
            let url = c.url.clone();
            item = item
                .child(
                    text("Open ↗")
                        .font(k.ty.small_medium)
                        .color(k.c.text_accent),
                )
                .cursor(rune::Cursor::Pointer)
                .on_press_tx(move |_: &mut App, tx| {
                    let _ = tx.open_url(&url);
                });
            item = k.interactive(item);
        }
        list = list.child(item.key(format!("check{i}")));
    }
    let body = col()
        .w_full()
        .gap(8.0)
        .child(text(summary).font(k.ty.small).color(k.c.text_muted))
        .child(
            col()
                .w_full()
                .max_h(440.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(list),
        );
    let all = match &s.target {
        Some(Target::Commit(slug, sha)) => format!("https://github.com/{slug}/commit/{sha}"),
        _ => s
            .pr()
            .map(|p| format!("{}/checks", p.url))
            .unwrap_or_default(),
    };
    let open_all = Button::new("Open on GitHub")
        .variant(ButtonVariant::Ghost)
        .disabled(all.is_empty())
        .on_press_tx(move |_: &mut App, tx| {
            let _ = tx.open_url(&all);
        })
        .build(k);
    let refresh = Button::new("Refresh")
        .variant(ButtonVariant::Ghost)
        .on_press(|s: &mut App| s.reload_detail(Duration::ZERO))
        .build(k);
    let done = Button::new("Done")
        .variant(ButtonVariant::Contrast)
        .on_press(|s: &mut App| s.confirm = None)
        .build(k);
    let title = if count(CheckResult::Pending) > 0 {
        "CI checks (refreshing every 15s)"
    } else {
        "CI checks"
    };
    modal_wide(
        "checks",
        title,
        None,
        vec![body],
        vec![open_all, refresh, done],
        k,
        layer,
        620.0,
    );
}

/// A dialog sized to its content, with a padded body and right-aligned actions.
fn modal(
    key: &str,
    title: &str,
    description: Option<&str>,
    body: Vec<El>,
    actions: Vec<El>,
    k: &Kit,
    layer: &mut Vec<El>,
) {
    modal_wide(key, title, description, body, actions, k, layer, 460.0);
}

#[allow(clippy::too_many_arguments)]
fn modal_wide(
    key: &str,
    title: &str,
    description: Option<&str>,
    body: Vec<El>,
    actions: Vec<El>,
    k: &Kit,
    layer: &mut Vec<El>,
    width: f32,
) {
    let mut footer = row().w_full().gap(8.0).pt(4.0).child(row().grow(1.0));
    for a in actions {
        footer = footer.child(a);
    }
    let mut content = col().w_full().px(16.0).pb(16.0).gap(12.0);
    if let Some(desc) = description {
        content = content.child(
            text(desc.to_string())
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .wrap()
                .mt(-6.0),
        );
    }
    let content = content.children(body).child(footer);
    Dialog::new(key, title)
        .open(true)
        .fit()
        .width(width)
        .child(content)
        .on_close(|s: &mut App| {
            s.add_repo = None;
            s.confirm = None;
        })
        .build(k, layer);
}

fn cancel(k: &Kit) -> El {
    Button::new("Cancel")
        .variant(ButtonVariant::Ghost)
        .on_press(|s: &mut App| {
            s.add_repo = None;
            s.confirm = None;
        })
        .build(k)
}

fn dialogs(s: &App, k: &Kit, layer: &mut Vec<El>) {
    if let Some(edit) = &s.add_repo {
        let input = TextInput::new(edit)
            .placeholder("owner/name or https://github.com/owner/name")
            .width(None)
            .on_edit(|s: &mut App| s.add_repo.as_mut().expect("dialog open"))
            .on_submit(|s: &mut App, _| s.add_repo())
            .build(k);
        let add = Button::new("Add")
            .variant(ButtonVariant::Contrast)
            .on_press(|s: &mut App| s.add_repo())
            .build(k);
        modal(
            "add-repo",
            "Add repository",
            Some("gitsnack clones it into ~/.local/share/gitsnack/repos and keeps it fetched."),
            vec![input],
            vec![cancel(k), add],
            k,
            layer,
        );
    }
    match &s.confirm {
        Some(Confirm::Draft(draft)) => {
            let draft = *draft;
            let (title, desc) = if draft {
                (
                    "Convert to draft?",
                    "Reviewers won't be asked to review it until it's marked ready again.",
                )
            } else {
                (
                    "Mark ready for review?",
                    "Requested reviewers will be notified.",
                )
            };
            let ok = Button::new("Confirm")
                .variant(ButtonVariant::Contrast)
                .on_press(move |s: &mut App| s.set_draft(draft))
                .build(k);
            modal(
                "draft",
                title,
                Some(desc),
                vec![],
                vec![cancel(k), ok],
                k,
                layer,
            );
        }
        Some(Confirm::Checks) => checks_dialog(s, k, layer),
        Some(Confirm::Update) => {
            let Some(tag) = s.update.clone() else { return };
            let notes = format!("https://github.com/{}/releases/tag/{tag}", update::REPO);
            let brew = update::brew_prefix().is_some();
            let mut body = vec![
                text(format!(
                    "gitsnack {tag} is available. You have v{}.",
                    update::current()
                ))
                .font(k.ty.ui)
                .color(k.c.text_base)
                .wrap(),
            ];
            if s.updating {
                body.push(
                    row()
                        .gap(10.0)
                        .items_center()
                        .child(Spinner::new().build(k))
                        .child(
                            text("Updating with Homebrew…")
                                .font(k.ty.ui)
                                .color(k.c.text_muted),
                        ),
                );
            } else if !brew {
                body.push(
                    text("This copy wasn't installed with Homebrew, so update it the way you installed it: download the new release, or `git pull && make install` from a checkout.")
                        .font(k.ty.ui)
                        .color(k.c.text_muted)
                        .wrap(),
                );
            }
            let open_notes = Button::new("Release notes")
                .variant(ButtonVariant::Ghost)
                .on_press_tx(move |_: &mut App, tx| {
                    let _ = tx.open_url(&notes);
                })
                .build(k);
            let skip_tag = tag.clone();
            let skip = Button::new("Skip this version")
                .variant(ButtonVariant::Ghost)
                .disabled(s.updating)
                .on_press(move |s: &mut App| {
                    s.store.skipped_version = Some(skip_tag.clone());
                    s.save();
                    s.update = None;
                    s.confirm = None;
                })
                .build(k);
            let later = Button::new("Later")
                .variant(ButtonVariant::Ghost)
                .disabled(s.updating)
                .on_press(|s: &mut App| {
                    s.update_hidden = true;
                    s.confirm = None;
                })
                .build(k);
            let mut actions = vec![open_notes, skip, later];
            if brew {
                actions.push(
                    Button::new("Update and restart")
                        .variant(ButtonVariant::Contrast)
                        .disabled(s.updating)
                        .on_press(|s: &mut App| s.run_update())
                        .build(k),
                );
            }
            modal("update", "Update gitsnack", None, body, actions, k, layer);
        }
        Some(Confirm::InstallApp) => {
            let never = Button::new("Don't ask again")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut App| {
                    s.confirm = None;
                    s.store.skip_app_prompt = true;
                    s.save();
                    s.notify("You can add it later with `gitsnack install-app`", false);
                })
                .build(k);
            let later = Button::new("Not now")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut App| s.confirm = None)
                .build(k);
            let add = Button::new("Add to Applications")
                .variant(ButtonVariant::Contrast)
                .on_press(|s: &mut App| s.install_app())
                .build(k);
            let detail = text(format!(
                "Creates {} so you can open gitsnack from Spotlight, Launchpad or the Dock. \
                 It runs the gitsnack you already have installed, so `brew upgrade` keeps it current. \
                 Same as running `gitsnack install-app`.",
                "~/Applications/gitsnack.app"
            ))
            .font(k.ty.ui)
            .color(k.c.text_muted)
            .wrap();
            modal(
                "install-app",
                "Add gitsnack to your Applications?",
                None,
                vec![detail],
                vec![never, later, add],
                k,
                layer,
            );
        }
        Some(Confirm::Merge(r)) => {
            let Some(r) = r else {
                let wait = row()
                    .gap(10.0)
                    .items_center()
                    .child(Spinner::new().build(k))
                    .child(
                        text("Checking CI and merge rules…")
                            .font(k.ty.ui)
                            .color(k.c.text_muted),
                    );
                modal(
                    "merge",
                    "Merge pull request",
                    None,
                    vec![wait],
                    vec![cancel(k)],
                    k,
                    layer,
                );
                return;
            };
            let line = |l: &str, v: &str, ok: bool, warn: bool| {
                row()
                    .w_full()
                    .gap(8.0)
                    .items_center()
                    .child(
                        text(l.to_string())
                            .font(k.ty.ui)
                            .color(k.c.text_muted)
                            .w(80.0),
                    )
                    .child(el().size(8.0, 8.0).radius(4.0).fill(if ok {
                        k.c.success.icon
                    } else if warn {
                        k.c.warning.icon
                    } else {
                        k.c.danger.icon
                    }))
                    .child(
                        text(v.to_string())
                            .font(k.ty.ui_medium)
                            .color(k.c.text_base),
                    )
            };
            let mut status = col()
                .w_full()
                .gap(8.0)
                .padding(12.0)
                .radius(6.0)
                .fill(k.c.bg_base)
                .child(line(
                    "Review",
                    if r.approved {
                        "approved"
                    } else {
                        "not approved"
                    },
                    r.approved,
                    false,
                ))
                .child(line(
                    "CI",
                    &r.ci_label,
                    r.ci_ok,
                    r.ci_label.starts_with("pending"),
                ))
                .child(line("Merge", &r.merge_label, r.merge_ok, false));
            if !r.problems.is_empty() {
                let mut list = col().gap(4.0).pt(4.0);
                for p in &r.problems {
                    list = list.child(
                        text(format!("• {p}"))
                            .font(k.ty.small)
                            .color(k.c.text_muted),
                    );
                }
                status = status.child(list);
            }
            let label = r
                .method
                .map(|m| format!("Merge ({})", m.label()))
                .unwrap_or_else(|| "Merge".into());
            let go = Button::new(label)
                .variant(ButtonVariant::Contrast)
                .disabled(!r.can_merge())
                .on_press(|s: &mut App| s.merge())
                .build(k);
            modal(
                "merge",
                "Merge pull request",
                None,
                vec![status],
                vec![cancel(k), go],
                k,
                layer,
            );
        }
        None => {}
    }
}

/// Headless-only sample data (`--scene demo`), for checking the layout without GitHub.
fn demo(s: &mut App) {
    let slug = "acme/widgets".to_string();
    let pr = |number, title: &str, author: &str, review, is_draft| PullRequest {
        number,
        title: title.into(),
        author: author.into(),
        base_ref: "main".into(),
        head_ref: format!("feature-{number}"),
        url: String::new(),
        is_draft,
        review,
        additions: 10,
        deletions: 2,
        updated: "2026-11-09".into(),
    };
    let mut repo = Repo {
        default_branch: "main".into(),
        ..Default::default()
    };
    repo.prs = vec![
        pr(
            42,
            "Add retry jitter to the HTTP client",
            "alice",
            ReviewState::Unreviewed,
            false,
        ),
        pr(
            41,
            "Cache resolved configs per workspace",
            "bob",
            ReviewState::Approved,
            false,
        ),
        pr(
            39,
            "WIP: streaming exports",
            "carol",
            ReviewState::ChangesRequested,
            true,
        ),
    ];
    s.repos = BTreeMap::from([(slug.clone(), repo)]);
    s.models = [
        ("anthropic/claude-sonnet", "Claude Sonnet", "anthropic"),
        ("opencode/iceman-free", "Iceman Free", "opencode"),
        ("openai/gpt-5", "GPT-5", "openai"),
    ]
    .into_iter()
    .map(|(id, name, p)| {
        let mut o = ComboboxOption::new(id, name);
        o.detail = p.into();
        o
    })
    .collect();
    s.store.model = ModelRef::parse("opencode/iceman-free");
    s.store
        .sessions
        .insert(store::session_key(&slug, 42), "ses_demo".into());
    s.target = Some(Target::Pr(slug, 42));
    let c = |author: &str, body: &str, path: Option<&str>, line, state: Option<&str>| Comment {
        author: author.into(),
        body: body.into(),
        created: "2026-11-09T10:00:00Z".into(),
        review_state: state.map(Into::into),
        path: path.map(Into::into),
        line,
        left: false,
        reply: false,
    };
    s.detail = Some(PrDetail {
        body: "Spreads retries out so clients that failed together don't retry together.".into(),
        checks: [
            (
                "CICD",
                "Build (ubuntu)",
                CheckResult::Pass,
                "success",
                "2026-11-09T10:00:00Z",
                "2026-11-09T10:04:12Z",
            ),
            (
                "CICD",
                "Clippy",
                CheckResult::Fail,
                "failure",
                "2026-11-09T10:00:00Z",
                "2026-11-09T10:01:30Z",
            ),
            (
                "CICD",
                "Build (macos)",
                CheckResult::Pending,
                "in progress",
                "2026-11-09T10:00:00Z",
                "",
            ),
            ("", "license/cla", CheckResult::Pass, "success", "", ""),
        ]
        .into_iter()
        .map(|(w, n, result, state, a, b)| Check {
            name: n.into(),
            workflow: w.into(),
            result,
            state: state.into(),
            url: "https://github.com/acme/widgets/actions/runs/1".into(),
            started: a.into(),
            completed: b.into(),
        })
        .collect(),
        head_sha: "abc".into(),
        comments: vec![
            c("bob", "Nice, this has bitten us before.", None, None, None),
            c(
                "carol",
                "Should `jitter` be configurable?",
                Some("src/retry.rs"),
                Some(4),
                None,
            ),
            c("dave", "", None, None, Some("APPROVED")),
        ],
    });
    s.set_diff(parse(
        "diff --git a/src/retry.rs b/src/retry.rs\n--- a/src/retry.rs\n+++ b/src/retry.rs\n@@ -1,4 +1,5 @@\n pub struct Backoff {\n     base: Duration,\n-    max: Duration,\n+    max: Duration,\n+    jitter: f32,\n }\ndiff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1,2 +1,2 @@\n # widgets\n-Retries back off.\n+Retries back off, with jitter.\n",
    ));
    let h = file_hash(&s.diff.files()[1]);
    s.store.viewed.insert(
        store::session_key("acme/widgets", 42),
        BTreeMap::from([("README.md".to_string(), h)]),
    );
    s.diff.set_collapsed(1, true);
    s.annotate();
    s.toast = Some(("Approved".into(), false));
    s.chats.insert(
        "ses_demo".into(),
        Snapshot {
            entries: vec![
                Entry::User("Context: …\n\nSummarize what this PR changes.".into()),
                Entry::Tool { name: "bash".into(), summary: "gh pr checkout 42 --force".into(), status: "completed".into() },
                Entry::Text("This PR adds **jitter** to `Backoff`:\n\n- new `jitter` field\n- `next()` takes a random roll\n\nThe critical path is `src/retry.rs:12`.".into()),
            ],
            running: true,
            permissions: vec![opencode::Permission { id: "per_1".into(), action: "bash".into(), resources: vec!["git push".into()], message: String::new() }],
        },
    );
}

fn main() {
    if appbundle::cli(&std::env::args().collect::<Vec<_>>()) {
        return;
    }
    // A bare binary has no bundle icon; this gives the Dock and app switcher the logo.
    let _ = rune::set_app_icon(include_bytes!("../assets/icon.png").to_vec());
    let spec = AppSpec {
        scenes: &[
            ("demo", demo),
            ("add-repo", |s: &mut App| {
                demo(s);
                s.add_repo = Some(TextEdit::new(""));
            }),
            ("line-review", |s: &mut App| {
                demo(s);
                s.toast = None;
                s.panel = Panel::Review;
                s.store.drafts.insert(
                    store::session_key("acme/widgets", 42),
                    vec![Draft {
                        path: "src/retry.rs".into(),
                        start: 3,
                        end: 3,
                        left: false,
                        body: "Can `max` be `Option`?".into(),
                    }],
                );
                s.open_composer(LineRange::line(0, LineRef::new(Side::New, 2)));
            }),
            ("checks", |s: &mut App| {
                demo(s);
                s.toast = None;
                s.confirm = Some(Confirm::Checks);
            }),
            ("install-app", |s: &mut App| {
                demo(s);
                s.toast = None;
                s.confirm = Some(Confirm::InstallApp);
            }),
            ("commit", |s: &mut App| {
                demo(s);
                s.toast = None;
                let sha = "4f2a9c1e0b7d3a6f5e8c2b1a0d9e8f7a6b5c4d3e".to_string();
                let summary = CommitSummary {
                    sha: sha.clone(),
                    short_sha: "4f2a9c1".into(),
                    author: "alice".into(),
                    date: "2026-11-10".into(),
                    subject: "Add retry jitter to the HTTP client (#42)".into(),
                };
                if let Some(r) = s.repos.get_mut("acme/widgets") {
                    r.default_branch = "main".into();
                    r.ci.insert(sha.clone(), CheckResult::Fail);
                    r.commits = vec![summary.clone()];
                }
                s.target = Some(Target::Commit("acme/widgets".into(), sha.clone()));
                s.commit = Some((
                    summary,
                    "Spreads retries out so clients don't retry together.".into(),
                ));
                s.detail = Some(PrDetail {
                    checks: vec![Check {
                        name: "Clippy".into(),
                        workflow: "CI".into(),
                        result: CheckResult::Fail,
                        state: "failure".into(),
                        url: "https://github.com/acme/widgets/actions/runs/1".into(),
                        started: String::new(),
                        completed: String::new(),
                    }],
                    head_sha: sha.clone(),
                    ..PrDetail::default()
                });
                s.annotate();
                s.store
                    .sessions
                    .insert(format!("acme/widgets@{sha}"), "ses_ci".into());
                s.chats.insert(
                    "ses_ci".into(),
                    Snapshot {
                        entries: vec![
                            Entry::User("Context: …\n\nExplain why CI failed on this commit.".into()),
                            Entry::Tool { name: "bash".into(), summary: "gh run view 123 --log-failed".into(), status: "completed".into() },
                            Entry::Text("**Clippy** fails on `src/retry.rs:12`: `jitter` is never read. It's a real lint error, not a flake; the fix is to use it in `next()`.".into()),
                        ],
                        running: false,
                        permissions: Vec::new(),
                    },
                );
            }),
            ("update", |s: &mut App| {
                demo(s);
                s.toast = None;
                s.update = Some("v9.0.0".into());
                s.confirm = Some(Confirm::Update);
            }),
            ("comments", |s: &mut App| {
                demo(s);
                s.panel = Panel::Comments;
                s.toast = None;
            }),
            ("merge", |s: &mut App| {
                demo(s);
                s.confirm = Some(Confirm::Merge(Some(MergeReadiness::assess(
                    true,
                    "MERGEABLE",
                    "CLEAN",
                    false,
                    &[("ci".into(), model::CheckResult::Pending)],
                    Some(model::MergeMethod::Squash),
                ))));
            }),
        ],
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        fonts: rune_theme_opencode::FONTS,
        families: rune_theme_opencode::FAMILIES,
        min_size: Some((900.0, 560.0)),
        ..AppSpec::new("gitsnack", (1440.0, 900.0), init, view)
    };
    let chrome = Chrome {
        full_size_content: true,
        traffic_lights: Some((16.0, 25.0)),
        drag_regions: true,
    };
    rune::App::new(spec).chrome(chrome).modifier_keys().run();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hits GitHub: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn tracks_a_real_repo() {
        let slug = "sharkdp/hexyl";
        let dir = std::env::temp_dir().join("gitsnack-it").join(slug);
        if !dir.join(".git").exists() {
            gh::clone(slug, &dir).unwrap();
        }
        git::fetch(&dir).unwrap();
        let default = git::default_branch(&dir);
        let commits = git::list_commits(&dir, &default, 5).unwrap();
        assert!(
            !parse(&git::commit_diff(&dir, &commits[0].sha).unwrap()).is_empty()
                || commits[0].subject.contains("Merge")
        );
        git::list_branches(&dir, &default).unwrap();
        let prs = gh::list_prs(slug).unwrap();
        if let Some(pr) = prs.first() {
            assert!(!parse(&gh::pr_diff(slug, pr.number).unwrap()).is_empty());
            gh::pr_detail(slug, pr.number).unwrap();
        }
        println!(
            "default={default} prs={} commits={}",
            prs.len(),
            commits.len()
        );
    }
}

#[cfg(test)]
mod agent_tests {
    use super::*;

    /// Talks to the local opencode service: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn creates_and_reads_a_session() {
        let oc = Client::connect().unwrap();
        let models = oc.models().unwrap();
        let free = models
            .iter()
            .find(|m| m.0 == "opencode/iceman-free")
            .or(models.first())
            .map(|m| ModelRef::parse(&m.0).unwrap());
        let id = oc
            .create_session("gitsnack test", &std::env::temp_dir(), free.as_ref())
            .unwrap();
        if let Some(m) = &free {
            oc.set_model(&id, m).unwrap();
        }
        oc.prompt(&id, "Reply with exactly: pong").unwrap();
        let snap = oc.snapshot(&id).unwrap();
        oc.delete_session(&id).unwrap();
        assert!(matches!(snap.entries.first(), Some(Entry::User(t)) if t.contains("pong")));
    }
}

#[cfg(test)]
mod comment_tests {
    use super::*;

    /// Hits GitHub: `cargo test -- --ignored`. hexyl#300 has inline review comments.
    #[test]
    #[ignore]
    fn reads_inline_comments() {
        let d = gh::pr_detail("sharkdp/hexyl", 300).unwrap();
        assert!(!d.head_sha.is_empty());
        assert!(
            d.comments
                .iter()
                .any(|c| c.path.is_some() && c.line.is_some())
        );
    }
}

#[cfg(test)]
mod commit_ci_tests {
    use super::*;

    /// Hits GitHub: `cargo test -- --ignored`. gitsnack's own commits have Actions runs.
    #[test]
    #[ignore]
    fn reads_ci_for_commits() {
        let slug = "vangheem/gitsnack";
        let states = gh::branch_ci(slug, "main").unwrap();
        assert!(!states.is_empty());
        let (sha, _) = states
            .iter()
            .find(|(_, r)| **r == CheckResult::Pass)
            .unwrap();
        let checks = gh::commit_checks(slug, sha).unwrap();
        assert!(
            checks
                .iter()
                .any(|c| c.workflow == "CI" && c.url.starts_with("https://"))
        );
        let preamble = commit_preamble(
            slug,
            "main",
            &CommitSummary {
                sha: sha.clone(),
                short_sha: sha[..7].into(),
                author: "a".into(),
                date: "d".into(),
                subject: "s".into(),
            },
            &checks,
        );
        assert!(preamble.starts_with("Context: ") && !preamble.contains("\n\n"));
    }
}
