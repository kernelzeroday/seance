//! OpenAI Codex CLI: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, with thread names in
//! `~/.codex/session_index.jsonl`. Resumed with `codex resume <id>`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde_json::Value;

use super::{FileCache, content_text, each_json_line, files_with, mtime_secs, titleize, tool_line};
use crate::{
    model::{Agent, Role, Session, Turn},
    text::is_scaffolding,
};

fn root() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::home().join(".codex"))
}

pub fn scan(cache: &FileCache) -> Result<Vec<Session>> {
    let files = files_with(&root().join("sessions"), ".jsonl", 4);
    let mut sessions = cache.parse_all(files, parse);
    // Thread names change after the rollout is written; read them fresh each time.
    let mut names: HashMap<String, String> = HashMap::new();
    let _ = each_json_line(&root().join("session_index.jsonl"), |_, v| {
        if let (Some(id), Some(name)) = (
            v.get("id").and_then(Value::as_str),
            v.get("thread_name").and_then(Value::as_str),
        ) && !name.is_empty()
        {
            names.insert(id.to_string(), name.to_string());
        }
        true
    });
    for s in &mut sessions {
        if let Some(n) = names.get(&s.id) {
            if s.prompt.is_empty() {
                s.prompt = std::mem::take(&mut s.title);
            }
            s.title = titleize(n);
        }
    }
    Ok(sessions)
}

fn user_text(p: &Value) -> Option<String> {
    if p.get("type").and_then(Value::as_str) != Some("message")
        || p.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let Value::Array(parts) = p.get("content")? else {
        return None;
    };
    let text: Vec<String> = parts
        .iter()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .filter(|t| !is_scaffolding(t))
        .map(String::from)
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

fn parse(path: &Path) -> Option<Session> {
    let (mut id, mut cwd, mut model) = (None::<String>, None::<String>, None::<String>);
    let mut prompt = String::new();
    let mut messages = 0;
    let mut last_ts = None;
    let mut branch = None;
    let _ = each_json_line(path, |_, v| {
        let p = v.get("payload").unwrap_or(&Value::Null);
        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                id = p.get("id").and_then(Value::as_str).map(String::from);
                cwd = p.get("cwd").and_then(Value::as_str).map(String::from);
                branch = p
                    .pointer("/git/branch")
                    .and_then(Value::as_str)
                    .map(String::from);
            }
            Some("turn_context") => {
                if let Some(m) = p.get("model").and_then(Value::as_str) {
                    model = Some(m.to_string());
                }
            }
            Some("response_item") => {
                if let Some(t) = user_text(p) {
                    messages += 1;
                    if prompt.is_empty() {
                        prompt = t;
                    }
                } else if p.get("role").and_then(Value::as_str) == Some("assistant") {
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
    if prompt.is_empty() {
        return None;
    }
    Some(Session {
        agent: Agent::Codex,
        id,
        title: titleize(&prompt),
        prompt: String::new(),
        cwd: cwd.map(PathBuf::from),
        updated: last_ts
            .as_deref()
            .and_then(crate::text::parse_iso)
            .unwrap_or_else(|| mtime_secs(path)),
        messages,
        model,
        branch,
        path: Some(path.to_path_buf()),
    })
}

pub fn transcript(s: &Session) -> Result<Vec<Turn>> {
    let Some(path) = &s.path else {
        return Ok(vec![]);
    };
    let mut turns = Vec::new();
    each_json_line(path, |_, v| {
        if v.get("type").and_then(Value::as_str) != Some("response_item") {
            return true;
        }
        let p = &v["payload"];
        if let Some(t) = user_text(p) {
            turns.push(Turn::new(Role::User, t));
            return true;
        }
        match p.get("type").and_then(Value::as_str) {
            Some("message") if p.get("role").and_then(Value::as_str) == Some("assistant") => {
                turns.push(Turn::new(Role::Assistant, content_text(&p["content"])));
            }
            Some("function_call" | "custom_tool_call" | "local_shell_call") => {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("shell");
                let args = p
                    .get("arguments")
                    .or_else(|| p.get("input"))
                    .or_else(|| p.get("action"));
                turns.push(Turn::new(Role::Tool, tool_line(name, args)));
            }
            _ => {}
        }
        true
    })?;
    Ok(turns)
}
