//! Readers for each agent's session store. Every reader is read-only: seance never writes to
//! another program's files or databases.
//!
//! File-per-session stores (claude, codex, pi, omp, sd, fx) are parsed in parallel and the
//! results cached by path, size and modification time, so a rescan only reads what changed.

mod agy;
mod claude;
mod claurst;
mod codex;
mod fx;
mod goose;
mod hermes;
mod opencode;
mod pilike;
mod sd;
mod sparkle;

use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Agent, Role, Session, Turn};

/// The most turns a preview holds, newest kept.
pub const MAX_TURNS: usize = 400;
/// The longest a single turn's text gets in a preview.
pub const MAX_TURN_CHARS: usize = 6000;

pub fn home() -> PathBuf {
    crate::text::home().unwrap_or_else(|| "/".into())
}

/// Sessions of one agent, newest first is not required (the caller sorts).
pub fn scan(agent: Agent, cache: &FileCache) -> Result<Vec<Session>> {
    match agent {
        Agent::Claude => claude::scan(cache),
        Agent::Claurst => claurst::scan(cache),
        Agent::Codex => codex::scan(cache),
        Agent::Agy => agy::scan(),
        Agent::Omp => pilike::scan(Agent::Omp, cache),
        Agent::Pi => pilike::scan(Agent::Pi, cache),
        Agent::Hermes => hermes::scan(),
        Agent::Fx => fx::scan(),
        Agent::Goose => goose::scan(),
        Agent::Opencode => opencode::scan(),
        Agent::Sparkle => sparkle::scan(),
        Agent::Sd => sd::scan(cache),
    }
}

/// The conversation of `s`, oldest turn first, trimmed to [`MAX_TURNS`].
pub fn transcript(s: &Session) -> Result<Vec<Turn>> {
    let mut turns = match s.agent {
        Agent::Claude => claude::transcript(s),
        Agent::Claurst => claurst::transcript(s),
        Agent::Codex => codex::transcript(s),
        Agent::Agy => agy::transcript(s),
        Agent::Omp | Agent::Pi => pilike::transcript(s),
        Agent::Hermes => hermes::transcript(s),
        Agent::Fx => fx::transcript(s),
        Agent::Goose => goose::transcript(s),
        Agent::Opencode => opencode::transcript(s),
        Agent::Sparkle => sparkle::transcript(s),
        Agent::Sd => sd::transcript(s),
    }?;
    turns.retain(|t| !t.text.trim().is_empty());
    // Agents often split one reply into several events; show it as one turn.
    let mut merged: Vec<Turn> = Vec::with_capacity(turns.len());
    for t in turns {
        match merged.last_mut() {
            Some(prev) if prev.role == t.role && matches!(t.role, Role::Assistant | Role::Tool) => {
                prev.text
                    .push_str(if t.role == Role::Tool { "\n" } else { "\n\n" });
                prev.text.push_str(t.text.trim());
            }
            _ => merged.push(Turn {
                role: t.role,
                text: t.text.trim().to_string(),
            }),
        }
    }
    let mut turns = merged;
    for t in &mut turns {
        if t.text.chars().count() > MAX_TURN_CHARS {
            t.text = crate::text::clip(&t.text, MAX_TURN_CHARS);
        }
    }
    if turns.len() > MAX_TURNS {
        turns.drain(..turns.len() - MAX_TURNS);
    }
    Ok(turns)
}

// ---------------------------------------------------------------------------------------------
// What to watch for live updates

/// A directory whose changes can mean new or updated sessions of one agent.
pub struct Watch {
    pub dir: PathBuf,
    pub recursive: bool,
    /// Whether a changed path in `dir` is one the agent's sessions live in.
    pub accept: fn(&Path) -> bool,
}

fn name(p: &Path) -> &str {
    p.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

fn jsonl(p: &Path) -> bool {
    name(p).ends_with(".jsonl")
}

/// An SQLite database or its write-ahead log. Never `-shm`: readers (seance included) write
/// to it, so watching it would wake seance on its own reads.
fn sqlite(p: &Path, db: &str) -> bool {
    let n = name(p);
    n == db || n == format!("{db}-wal")
}

pub fn watches(agent: Agent) -> Vec<Watch> {
    let w = |dir: PathBuf, recursive: bool, accept: fn(&Path) -> bool| Watch {
        dir,
        recursive,
        accept,
    };
    match agent {
        Agent::Claude => vec![w(claude::root(), true, jsonl)],
        // The snapshot is replaced by rename and the journal appended; never the .lock.
        Agent::Claurst => vec![w(claurst::sessions_dir(), false, |p| {
            matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("json" | "jsonl")
            )
        })],
        Agent::Codex => vec![
            w(codex::root().join("sessions"), true, jsonl),
            w(codex::root(), false, |p| name(p) == "session_index.jsonl"),
        ],
        Agent::Agy => vec![
            w(agy::root().join("conversations"), false, |p| {
                name(p).ends_with(".db") || name(p).ends_with(".db-wal")
            }),
            w(agy::root().join("cache"), false, |p| {
                name(p) == "conversation_metadata.json"
            }),
            w(agy::root(), false, |p| name(p) == "history.jsonl"),
        ],
        Agent::Omp | Agent::Pi => vec![w(pilike::root(agent), true, jsonl)],
        Agent::Hermes => vec![w(parent(hermes::db()), false, |p| sqlite(p, "state.db"))],
        Agent::Fx => vec![w(fx::root(), true, |p| {
            matches!(name(p), "index.jsonl" | "log.jsonl")
        })],
        Agent::Goose => vec![w(parent(goose::db()), false, |p| sqlite(p, "sessions.db"))],
        Agent::Opencode => vec![w(parent(opencode::db()), false, |p| {
            sqlite(p, "opencode.db")
        })],
        Agent::Sparkle => vec![w(parent(sparkle::db()), false, |p| sqlite(p, "sparkle.db"))],
        Agent::Sd => vec![w(sd::root(), false, jsonl)],
    }
}

fn parent(p: PathBuf) -> PathBuf {
    p.parent().map(Path::to_path_buf).unwrap_or(p)
}

// ---------------------------------------------------------------------------------------------
// File cache

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    mtime: i128,
    size: u64,
    session: Option<Session>,
}

/// Parsed sessions keyed by file path, valid while the file's size and mtime match.
#[derive(Default)]
pub struct FileCache {
    old: HashMap<PathBuf, Entry>,
    new: Mutex<HashMap<PathBuf, Entry>>,
}

const CACHE_VERSION: u32 = 3;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: HashMap<PathBuf, Entry>,
}

impl FileCache {
    pub fn path() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|d| d.cache_dir().join("seance").join("index.json"))
    }

    pub fn load() -> FileCache {
        let old = Self::path()
            .and_then(|p| fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<CacheFile>(&b).ok())
            .filter(|c| c.version == CACHE_VERSION)
            .map(|c| c.entries)
            .unwrap_or_default();
        FileCache {
            old,
            new: Mutex::default(),
        }
    }

    /// Writes what is known: entries seen since loading override the loaded ones, and files
    /// that no longer exist fall away.
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        let mut entries = self.old.clone();
        if let Ok(m) = self.new.lock() {
            entries.extend(m.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        // Claurst entries are keyed by `<sessions>/<id>`, which is not itself a file.
        entries.retain(|p, _| {
            p.exists() || p.with_extension("json").exists() || p.with_extension("jsonl").exists()
        });
        let file = CacheFile {
            version: CACHE_VERSION,
            entries,
        };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec(&file)
            && fs::write(&tmp, bytes).is_ok()
        {
            let _ = fs::rename(&tmp, &path);
        }
    }

    /// The session parsed from `path`, from the cache when its files are unchanged. `deps`
    /// lists the files the session is read from (just `path` for most agents); the cache
    /// entry is valid while their newest mtime and total size stay the same.
    fn get(
        &self,
        path: &Path,
        deps: impl Fn(&Path) -> Vec<PathBuf>,
        parse: impl Fn(&Path) -> Option<Session>,
    ) -> Option<Session> {
        let (mut mtime, mut size, mut any) = (0i128, 0u64, false);
        for dep in deps(path) {
            let Ok(meta) = fs::metadata(&dep) else {
                continue;
            };
            any = true;
            let m = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i128)
                .unwrap_or(0);
            mtime = mtime.max(m);
            size += meta.len();
        }
        if !any {
            return None;
        }
        // A long-lived cache rescans many times: what it saw last beats what it loaded.
        let known = self
            .new
            .lock()
            .ok()
            .and_then(|m| m.get(path).cloned())
            .or_else(|| self.old.get(path).cloned());
        let entry = match known {
            Some(e) if e.mtime == mtime && e.size == size => e,
            _ => Entry {
                mtime,
                size,
                session: parse(path),
            },
        };
        let session = entry.session.clone();
        if let Ok(mut m) = self.new.lock() {
            m.insert(path.to_path_buf(), entry);
        }
        session
    }

    /// Parses every file in parallel through the cache.
    pub fn parse_all(
        &self,
        files: Vec<PathBuf>,
        parse: impl Fn(&Path) -> Option<Session> + Sync,
    ) -> Vec<Session> {
        self.parse_all_with(files, |p| vec![p.to_path_buf()], parse)
    }

    /// Like [`parse_all`](Self::parse_all), for sessions read from several files: `deps`
    /// names them for each key path.
    pub fn parse_all_with(
        &self,
        files: Vec<PathBuf>,
        deps: impl Fn(&Path) -> Vec<PathBuf> + Sync,
        parse: impl Fn(&Path) -> Option<Session> + Sync,
    ) -> Vec<Session> {
        let next = AtomicUsize::new(0);
        let out = Mutex::new(Vec::with_capacity(files.len()));
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(files.len().max(1));
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(f) = files.get(i) else { break };
                        if let Some(s) = self.get(f, &deps, &parse)
                            && let Ok(mut o) = out.lock()
                        {
                            o.push(s);
                        }
                    }
                });
            }
        });
        out.into_inner().unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers shared by the readers

pub fn mtime_secs(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Files under `dir` (recursively, to `depth` levels) whose name ends with `suffix`.
pub fn files_with(dir: &Path, suffix: &str, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, suffix: &str, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth > 0 {
                    walk(&p, suffix, depth - 1, out);
                }
            } else if p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(suffix))
            {
                out.push(p);
            }
        }
    }
    walk(dir, suffix, depth, &mut out);
    out
}

/// Calls `f` for each JSON line of a file, skipping lines that don't parse. `f` returns false
/// to stop early.
pub fn each_json_line(path: &Path, mut f: impl FnMut(&str, Value) -> bool) -> Result<()> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(l)
            && !f(l, v)
        {
            break;
        }
    }
    Ok(())
}

/// Text from a message `content` that is a string or an array of `{type, text}` parts.
pub fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p {
                Value::String(s) => Some(s.as_str()),
                _ => {
                    let ty = p.get("type").and_then(Value::as_str).unwrap_or("text");
                    if matches!(ty, "text" | "input_text" | "output_text") {
                        p.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A compact one-line description of a tool call: `name  arg-ish summary`.
pub fn tool_line(name: &str, input: Option<&Value>) -> String {
    let arg = input.and_then(|i| {
        let i = match i {
            // Some agents store arguments as a JSON string.
            Value::String(s) => {
                serde_json::from_str::<Value>(s).unwrap_or(Value::String(s.clone()))
            }
            other => other.clone(),
        };
        for k in [
            "command",
            "cmd",
            "file_path",
            "path",
            "pattern",
            "query",
            "url",
            "description",
            "prompt",
        ] {
            if let Some(v) = i.get(k) {
                let s = match v {
                    Value::String(s) => s.clone(),
                    Value::Array(a) => a
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => continue,
                };
                return Some(s);
            }
        }
        match i {
            Value::String(s) => Some(s),
            _ => None,
        }
    });
    match arg {
        Some(a) => format!("{name}  {}", crate::text::one_line(&a, 160)),
        None => name.to_string(),
    }
}

/// Opens an SQLite database read-only, without creating anything.
pub fn open_db(path: &Path) -> Result<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    let c = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {}", path.display()))?;
    c.busy_timeout(std::time::Duration::from_millis(500))?;
    Ok(c)
}

/// Turns a stored title or prompt into a list title.
pub fn titleize(s: &str) -> String {
    crate::text::one_line(&crate::text::strip_tags(s), 200)
}
