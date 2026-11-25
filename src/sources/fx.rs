//! fx: `~/.fx/sessions/v2/index.jsonl` (the latest `put` per id is the session's summary) and
//! `~/.fx/sessions/v2/<id>/log.jsonl` (its events). Resumed with `fx session resume <id>` from
//! its workspace.

use std::{collections::HashMap, path::PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::{each_json_line, titleize, tool_line};
use crate::model::{Agent, Role, Session, Turn};

pub(super) fn root() -> PathBuf {
    super::home().join(".fx").join("sessions").join("v2")
}

pub fn scan() -> Result<Vec<Session>> {
    let root = root();
    let mut latest: HashMap<String, Value> = HashMap::new();
    let _ = each_json_line(&root.join("index.jsonl"), |_, v| {
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            match v.get("op").and_then(Value::as_str) {
                Some("put") => {
                    latest.insert(id.to_string(), v);
                }
                Some("delete" | "del" | "remove") => {
                    latest.remove(id);
                }
                _ => {}
            }
        }
        true
    });
    let mut out = Vec::new();
    for (id, v) in latest {
        // Subagents are listed under their parent; only top-level sessions resume.
        if v.get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| r != "root")
        {
            continue;
        }
        let turns = v.get("turns").and_then(Value::as_u64).unwrap_or(0);
        if turns == 0 {
            continue;
        }
        let log = root.join(&id).join("log.jsonl");
        let mut title = v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut model = None;
        let mut first = String::new();
        // The index is written once per turn; every log line has its own time.
        let mut last_ts = v.get("updated").and_then(Value::as_i64).unwrap_or(0);
        let _ = each_json_line(&log, |_, e| {
            if let Some(ts) = e.get("ts").and_then(Value::as_i64) {
                last_ts = last_ts.max(ts);
            }
            match (
                e.get("kind").and_then(Value::as_str),
                e.get("type").and_then(Value::as_str),
            ) {
                (Some("set"), _) if e.get("key").and_then(Value::as_str) == Some("prefs") => {
                    model = e
                        .pointer("/value/model")
                        .and_then(Value::as_str)
                        .map(String::from);
                }
                (Some("item"), Some("user")) if first.is_empty() => {
                    first = e
                        .pointer("/data/text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                }
                _ => {}
            }
            true
        });
        // fx titles are the first words of the prompt; show the whole prompt instead.
        if title.is_empty() || first.starts_with(title.trim_end_matches('…')) {
            title = first.clone();
            first.clear();
        }
        if title.is_empty() {
            continue;
        }
        out.push(Session {
            agent: Agent::Fx,
            title: titleize(&title),
            prompt: titleize(&first),
            cwd: v
                .get("workspace")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            updated: last_ts / 1000,
            messages: turns as usize * 2,
            model,
            branch: None,
            path: Some(log),
            id,
        });
    }
    Ok(out)
}

pub fn transcript(s: &Session) -> Result<Vec<Turn>> {
    let Some(path) = &s.path else {
        return Ok(vec![]);
    };
    let mut turns = Vec::new();
    each_json_line(path, |_, e| {
        if e.get("kind").and_then(Value::as_str) != Some("item") {
            return true;
        }
        let text = || {
            e.pointer("/data/text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match e.get("type").and_then(Value::as_str) {
            Some("user" | "steering") => turns.push(Turn::new(Role::User, text())),
            Some("assistant") => turns.push(Turn::new(Role::Assistant, text())),
            Some("tool_call") => {
                let name = e
                    .pointer("/data/tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                turns.push(Turn::new(
                    Role::Tool,
                    tool_line(name, e.pointer("/data/arguments_json")),
                ));
            }
            Some("interruption") => turns.push(Turn::new(Role::Info, "interrupted")),
            _ => {}
        }
        true
    })?;
    Ok(turns)
}
