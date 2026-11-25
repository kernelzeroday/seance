//! Hermes agent: the `sessions` and `messages` tables of `~/.hermes/state.db`.
//! Resumed with `hermes --resume <id>` (hermes restores the working directory itself).

use std::path::PathBuf;

use anyhow::Result;

use super::{open_db, titleize, tool_line};
use crate::model::{Agent, Role, Session, Turn};

fn db() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::home().join(".hermes"))
        .join("state.db")
}

pub fn scan() -> Result<Vec<Session>> {
    let path = db();
    if !path.exists() {
        return Ok(vec![]);
    }
    let conn = open_db(&path)?;
    let mut stmt = conn.prepare(
        "SELECT s.id, s.title, s.display_name, s.cwd, s.git_branch, s.model, s.message_count,
                COALESCE(s.last_activity_at, s.ended_at, s.started_at),
                (SELECT content FROM messages m WHERE m.session_id = s.id AND m.role = 'user'
                   ORDER BY m.id LIMIT 1)
           FROM sessions s
          WHERE COALESCE(s.hidden, 0) = 0 AND COALESCE(s.message_count, 0) > 0",
    )?;
    let rows = stmt.query_map([], |r| {
        let title: Option<String> = r.get(1)?;
        let display: Option<String> = r.get(2)?;
        let first: Option<String> = r.get(8)?;
        let first = first.map(|f| titleize(&f)).unwrap_or_default();
        let title = title
            .or(display)
            .filter(|t| !t.trim().is_empty())
            .map(|t| titleize(&t));
        Ok(Session {
            agent: Agent::Hermes,
            id: r.get(0)?,
            prompt: if title.is_some() {
                first.clone()
            } else {
                String::new()
            },
            title: title.unwrap_or(first),
            cwd: r.get::<_, Option<String>>(3)?.map(PathBuf::from),
            branch: r.get(4)?,
            model: r.get(5)?,
            messages: r.get::<_, Option<i64>>(6)?.unwrap_or(0) as usize,
            updated: r.get::<_, Option<f64>>(7)?.unwrap_or(0.0) as i64,
            path: Some(path.clone()),
        })
    })?;
    Ok(rows.flatten().collect())
}

pub fn transcript(s: &Session) -> Result<Vec<Turn>> {
    let conn = open_db(&db())?;
    let mut stmt = conn.prepare(
        "SELECT role, content, tool_name, tool_calls FROM messages WHERE session_id = ?1
          ORDER BY timestamp, id",
    )?;
    let rows = stmt.query_map([&s.id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut turns = Vec::new();
    for (role, content, _tool, calls) in rows.flatten() {
        match role.as_str() {
            "user" => turns.push(Turn::new(Role::User, content)),
            "assistant" => {
                turns.push(Turn::new(Role::Assistant, content));
                if let Some(calls) = calls
                    && let Ok(serde_json::Value::Array(calls)) = serde_json::from_str(&calls)
                {
                    for c in calls {
                        let f = c.get("function").unwrap_or(&c);
                        let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        turns.push(Turn::new(Role::Tool, tool_line(name, f.get("arguments"))));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(turns)
}
