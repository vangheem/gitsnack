//! Persistent app data: tracked repos and the opencode session attached to each PR.
//! Lives in `~/.local/share/gitsnack/state.json`; managed clones sit beside it in `repos/`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub repos: Vec<String>,
    /// `"owner/name#123"` → opencode session id.
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,
    /// `"owner/name#123"` → file path → hash of that file's diff when it was marked viewed.
    #[serde(default)]
    pub viewed: BTreeMap<String, BTreeMap<String, String>>,
    /// `"owner/name#123"` → line comments waiting to be submitted with the next review.
    /// The opencode model agent sessions use; opencode's default when unset.
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub drafts: BTreeMap<String, Vec<Draft>>,
    /// The user chose "Don't ask again" for adding gitsnack.app to ~/Applications.
    #[serde(default)]
    pub skip_app_prompt: bool,
    /// A release the user chose to skip ("v0.4.0").
    #[serde(default)]
    pub skipped_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

impl ModelRef {
    /// `provider/id`, the combobox option id.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    pub fn parse(key: &str) -> Option<ModelRef> {
        let (provider, id) = key.split_once('/')?;
        Some(ModelRef {
            provider: provider.into(),
            id: id.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub path: String,
    pub start: u32,
    pub end: u32,
    /// On the old side of the diff (a removed line).
    pub left: bool,
    pub body: String,
}

pub fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("gitsnack")
}

/// Where gitsnack keeps its own clone of `owner/name`.
pub fn clone_dir(slug: &str) -> PathBuf {
    data_dir().join("repos").join(slug)
}

pub fn session_key(slug: &str, pr: u64) -> String {
    format!("{slug}#{pr}")
}

impl Store {
    fn path() -> PathBuf {
        data_dir().join("state.json")
    }

    pub fn load() -> Store {
        std::fs::read(Self::path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap()).context("create data dir")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

/// Accepts `owner/name`, `https://github.com/owner/name(.git)` or `git@github.com:owner/name.git`.
pub fn normalize_slug(input: &str) -> Option<String> {
    let s = input.trim().trim_end_matches('/').trim_end_matches(".git");
    let s = s
        .strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))
        .or_else(|| s.strip_prefix("git@github.com:"))
        .or_else(|| s.strip_prefix("github.com/"))
        .unwrap_or(s);
    let mut parts = s.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let valid = |p: &str| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    (parts.next().is_none() && valid(owner) && valid(name)).then(|| format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::normalize_slug;

    #[test]
    fn slugs_from_urls() {
        assert_eq!(
            normalize_slug("https://github.com/anomalyco/rune.git").as_deref(),
            Some("anomalyco/rune")
        );
        assert_eq!(
            normalize_slug("git@github.com:a/b.git").as_deref(),
            Some("a/b")
        );
        assert_eq!(normalize_slug(" a/b/ ").as_deref(), Some("a/b"));
        assert_eq!(normalize_slug("a/b/c"), None);
        assert_eq!(normalize_slug("nope"), None);
    }
}
