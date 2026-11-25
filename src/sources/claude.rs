//! Claude Code: `~/.claude/projects/<dir>/<session-id>.jsonl`, one event per line.
//! Resumed with `claude --resume <id>` from the session's working directory.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::{FileCache, content_text, each_json_line, files_with, mtime_secs, titleize, tool_line};
use crate::{
    model::{Agent, Role, Session, Turn},
    text::is_scaffolding,
};

fn root() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::home().join(".claude"))
        .join("projects")
}

pub fn scan(cache: &FileCache) -> Result<Vec<Session>> {
    let files = files_with(&root(), ".jsonl", 1);
    Ok(cache.parse_all(files, parse))
}

/// The text the user typed in a `user` event, or None for tool results and scaffolding.
fn typed(v: &Value) -> Option<String> {
    if v.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || v.get("isMeta").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let content = v.get("message")?.get("content")?;
    if let Value::Array(parts) = content
        && parts
            .iter()
            .any(|p| p.get("type").and_then(Value::as_str) == Some("tool_result"))
    {
        return None;
    }
    let text = content_text(content);
    let clean = crate::text::strip_tags(&text);
    if is_scaffolding(&clean) && !text.contains("<command-name>") {
        return None;
    }
    Some(clean)
}

fn parse(path: &Path) -> Option<Session> {
    let id = path.file_stem()?.to_str()?.to_string();
    let (mut prompt, mut ai_title, mut custom, mut summary) =
        (String::new(), String::new(), String::new(), String::new());
    let (mut cwd, mut branch, mut model) = (None::<String>, None::<String>, None::<String>);
    let mut messages = 0usize;
    let mut last_ts = None;
    let _ = each_json_line(path, |line, v| {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "user" => {
                if let Some(t) = typed(&v) {
                    messages += 1;
                    if prompt.is_empty() {
                        prompt = t;
                    }
                }
            }
            "assistant" => {
                if line.contains("\"type\":\"text\"") {
                    messages += 1;
                }
                if let Some(m) = v.pointer("/message/model").and_then(Value::as_str)
                    && !m.starts_with('<')
                {
                    model = Some(m.to_string());
                }
            }
            "ai-title" => {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                    ai_title = t.to_string();
                }
            }
            "custom-title" => {
                if let Some(t) = v
                    .get("customTitle")
                    .or_else(|| v.get("title"))
                    .and_then(Value::as_str)
                {
                    custom = t.to_string();
                }
            }
            "summary" => {
                if let Some(t) = v.get("summary").and_then(Value::as_str) {
                    summary = t.to_string();
                }
            }
            _ => {}
        }
        if cwd.is_none()
            && let Some(c) = v.get("cwd").and_then(Value::as_str)
        {
            cwd = Some(c.to_string());
        }
        if let Some(b) = v.get("gitBranch").and_then(Value::as_str)
            && b != "HEAD"
            && !b.is_empty()
        {
            branch = Some(b.to_string());
        }
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str) {
            last_ts = Some(ts.to_string());
        }
        true
    });
    if prompt.is_empty() {
        return None;
    }
    let title = [&custom, &ai_title, &summary, &prompt]
        .into_iter()
        .find(|s| !s.is_empty())
        .map(|s| titleize(s))
        .unwrap_or_default();
    let updated = last_ts
        .as_deref()
        .and_then(crate::text::parse_iso)
        .unwrap_or_else(|| mtime_secs(path));
    Some(Session {
        agent: Agent::Claude,
        id,
        prompt: if titleize(&prompt) == title {
            String::new()
        } else {
            titleize(&prompt)
        },
        title,
        cwd: cwd.map(PathBuf::from),
        updated,
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
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                if let Some(t) = typed(&v) {
                    turns.push(Turn::new(Role::User, t));
                }
            }
            Some("assistant") => {
                if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                    return true;
                }
                if let Some(Value::Array(parts)) = v.pointer("/message/content") {
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
            }
            _ => {}
        }
        true
    })?;
    Ok(turns)
}
