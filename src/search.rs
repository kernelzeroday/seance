//! Full-text search over what was said in each session: the user's and the agent's messages,
//! not tool output or system prompts (those would make every word that ever passed through a
//! file listing match). The text is extracted once per session version and cached in
//! `~/.cache/seance/text-index.json`, so later searches and runs only re-read sessions that
//! changed. Matching is ASCII case-insensitive, so byte offsets in the lowered copy line up
//! with the original text for snippets.

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use serde::{Deserialize, Serialize};

use crate::{
    model::{Role, Session},
    sources,
};

/// The most text kept per session (the newest part is kept).
const MAX_DOC: usize = 2_000_000;
const VERSION: u32 = 1;

pub fn doc_key(s: &Session) -> String {
    format!("{}:{}", s.agent.key(), s.id)
}

/// What identifies a session's version: when it was last active and how many messages.
fn stamp(s: &Session) -> (i64, usize) {
    (s.updated, s.messages)
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Doc {
    stamp: (i64, usize),
    pub text: String,
    #[serde(skip)]
    pub lower: String,
}

impl Doc {
    fn new(stamp: (i64, usize), text: String) -> Doc {
        let lower = text.to_ascii_lowercase();
        Doc { stamp, text, lower }
    }
}

/// The searchable text of a session: its user and assistant messages, oldest first.
pub fn extract(s: &Session) -> String {
    let Ok(turns) = sources::raw_transcript(s) else {
        return String::new();
    };
    let mut out = String::new();
    for t in turns {
        if matches!(t.role, Role::User | Role::Assistant) && !t.text.trim().is_empty() {
            out.push_str(t.text.trim());
            out.push('\n');
        }
    }
    if out.len() > MAX_DOC {
        let mut cut = out.len() - MAX_DOC;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        out.drain(..cut);
    }
    out
}

#[derive(Default)]
pub struct Index {
    docs: HashMap<String, Doc>,
}

#[derive(Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    docs: HashMap<String, Doc>,
}

impl Index {
    fn path() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|d| d.cache_dir().join("seance").join("text-index.json"))
    }

    pub fn load() -> Index {
        let docs = Self::path()
            .and_then(|p| fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<IndexFile>(&b).ok())
            .filter(|f| f.version == VERSION)
            .map(|f| {
                f.docs
                    .into_iter()
                    .map(|(k, d)| (k, Doc::new(d.stamp, d.text)))
                    .collect()
            })
            .unwrap_or_default();
        Index { docs }
    }

    /// Writes the index atomically; the temp file is per process, so a TUI and a CLI run
    /// saving at once can't interleave.
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        let file = IndexFile {
            version: VERSION,
            docs: self.docs.clone(),
        };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        if let Ok(bytes) = serde_json::to_vec(&file)
            && fs::write(&tmp, bytes).is_ok()
        {
            let _ = fs::rename(&tmp, &path);
        }
    }

    pub fn get(&self, s: &Session) -> Option<&Doc> {
        self.docs.get(&doc_key(s)).filter(|d| d.stamp == stamp(s))
    }

    /// Sessions whose text is missing or out of date.
    pub fn stale<'a>(&self, sessions: &'a [Session]) -> Vec<&'a Session> {
        sessions.iter().filter(|s| self.get(s).is_none()).collect()
    }

    pub fn insert(&mut self, s: &Session, text: String) {
        self.docs.insert(doc_key(s), Doc::new(stamp(s), text));
    }

    /// Drops sessions that no longer exist.
    pub fn retain(&mut self, sessions: &[Session]) {
        let live: std::collections::HashSet<String> = sessions.iter().map(doc_key).collect();
        self.docs.retain(|k, _| live.contains(k));
    }

    /// Brings every session's text up to date, reading the stale ones in parallel.
    pub fn refresh(&mut self, sessions: &[Session]) -> usize {
        let stale = self.stale(sessions);
        let n = stale.len();
        for (s, text) in extract_all(&stale) {
            self.insert(s, text);
        }
        self.retain(sessions);
        n
    }
}

/// Extracts the text of many sessions in parallel.
pub fn extract_all<'a>(sessions: &[&'a Session]) -> Vec<(&'a Session, String)> {
    let next = AtomicUsize::new(0);
    let out = Mutex::new(Vec::with_capacity(sessions.len()));
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(sessions.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&s) = sessions.get(i) else { break };
                    let text = extract(s);
                    if let Ok(mut o) = out.lock() {
                        o.push((s, text));
                    }
                }
            });
        }
    });
    out.into_inner().unwrap_or_default()
}

/// The query's words, lowered the same way the text is.
pub fn words(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect()
}

/// How a session matched a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Every word is in the title, prompt, folder, model or id.
    Meta,
    /// At least one word is only in what was said.
    Text,
}

/// Every word must appear in the session's details or in its text.
pub fn hit(s: &Session, doc: Option<&Doc>, words: &[String]) -> Option<Hit> {
    let hay = s.haystack().to_ascii_lowercase();
    if words.iter().all(|w| hay.contains(w.as_str())) {
        return Some(Hit::Meta);
    }
    let lower = doc.map(|d| d.lower.as_str()).unwrap_or("");
    words
        .iter()
        .all(|w| hay.contains(w.as_str()) || lower.contains(w.as_str()))
        .then_some(Hit::Text)
}

/// A one-line excerpt around the most recent mention of the query (its longest word).
pub fn snippet(doc: &Doc, words: &[String], width: usize) -> Option<String> {
    let w = words.iter().max_by_key(|w| w.len())?;
    let at = doc.lower.rfind(w.as_str())?;
    let half = width.saturating_sub(w.len()) / 2;
    let mut start = at.saturating_sub(half);
    while !doc.text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (at + w.len() + half).min(doc.text.len());
    while !doc.text.is_char_boundary(end) {
        end += 1;
    }
    let body: String = doc.text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        // The text's own ellipses next to ours would read "……".
        .trim_matches('…')
        .to_string();
    let lead = if start > 0 { "…" } else { "" };
    let tail = if end < doc.text.len() { "…" } else { "" };
    Some(format!("{lead}{body}{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Agent;

    fn session(title: &str) -> Session {
        Session {
            agent: Agent::Claude,
            id: "id1".into(),
            title: title.into(),
            prompt: String::new(),
            cwd: None,
            updated: 1,
            messages: 2,
            model: None,
            branch: None,
            path: None,
        }
    }

    #[test]
    fn matches_title_or_text() {
        let s = session("Agent session selector TUI");
        let doc = Doc::new((1, 2), "we renamed it to Seance today\nnice".into());
        assert_eq!(hit(&s, Some(&doc), &words("selector")), Some(Hit::Meta));
        assert_eq!(hit(&s, Some(&doc), &words("seance")), Some(Hit::Text));
        // Words may be split between the details and the text.
        assert_eq!(hit(&s, Some(&doc), &words("tui SEANCE")), Some(Hit::Text));
        assert_eq!(hit(&s, Some(&doc), &words("seance nowhere")), None);
        assert_eq!(hit(&s, None, &words("seance")), None);
    }

    #[test]
    fn snippets_center_on_the_latest_mention() {
        let doc = Doc::new(
            (1, 2),
            "first seance mention\nlots of other words here and then the Seance again at the end"
                .into(),
        );
        let s = snippet(&doc, &words("seance"), 30).unwrap();
        assert!(s.contains("Seance again"), "{s}");
        assert!(s.starts_with('…'));
    }
}
