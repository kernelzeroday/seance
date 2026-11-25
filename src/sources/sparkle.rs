//! sparkle: `conversations` and `messages` in `~/.local/share/sparkle/sparkle.db`.
//! Resumed with `sparkle chat --cid <id>`. sparkle conversations have no working directory.

use std::path::PathBuf;

use anyhow::Result;

use super::{open_db, titleize};
use crate::model::{Agent, Role, Session, Turn};

pub(super) fn db() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| super::home().join(".local/share"))
        .join("sparkle/sparkle.db")
}

pub fn scan() -> Result<Vec<Session>> {
    let path = db();
    if !path.exists() {
        return Ok(vec![]);
    }
    let conn = open_db(&path)?;
    let mut stmt = conn.prepare(
        "SELECT c.id, c.title, c.model, c.created_ms,
                (SELECT max(created_ms) FROM messages m WHERE m.conversation_id = c.id),
                (SELECT count(*) FROM messages m WHERE m.conversation_id = c.id
                   AND m.role IN ('user', 'assistant')),
                (SELECT content FROM messages m WHERE m.conversation_id = c.id AND m.role = 'user'
                   ORDER BY created_ms LIMIT 1)
           FROM conversations c",
    )?;
    let rows = stmt.query_map([], |r| {
        let title: String = r.get(1)?;
        let first = r
            .get::<_, Option<String>>(6)?
            .map(|f| titleize(&f))
            .unwrap_or_default();
        let created: i64 = r.get(3)?;
        let last: Option<i64> = r.get(4)?;
        Ok(Session {
            agent: Agent::Sparkle,
            id: r.get(0)?,
            prompt: first,
            title: titleize(&title),
            cwd: None,
            updated: last.unwrap_or(created) / 1000,
            messages: r.get::<_, i64>(5)? as usize,
            model: Some(r.get(2)?),
            branch: None,
            path: Some(path.clone()),
        })
    })?;
    Ok(rows.flatten().filter(|s| s.messages > 0).collect())
}

pub fn transcript(s: &Session) -> Result<Vec<Turn>> {
    let conn = open_db(&db())?;
    let mut stmt = conn.prepare(
        "SELECT role, content FROM messages WHERE conversation_id = ?1 ORDER BY created_ms",
    )?;
    let rows = stmt.query_map([&s.id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    Ok(rows
        .flatten()
        .filter_map(|(role, text)| match role.as_str() {
            "user" => Some(Turn::new(Role::User, text)),
            "assistant" => Some(Turn::new(Role::Assistant, text)),
            _ => None,
        })
        .collect())
}
