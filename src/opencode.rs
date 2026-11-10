//! A small client for the shared opencode background service (the one the TUI uses), so
//! sessions gitsnack creates can also be opened with `opencode -s <id>`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use serde_json::{Value, json};

use crate::store::ModelRef;

#[derive(Clone)]
pub struct Client {
    url: String,
    auth: String,
    agent: ureq::Agent,
}

/// One rendered entry of a session transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    Text(String),
    Reasoning(String),
    Tool {
        name: String,
        summary: String,
        status: String,
    },
    Shell {
        command: String,
        status: String,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Permission {
    pub id: String,
    pub action: String,
    pub resources: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub entries: Vec<Entry>,
    pub running: bool,
    pub permissions: Vec<Permission>,
}

fn service_file() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"))
        .join("opencode/service.json")
}

impl Client {
    /// Finds the background service, starting it when it isn't registered or healthy.
    pub fn connect() -> Result<Client> {
        if let Ok(c) = Self::from_file()
            && c.get("/api/info").is_ok()
        {
            return Ok(c);
        }
        let status = Command::new("opencode")
            .args(["service", "start"])
            .status()
            .context("run `opencode service start`")?;
        if !status.success() {
            return Err(anyhow!("`opencode service start` failed"));
        }
        let c = Self::from_file()?;
        c.get("/api/info")?;
        Ok(c)
    }

    fn from_file() -> Result<Client> {
        let raw = std::fs::read(service_file()).context("opencode service is not registered")?;
        let v: Value = serde_json::from_slice(&raw)?;
        let url = v["url"]
            .as_str()
            .ok_or_else(|| anyhow!("service.json has no url"))?
            .trim_end_matches('/')
            .to_string();
        let password = v["password"].as_str().unwrap_or("");
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"))
        );
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .build();
        Ok(Client { url, auth, agent })
    }

    fn get(&self, path: &str) -> Result<Value> {
        let r = self
            .agent
            .get(&format!("{}{path}", self.url))
            .set("Authorization", &self.auth)
            .call();
        Ok(map_err(r)?.into_json()?)
    }

    fn post(&self, path: &str, body: Value) -> Result<Value> {
        let r = self
            .agent
            .post(&format!("{}{path}", self.url))
            .set("Authorization", &self.auth)
            .send_json(body);
        // Some endpoints (switching a session's model) answer with no body.
        let text = map_err(r)?.into_string()?;
        if text.trim().is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_str(&text)?)
        }
    }

    pub fn create_session(
        &self,
        title: &str,
        dir: &Path,
        model: Option<&ModelRef>,
    ) -> Result<String> {
        let mut body = json!({ "title": title, "location": { "directory": dir } });
        if let Some(m) = model {
            body["model"] = json!({ "id": m.id, "providerID": m.provider });
        }
        let v = self.post("/api/session", body)?;
        v["data"]["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("session create returned no id"))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn delete_session(&self, id: &str) -> Result<()> {
        map_err(
            self.agent
                .delete(&format!("{}/api/session/{id}", self.url))
                .set("Authorization", &self.auth)
                .call(),
        )?;
        Ok(())
    }

    pub fn session_exists(&self, id: &str) -> bool {
        self.get(&format!("/api/session/{id}")).is_ok()
    }

    pub fn set_model(&self, id: &str, m: &ModelRef) -> Result<()> {
        self.post(
            &format!("/api/session/{id}/model"),
            json!({ "model": { "id": m.id, "providerID": m.provider } }),
        )?;
        Ok(())
    }

    /// Enabled models as `(provider/id, name, provider)`, sorted by provider then name.
    pub fn models(&self) -> Result<Vec<(String, String, String)>> {
        let v = self.get("/api/model")?;
        let mut out: Vec<_> = v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| {
                m["enabled"].as_bool().unwrap_or(true) && m["status"].as_str() != Some("deprecated")
            })
            .filter_map(|m| {
                let (id, provider) = (m["id"].as_str()?, m["providerID"].as_str()?);
                let name = m["name"].as_str().unwrap_or(id);
                Some((
                    format!("{provider}/{id}"),
                    name.to_string(),
                    provider.to_string(),
                ))
            })
            .collect();
        out.sort_by(|a, b| (&a.2, &a.1).cmp(&(&b.2, &b.1)));
        Ok(out)
    }

    pub fn prompt(&self, id: &str, text: &str) -> Result<()> {
        self.post(
            &format!("/api/session/{id}/prompt"),
            json!({ "text": text }),
        )?;
        Ok(())
    }

    pub fn interrupt(&self, id: &str) -> Result<()> {
        self.post(&format!("/api/session/{id}/interrupt"), json!({}))?;
        Ok(())
    }

    pub fn reply_permission(&self, id: &str, request: &str, decision: &str) -> Result<()> {
        self.post(
            &format!("/api/session/{id}/permission/{request}/reply"),
            json!({ "decision": decision }),
        )?;
        Ok(())
    }

    pub fn snapshot(&self, id: &str) -> Result<Snapshot> {
        let messages = self.get(&format!("/api/session/{id}/message?order=desc&limit=200"))?;
        let active = self.get("/api/session/active")?;
        let permissions = self
            .get(&format!("/api/session/{id}/permission"))
            .unwrap_or(Value::Null);
        Ok(Snapshot {
            entries: messages["data"]
                .as_array()
                .map(|m| m.iter().rev().flat_map(entries).collect())
                .unwrap_or_default(),
            running: active["data"].get(id).is_some(),
            permissions: permissions["data"]
                .as_array()
                .map(|p| {
                    p.iter()
                        .map(|p| Permission {
                            id: p["id"].as_str().unwrap_or("").into(),
                            action: p["action"].as_str().unwrap_or("").into(),
                            resources: p["resources"]
                                .as_array()
                                .map(|r| {
                                    r.iter()
                                        .filter_map(|x| x.as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default(),
                            message: p["message"].as_str().unwrap_or("").into(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

fn map_err(r: Result<ureq::Response, ureq::Error>) -> Result<ureq::Response> {
    match r {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or(body);
            Err(anyhow!("opencode {code}: {msg}"))
        }
        Err(e) => Err(anyhow!("opencode: {e}")),
    }
}

fn entries(m: &Value) -> Vec<Entry> {
    let mut out = Vec::new();
    match m["type"].as_str().unwrap_or("") {
        "user" => out.push(Entry::User(m["text"].as_str().unwrap_or("").into())),
        "shell" => out.push(Entry::Shell {
            command: m["command"].as_str().unwrap_or("").into(),
            status: m["status"].as_str().unwrap_or("").into(),
        }),
        "assistant" => {
            for c in m["content"].as_array().into_iter().flatten() {
                match c["type"].as_str().unwrap_or("") {
                    "text" => out.push(Entry::Text(c["text"].as_str().unwrap_or("").into())),
                    "reasoning" => {
                        let t = c["text"].as_str().unwrap_or("").trim();
                        if !t.is_empty() {
                            out.push(Entry::Reasoning(t.into()));
                        }
                    }
                    "tool" => out.push(Entry::Tool {
                        name: c["name"].as_str().unwrap_or("tool").into(),
                        summary: tool_summary(&c["state"]["input"]),
                        status: c["state"]["status"].as_str().unwrap_or("").into(),
                    }),
                    _ => {}
                }
            }
            if let Some(e) = m["error"]["message"].as_str() {
                out.push(Entry::Error(e.into()));
            }
        }
        _ => {}
    }
    out
}

/// The most telling input of a tool call: a command, a path or a pattern.
fn tool_summary(input: &Value) -> String {
    for key in [
        "command",
        "description",
        "filePath",
        "path",
        "pattern",
        "url",
        "query",
    ] {
        if let Some(s) = input[key].as_str() {
            let line = s.lines().next().unwrap_or("");
            return if line.chars().count() > 120 {
                format!("{}…", line.chars().take(120).collect::<String>())
            } else {
                line.into()
            };
        }
    }
    String::new()
}
