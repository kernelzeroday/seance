//! goose: `~/.local/share/goose/sessions/sessions.db`. Resumed with
//! `goose session --resume --session-id <id>`.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use super::{content_text, open_db, titleize, tool_line};
use crate::model::{Agent, Role, Session, Turn};

fn db() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| super::home().join(".local/share"))
        .join("goose/sessions/sessions.db")
}

pub fn scan() -> Result<Vec<Session>> {
    let path = db();
    if !path.exists() {
        return Ok(vec![]);
    }
    let conn = open_db(&path)?;
    let mut stmt = conn.prepare(
        "SELECT s.id, s.name, s.description, s.working_dir, s.updated_at,
                (SELECT count(*) FROM messages m WHERE m.session_id = s.id),
                (SELECT content_json FROM messages m WHERE m.session_id = s.id AND m.role = 'user'
                   ORDER BY m.created_timestamp, m.id LIMIT 1),
                s.model_config_json
           FROM sessions s
          WHERE s.session_type = 'user' AND s.archived_at IS NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        let name: String = r.get(1)?;
        let description: String = r.get(2)?;
        let first: Option<String> = r.get(6)?;
        let first = first
            .and_then(|f| serde_json::from_str::<Value>(&f).ok())
            .map(|v| titleize(&content_text(&v)))
            .unwrap_or_default();
        let title = [name, description]
            .into_iter()
            .find(|t| !t.trim().is_empty())
            .map(|t| titleize(&t));
        let model = r
            .get::<_, Option<String>>(7)?
            .and_then(|m| serde_json::from_str::<Value>(&m).ok())
            .and_then(|m| {
                m.get("model_name")
                    .and_then(Value::as_str)
                    .map(String::from)
            });
        Ok(Session {
            agent: Agent::Goose,
            id: r.get(0)?,
            prompt: if title.is_some() {
                first.clone()
            } else {
                String::new()
            },
            title: title.unwrap_or(first),
            cwd: Some(PathBuf::from(r.get::<_, String>(3)?)),
            updated: r
                .get::<_, Option<String>>(4)?
                .as_deref()
                .and_then(crate::text::parse_sql_time)
                .unwrap_or(0),
            messages: r.get::<_, i64>(5)? as usize,
            model,
            branch: None,
            path: Some(path.clone()),
        })
    })?;
    Ok(rows.flatten().filter(|s| s.messages > 0).collect())
}

pub fn transcript(s: &Session) -> Result<Vec<Turn>> {
    let conn = open_db(&db())?;
    let mut stmt = conn.prepare(
        "SELECT role, content_json FROM messages WHERE session_id = ?1
          ORDER BY created_timestamp, id",
    )?;
    let rows = stmt.query_map([&s.id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut turns = Vec::new();
    for (role, json) in rows.flatten() {
        let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(&json) else {
            continue;
        };
        for p in &parts {
            match p.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let t = p.get("text").and_then(Value::as_str).unwrap_or("");
                    if role == "user" {
                        let t = crate::text::strip_tags(t);
                        if !crate::text::is_scaffolding(&t) {
                            turns.push(Turn::new(Role::User, t));
                        }
                    } else {
                        turns.push(Turn::new(Role::Assistant, t));
                    }
                }
                Some("toolRequest") => {
                    let call = p.pointer("/toolCall/value").unwrap_or(&Value::Null);
                    let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
                    turns.push(Turn::new(
                        Role::Tool,
                        tool_line(name, call.get("arguments")),
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(turns)
}
