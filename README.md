# gitsnack

A native PR review app, built with [Rune](https://github.com/anomalyco/rune). It replaces gitcrack and lets you bring an opencode agent into each PR review.

- **Repos**: add `owner/name` (or a GitHub URL). gitsnack keeps its own clone under `~/.local/share/gitsnack/repos/` and fetches it on refresh. Nothing touches your working copies.
- **Review** (ported from gitcrack): open PRs with their diffs (unified or split), branches compared with the default branch, commits and commit ranges. You can comment, approve or request changes, merge after a CI and merge-rule check, and switch a PR between ready and draft. GitHub actions go through `gh`.
- **Agent**: each PR gets an opencode session on your shared opencode background service. The session runs in gitsnack's clone, and its first prompt tells the agent which PR to check out. Use the presets (Review, Summarize, Risks, Tests) or ask your own question. Click the "+" on a diff line to ask about those lines. Permission requests appear inline. "Open in terminal" attaches the opencode TUI to the same session (`opencode -s <id>`).

## Install

Requires Rust (the toolchain is pinned in `rust-toolchain.toml`), `git`, `gh` and `opencode`.

```sh
make install   # ~/.local/bin/gitsnack
```

## Keys

| Key | Action |
| --- | --- |
| ⌘R | refresh repos and the open diff |
| ⌘I | show or hide the agent/review panel |
| ⇧⌘B | show or hide the sidebar |
| j / k | next / previous hunk |
| ] / [ | next / previous file |

`gitsnack --snap out.png --scene demo` renders sample data headlessly.
