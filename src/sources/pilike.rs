//! pi and oh-my-pi share a session format: `~/.pi/agent/sessions/<dir>/<time>_<id>.jsonl` and
//! `~/.omp/agent/sessions/<dir>/...`, a `session` header then `message` events. omp also keeps
//! a `title` line at the top. Resumed with `pi --session <file>` / `omp --resume <file>`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::{FileCache, content_text, each_json_line, files_with, mtime_secs, titleize, tool_line};
use crate::{
    model::{Agent, Role, Session, Turn},
    text::{is_scaffolding, strip_tags},
};

pub(super) fn root(agent: Agent) -> PathBuf {
    match agent {
        Agent::Pi => std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| super::home().join(".pi/agent/sessions")),
        _ => super::home().join(".omp/agent/sessions"),
    }
}

pub fn scan(agent: Agent, cache: &FileCache) -> Result<Vec<Session>> {
    let files = files_with(&root(agent), ".jsonl", 1);
    Ok(cache.parse_all(files, |p| parse(agent, p)))
}

fn user_text(v: &Value) -> Option<String> {
    let m = v.get("message")?;
    if m.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let t = strip_tags(&content_text(m.get("content")?));
    (!is_scaffolding(&t)).then_some(t)
}

fn parse(agent: Agent, path: &Path) -> Option<Session> {
    let (mut id, mut cwd, mut title, mut model) = (
        None::<String>,
        None::<String>,
        String::new(),
        None::<String>,
    );
    let mut prompt = String::new();
    let mut messages = 0;
    let mut last_ts = None;
    let _ = each_json_line(path, |_, v| {
        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                id = v.get("id").and_then(Value::as_str).map(String::from);
                cwd = v.get("cwd").and_then(Value::as_str).map(String::from);
            }
            Some("title" | "title_change" | "session_info") => {
                if let Some(t) = v
                    .get("title")
                    .or_else(|| v.get("name"))
                    .and_then(Value::as_str)
                {
                    title = t.trim().to_string();
                }
            }
            Some("model_change") => {
                model = v
                    .get("modelId")
                    .or_else(|| v.get("model"))
                    .and_then(Value::as_str)
                    .map(|m| m.rsplit('/').next().unwrap_or(m).to_string());
            }
            Some("message") => {
                if let Some(t) = user_text(&v) {
                    messages += 1;
                    if prompt.is_empty() {
                        prompt = t;
                    }
                } else if v.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                    messages += 1;
                }
            }
            _ => {}
        }
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str) {
            last_ts = Some(ts.to_string());
        }
        true
    });
    let id = id?;
    if prompt.is_empty() && title.is_empty() {
        return None;
    }
    let (title, prompt) = if title.is_empty() {
        (titleize(&prompt), String::new())
    } else {
        (titleize(&title), titleize(&prompt))
    };
    Some(Session {
        agent,
        id,
        title,
        prompt,
        cwd: cwd.map(PathBuf::from),
        updated: last_ts
            .as_deref()
            .and_then(crate::text::parse_iso)
            .unwrap_or_else(|| mtime_secs(path)),
        messages,
        model,
        branch: None,
        path: Some(path.to_path_buf()),
    })
}

pub fn transcript(s: &Session) -> Result<Vec<Turn>> {
    let Some(path) = &s.path else {
        return Ok(vec![]);
    };
    let mut turns = Vec::new();
    each_json_line(path, |_, v| {
        match v.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(t) = user_text(&v) {
                    turns.push(Turn::new(Role::User, t));
                } else if v.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
                    && let Some(Value::Array(parts)) = v.pointer("/message/content")
                {
                    for p in parts {
                        match p.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = p.get("text").and_then(Value::as_str) {
                                    turns.push(Turn::new(Role::Assistant, t));
                                }
                            }
                            Some("toolCall" | "tool_call" | "tool_use") => {
                                let name = p.get("name").and_then(Value::as_str).unwrap_or("tool");
                                let args = p.get("arguments").or_else(|| p.get("input"));
                                turns.push(Turn::new(Role::Tool, tool_line(name, args)));
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("model_change") => {
                if let Some(m) = v
                    .get("modelId")
                    .or_else(|| v.get("model"))
                    .and_then(Value::as_str)
                {
                    turns.push(Turn::new(Role::Info, format!("model → {m}")));
                }
            }
            _ => {}
        }
        true
    })?;
    Ok(turns)
}
