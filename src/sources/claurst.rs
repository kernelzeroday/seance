//! Claurst: `~/.claurst/sessions/<id>.json` (a snapshot of the whole conversation, rewritten
//! at turn end) and `<id>.jsonl` (a journal appended after every step). Loading takes the
//! snapshot and replays the journal entries numbered above its `journal_seq`, as Claurst
//! does; a session that crashed before its first snapshot is rebuilt from the journal alone.
//! Resumed with `claurst --resume <id>` from its working directory.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde_json::Value;

use super::{FileCache, content_text, each_json_line, mtime_secs, titleize, tool_line};
use crate::{
    model::{Agent, Role, Session, Turn},
    text::{is_scaffolding, strip_tags},
};

/// The first of `$CLAURST_HOME`, `~/.claurst` (if it exists), `$XDG_CONFIG_HOME/claurst`,
/// `~/.config/claurst` — Claurst's own order.
pub(super) fn root() -> PathBuf {
    if let Some(h) = std::env::var_os("CLAURST_HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(h);
    }
    let dot = super::home().join(".claurst");
    if dot.is_dir() {
        return dot;
    }
    match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(x) if x.is_absolute() => x.join("claurst"),
        _ => super::home().join(".config/claurst"),
    }
}

pub(super) fn sessions_dir() -> PathBuf {
    root().join("sessions")
}

/// Both files a session is read from; the cache fingerprints them together.
fn files(id_path: &Path) -> Vec<PathBuf> {
    vec![
        id_path.with_extension("json"),
        id_path.with_extension("jsonl"),
    ]
}

pub fn scan(cache: &FileCache) -> Result<Vec<Session>> {
    // One entry per session id, whichever of its files exist.
    let mut ids = BTreeSet::new();
    if let Ok(rd) = fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("json" | "jsonl")
            ) {
                ids.insert(p.with_extension(""));
            }
        }
    }
    Ok(cache.parse_all_with(ids.into_iter().collect(), files, parse))
}

/// The conversation as Claurst would load it: (metadata, messages).
fn load(id_path: &Path) -> Option<(Value, Vec<Value>)> {
    let snapshot: Option<Value> = fs::read(id_path.with_extension("json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let mut seq_done = 0;
    let (mut meta, mut messages) = match &snapshot {
        Some(s) => {
            seq_done = s.get("journal_seq").and_then(Value::as_u64).unwrap_or(0);
            let msgs = s
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            (s.clone(), msgs)
        }
        None => (Value::Null, Vec::new()),
    };
    let _ = each_json_line(&id_path.with_extension("jsonl"), |_, line| {
        let seq = line.get("seq").and_then(Value::as_u64).unwrap_or(0);
        if snapshot.is_some() && seq <= seq_done {
            return true;
        }
        let e = &line["entry"];
        match e.get("type").and_then(Value::as_str) {
            Some("meta") => {
                // Metadata only: keep what the snapshot had for messages.
                if let Some(s) = e.get("session") {
                    meta = s.clone();
                }
            }
            Some("message") => {
                if let Some(m) = e.get("message") {
                    messages.push(m.clone());
                }
            }
            Some("truncate") => {
                let len = e.get("len").and_then(Value::as_u64).unwrap_or(0) as usize;
                messages.truncate(len);
            }
            Some("checkpoint") => {
                if let Some(s) = e.get("session") {
                    messages = s
                        .get("messages")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    meta = s.clone();
                }
            }
            _ => {}
        }
        true
    });
    (!meta.is_null() || !messages.is_empty()).then_some((meta, messages))
}

/// What the user typed in a `user` message (None for tool results and scaffolding).
fn typed(m: &Value) -> Option<String> {
    let content = m.get("content")?;
    if let Value::Array(parts) = content
        && parts
            .iter()
            .any(|p| p.get("type").and_then(Value::as_str) == Some("tool_result"))
    {
        return None;
    }
    let t = strip_tags(&content_text(content));
    (!is_scaffolding(&t)).then_some(t)
}

fn parse(id_path: &Path) -> Option<Session> {
    let id = id_path.file_name()?.to_str()?.to_string();
    let (meta, messages) = load(id_path)?;
    let mut prompt = String::new();
    let mut count = 0;
    for m in &messages {
        match m.get("role").and_then(Value::as_str) {
            Some("user") => {
                if let Some(t) = typed(m) {
                    count += 1;
                    if prompt.is_empty() {
                        prompt = t;
                    }
                }
            }
            Some("assistant") => count += 1,
            _ => {}
        }
    }
    if prompt.is_empty() {
        return None;
    }
    let title = meta
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(titleize);
    // `updated_at` only moves when a turn ends; while Claurst works, only the journal
    // grows. Its last write is the live signal.
    let updated = meta
        .get("updated_at")
        .and_then(Value::as_str)
        .and_then(crate::text::parse_iso)
        .unwrap_or(0)
        .max(mtime_secs(&id_path.with_extension("jsonl")));
    Some(Session {
        agent: Agent::Claurst,
        prompt: if title.is_some() {
            titleize(&prompt)
        } else {
            String::new()
        },
        title: title.unwrap_or_else(|| titleize(&prompt)),
        cwd: meta
            .get("working_dir")
            .and_then(Value::as_str)
            .map(PathBuf::from),
        updated,
        messages: count,
        model: meta.get("model").and_then(Value::as_str).map(String::from),
        branch: None,
        path: Some(id_path.with_extension("json")),
        id,
    })
}

pub fn transcript(s: &Session) -> Result<Vec<Turn>> {
    let Some(path) = &s.path else {
        return Ok(vec![]);
    };
    let Some((_, messages)) = load(&path.with_extension("")) else {
        return Ok(vec![]);
    };
    let mut turns = Vec::new();
    for m in &messages {
        match m.get("role").and_then(Value::as_str) {
            Some("user") => {
                if let Some(t) = typed(m) {
                    turns.push(Turn::new(Role::User, t));
                }
            }
            Some("assistant") => match m.get("content") {
                Some(Value::String(t)) => turns.push(Turn::new(Role::Assistant, t.clone())),
                Some(Value::Array(parts)) => {
                    for p in parts {
                        match p.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = p.get("text").and_then(Value::as_str) {
                                    turns.push(Turn::new(Role::Assistant, t));
                                }
                            }
                            Some("tool_use") => {
                                let name = p.get("name").and_then(Value::as_str).unwrap_or("tool");
                                turns.push(Turn::new(Role::Tool, tool_line(name, p.get("input"))));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(turns)
}
