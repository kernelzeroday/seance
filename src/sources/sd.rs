//! sd (sd-tui): `~/.local/share/sd-tui/sessions/<id>.jsonl`, one line per image request.
//! Resumed with `sd --resume <id>`. `cli.jsonl` holds one-shot runs and isn't a session.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::{FileCache, each_json_line, files_with, mtime_secs, titleize};
use crate::model::{Agent, Role, Session, Turn};

pub(super) fn root() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| super::home().join(".local/share"))
        .join("sd-tui/sessions")
}

pub fn scan(cache: &FileCache) -> Result<Vec<Session>> {
    let files = files_with(&root(), ".jsonl", 0)
        .into_iter()
        .filter(|p| p.file_stem().and_then(|s| s.to_str()) != Some("cli"))
        .collect();
    Ok(cache.parse_all(files, parse))
}

fn parse(path: &Path) -> Option<Session> {
    let id = path.file_stem()?.to_str()?.to_string();
    let (mut first, mut last) = (String::new(), String::new());
    let (mut n, mut profile, mut dir, mut ts) = (0, None, None, None);
    let _ = each_json_line(path, |_, v| {
        if let Some(p) = v.get("prompt").and_then(Value::as_str) {
            n += 1;
            if first.is_empty() {
                first = p.to_string();
            }
            last = p.to_string();
        }
        profile = v
            .get("profile")
            .and_then(Value::as_str)
            .map(String::from)
            .or(profile.take());
        if let Some(p) = v.get("path").and_then(Value::as_str) {
            dir = Path::new(p).parent().map(Path::to_path_buf);
        }
        if let Some(t) = v.get("time").and_then(Value::as_str) {
            ts = crate::text::parse_iso(t);
        }
        true
    });
    if n == 0 {
        return None;
    }
    Some(Session {
        agent: Agent::Sd,
        id,
        title: titleize(&last),
        prompt: if first != last {
            titleize(&first)
        } else {
            String::new()
        },
        cwd: dir,
        updated: ts.unwrap_or_else(|| mtime_secs(path)),
        messages: n,
        model: profile,
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
        if let Some(p) = v.get("prompt").and_then(Value::as_str) {
            turns.push(Turn::new(Role::User, p));
            let status = v.get("status").and_then(Value::as_str).unwrap_or("");
            let mut out = Vec::new();
            if let Some(img) = v.get("path").and_then(Value::as_str) {
                out.push(crate::text::tilde(Path::new(img)));
            }
            if let Some(seed) = v.get("seed").and_then(Value::as_i64) {
                out.push(format!("seed {seed}"));
            }
            if let Some(e) = v.get("elapsed_s").and_then(Value::as_f64) {
                out.push(format!("{e:.0} s"));
            }
            if !status.is_empty() && status != "ok" {
                out.push(status.to_string());
            }
            turns.push(Turn::new(
                Role::Assistant,
                format!("🖼  {}", out.join(" · ")),
            ));
        }
        true
    })?;
    Ok(turns)
}
