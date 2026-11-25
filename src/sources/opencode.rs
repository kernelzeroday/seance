//! OpenCode: `session_v2`, `session_message` and `session_part` in
//! `~/.local/share/opencode/opencode.db`. Resumed with `opencode --session <id>` from the
//! session's directory.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use super::{content_text, open_db, titleize, tool_line};
use crate::model::{Agent, Role, Session, Turn};

pub(super) fn db() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| super::home().join(".local/share"))
        .join("opencode/opencode.db")
}

pub fn scan() -> Result<Vec<Session>> {
    let path = db();
    if !path.exists() {
        return Ok(vec![]);
    }
    let conn = open_db(&path)?;
    let mut stmt = conn.prepare(
        "SELECT s.id, s.title, s.directory, s.time_updated, s.model,
                (SELECT count(*) FROM session_message m
                  WHERE m.session_id = s.id AND m.type IN ('user', 'assistant')),
                (SELECT data FROM session_message m WHERE m.session_id = s.id AND m.type = 'user'
                  ORDER BY m.seq LIMIT 1)
           FROM session_v2 s
          WHERE s.parent_id IS NULL AND s.time_archived IS NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        let title: Option<String> = r.get(1)?;
        let first = r
            .get::<_, Option<String>>(6)?
            .and_then(|d| serde_json::from_str::<Value>(&d).ok())
            .map(|d| titleize(d.get("text").and_then(Value::as_str).unwrap_or("")))
            .unwrap_or_default();
        let title = title.filter(|t| !t.trim().is_empty()).map(|t| titleize(&t));
        let model = r
            .get::<_, Option<String>>(4)?
            .and_then(|m| {
                serde_json::from_str::<Value>(&m)
                    .ok()
                    .or(Some(Value::String(m)))
            })
            .and_then(|m| match m {
                Value::String(s) => Some(s),
                v => v.get("id").and_then(Value::as_str).map(String::from),
            });
        Ok(Session {
            agent: Agent::Opencode,
            id: r.get(0)?,
            prompt: if title.is_some() {
                first.clone()
            } else {
                String::new()
            },
            title: title.unwrap_or(first),
            cwd: Some(PathBuf::from(r.get::<_, String>(2)?)),
            updated: r.get::<_, i64>(3)? / 1000,
            model,
            messages: r.get::<_, i64>(5)? as usize,
            branch: None,
            path: Some(path.clone()),
        })
    })?;
    Ok(rows.flatten().filter(|s| !s.title.is_empty()).collect())
}

pub fn transcript(s: &Session) -> Result<Vec<Turn>> {
    let conn = open_db(&db())?;
    let mut stmt = conn
        .prepare("SELECT id, type, data FROM session_message WHERE session_id = ?1 ORDER BY seq")?;
    let msgs: Vec<(String, String, String)> = stmt
        .query_map([&s.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .flatten()
        .collect();
    let mut parts_stmt =
        conn.prepare("SELECT type, data FROM session_part WHERE message_id = ?1 ORDER BY ordinal")?;
    let mut turns = Vec::new();
    for (id, ty, data) in msgs {
        let d: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        match ty.as_str() {
            "user" => {
                turns.push(Turn::new(
                    Role::User,
                    d.get("text").and_then(Value::as_str).unwrap_or(""),
                ));
            }
            "assistant" => {
                let mut any = false;
                for p in parts_stmt
                    .query_map([&id], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .flatten()
                {
                    let pd: Value = serde_json::from_str(&p.1).unwrap_or(Value::Null);
                    match p.0.as_str() {
                        "text" => {
                            any = true;
                            turns.push(Turn::new(
                                Role::Assistant,
                                pd.get("text").and_then(Value::as_str).unwrap_or(""),
                            ));
                        }
                        "tool" => {
                            any = true;
                            let name = pd.get("tool").and_then(Value::as_str).unwrap_or("tool");
                            turns.push(Turn::new(
                                Role::Tool,
                                tool_line(name, pd.pointer("/state/input")),
                            ));
                        }
                        _ => {}
                    }
                }
                if !any {
                    let t = content_text(d.get("content").unwrap_or(&Value::Null));
                    turns.push(Turn::new(Role::Assistant, t));
                }
                if let Some(e) = d.pointer("/error/message").and_then(Value::as_str) {
                    turns.push(Turn::new(
                        Role::Info,
                        format!("error: {}", crate::text::one_line(e, 200)),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(turns)
}
