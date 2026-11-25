//! Antigravity CLI (`agy`): conversation summaries in
//! `~/.gemini/antigravity-cli/cache/conversation_metadata.json`, typed prompts in
//! `history.jsonl`, and each conversation's steps as protobuf blobs in
//! `conversations/<id>.db`. Resumed with `agy --conversation <id>` from its workspace.
//!
//! The step blobs have no published schema, so the preview reads them loosely: prompts are
//! matched against the history file, assistant prose and tool arguments are the readable
//! strings of the planner steps.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
};

use anyhow::Result;
use serde_json::Value;

use super::{each_json_line, mtime_secs, open_db, titleize, tool_line};
use crate::model::{Agent, Role, Session, Turn};

fn root() -> PathBuf {
    super::home().join(".gemini").join("antigravity-cli")
}

/// conversationId → prompts typed into it, in order, with the workspace they were typed in.
fn history() -> HashMap<String, Vec<(i64, String, Option<String>)>> {
    let mut out: HashMap<String, Vec<(i64, String, Option<String>)>> = HashMap::new();
    let _ = each_json_line(&root().join("history.jsonl"), |_, v| {
        if let (Some(id), Some(text)) = (
            v.get("conversationId").and_then(Value::as_str),
            v.get("display").and_then(Value::as_str),
        ) {
            let ts = v.get("timestamp").and_then(Value::as_i64).unwrap_or(0) / 1000;
            let ws = v.get("workspace").and_then(Value::as_str).map(String::from);
            out.entry(id.to_string())
                .or_default()
                .push((ts, text.to_string(), ws));
        }
        true
    });
    out
}

pub fn scan() -> Result<Vec<Session>> {
    let root = root();
    let conv_dir = root.join("conversations");
    let mut sessions = Vec::new();
    let mut seen = HashSet::new();
    let meta: Value = fs::read(root.join("cache").join("conversation_metadata.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let hist = history();
    if let Some(convs) = meta.get("conversations").and_then(Value::as_object) {
        for (id, c) in convs {
            let s = &c["summary"];
            if s.get("Internal").and_then(Value::as_bool) == Some(true)
                || c.get("is_internal").and_then(Value::as_bool) == Some(true)
            {
                continue;
            }
            let title = s.get("Title").and_then(Value::as_str).unwrap_or("");
            let preview = s.get("Preview").and_then(Value::as_str).unwrap_or("");
            let cwd = s
                .get("WorkspaceURIs")
                .and_then(|w| w.get(0))
                .and_then(Value::as_str)
                .map(|u| PathBuf::from(u.trim_start_matches("file://")));
            let updated = s
                .get("UpdatedAt")
                .and_then(Value::as_str)
                .and_then(crate::text::parse_iso)
                .unwrap_or(0);
            let db = conv_dir.join(format!("{id}.db"));
            seen.insert(id.clone());
            sessions.push(Session {
                agent: Agent::Agy,
                id: id.clone(),
                title: titleize(if title.is_empty() { preview } else { title }),
                prompt: if title.is_empty() {
                    String::new()
                } else {
                    titleize(preview)
                },
                cwd,
                updated,
                messages: s.get("NumSteps").and_then(Value::as_u64).unwrap_or(0) as usize,
                model: None,
                branch: None,
                path: db.exists().then_some(db),
            });
        }
    }
    // Conversations the summary cache hasn't caught up with yet.
    if let Ok(rd) = fs::read_dir(&conv_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("db") {
                continue;
            }
            let Some(id) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            if seen.contains(&id) {
                continue;
            }
            let Some(prompts) = hist.get(&id) else {
                continue;
            };
            sessions.push(Session {
                agent: Agent::Agy,
                title: titleize(&prompts[0].1),
                prompt: String::new(),
                cwd: prompts.iter().find_map(|p| p.2.clone()).map(PathBuf::from),
                updated: prompts
                    .last()
                    .map(|p| p.0)
                    .filter(|t| *t > 0)
                    .unwrap_or_else(|| mtime_secs(&p)),
                messages: prompts.len(),
                model: None,
                branch: None,
                path: Some(p),
                id,
            });
        }
    }
    Ok(sessions)
}

// ---------------------------------------------------------------------------------------------
// A schema-less protobuf walk: every length-delimited field that is itself a valid message is
// descended into; the rest that are readable UTF-8 are collected.

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut r = 0u64;
    let mut shift = 0;
    loop {
        let c = *b.get(*i)?;
        *i += 1;
        r |= u64::from(c & 0x7f) << shift;
        if c < 0x80 {
            return Some(r);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Whether `b` parses fully as protobuf fields.
fn is_message(b: &[u8]) -> bool {
    let mut i = 0;
    while i < b.len() {
        let Some(key) = varint(b, &mut i) else {
            return false;
        };
        if key >> 3 == 0 {
            return false;
        }
        match key & 7 {
            0 => {
                if varint(b, &mut i).is_none() {
                    return false;
                }
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let Some(n) = varint(b, &mut i) else {
                    return false;
                };
                i += n as usize;
            }
            _ => return false,
        }
        if i > b.len() {
            return false;
        }
    }
    true
}

fn strings(b: &[u8], depth: usize, out: &mut Vec<String>) {
    let mut i = 0;
    while i < b.len() {
        let Some(key) = varint(b, &mut i) else { return };
        match key & 7 {
            0 => {
                if varint(b, &mut i).is_none() {
                    return;
                }
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let Some(n) = varint(b, &mut i) else { return };
                let end = i + n as usize;
                let Some(sub) = b.get(i..end) else { return };
                i = end;
                if depth < 16 && sub.len() > 1 && is_message(sub) {
                    strings(sub, depth + 1, out);
                } else if let Ok(s) = std::str::from_utf8(sub)
                    && !s
                        .chars()
                        .any(|c| c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
                {
                    out.push(s.to_string());
                }
            }
            _ => return,
        }
    }
}

fn looks_like_id(s: &str) -> bool {
    !s.contains(' ') || s.starts_with("file://") || s.starts_with("Created At:")
}

pub fn transcript(s: &Session) -> Result<Vec<Turn>> {
    let prompts: Vec<String> = history()
        .remove(&s.id)
        .unwrap_or_default()
        .into_iter()
        .map(|(_, t, _)| t)
        .collect();
    let Some(db) = s.path.as_ref().filter(|p| p.exists()) else {
        return Ok(prompts
            .into_iter()
            .map(|p| Turn::new(Role::User, p))
            .collect());
    };
    let conn = open_db(db)?;
    let mut stmt = conn.prepare("SELECT step_type, step_payload FROM steps ORDER BY idx")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
    })?;
    let prompt_set: HashSet<&str> = prompts.iter().map(String::as_str).collect();
    let mut used = HashSet::new();
    let mut turns = Vec::new();
    for row in rows.flatten() {
        let (ty, Some(payload)) = row else { continue };
        let mut found = Vec::new();
        strings(&payload, 0, &mut found);
        match ty {
            14 => {
                // The typed text is stored twice in a user step; history.jsonl confirms it
                // when it has the conversation.
                let twice = found
                    .iter()
                    .find(|f| !f.trim().is_empty() && found.iter().filter(|g| g == f).count() >= 2);
                let typed = found
                    .iter()
                    .find(|f| prompt_set.contains(f.as_str()))
                    .or(twice);
                if let Some(p) = typed
                    && used.insert(p.clone())
                {
                    turns.push(Turn::new(Role::User, p.clone()));
                }
            }
            15 => {
                let mut seen = HashSet::new();
                for f in found {
                    if !seen.insert(f.clone()) {
                        continue;
                    }
                    if f.starts_with('{') {
                        if let Ok(v) = serde_json::from_str::<Value>(&f) {
                            let name = v
                                .get("toolSummary")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            let arg = v
                                .get("CommandLine")
                                .or_else(|| v.get("AbsolutePath"))
                                .or_else(|| v.get("TargetFile"))
                                .or_else(|| v.get("Query"))
                                .cloned();
                            let line = match arg {
                                Some(Value::String(a)) => {
                                    format!("{name}  {}", crate::text::one_line(&a, 160))
                                }
                                _ => tool_line(&name, None),
                            };
                            turns.push(Turn::new(Role::Tool, line));
                        }
                    } else if f.len() >= 24 && !looks_like_id(&f) {
                        turns.push(Turn::new(Role::Assistant, f));
                    }
                }
            }
            _ => {}
        }
    }
    // Prompts the steps didn't show (older formats) still belong in the preview.
    if turns.iter().all(|t| t.role != Role::User) {
        let mut t: Vec<Turn> = prompts
            .into_iter()
            .map(|p| Turn::new(Role::User, p))
            .collect();
        t.extend(turns);
        return Ok(t);
    }
    Ok(turns)
}
