//! Live updates without polling: inotify (through `notify`) wakes seance when a file in one of
//! the agents' session folders is written, created, renamed or removed. Each event is mapped
//! to the agent whose folder it is in and sent on; the picker debounces and rescans just that
//! agent. Opens, reads and closes are ignored (as are SQLite's `-shm` files), so seance
//! reading the stores never wakes itself.

use std::{path::Path, sync::mpsc::Sender};

use notify::{Event, EventKind, RecursiveMode, Watcher, event::ModifyKind};

use crate::{
    model::Agent,
    sources::{self, Watch},
};

/// Keeps the watches alive; dropping it stops them.
pub struct Live {
    _watcher: notify::RecommendedWatcher,
}

/// Whether an event can mean a session changed.
fn interesting(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        // Not "closed after writing": SQLite opens the -wal read-write even for read-only
        // connections, so seance's own scans would raise it and rescan forever. Real writes
        // always come with a Modify event anyway.
        _ => false,
    }
}

/// The agent a changed path belongs to, if any of the watches wants it.
fn owner(path: &Path, watches: &[(Agent, Watch)]) -> Option<Agent> {
    watches
        .iter()
        .filter(|(_, w)| {
            if w.recursive {
                path.starts_with(&w.dir)
            } else {
                path.parent() == Some(w.dir.as_path())
            }
        })
        .find(|(_, w)| (w.accept)(path))
        .map(|(a, _)| *a)
}

pub fn start(agents: &[Agent], tx: Sender<Agent>) -> Option<Live> {
    let watches: Vec<(Agent, Watch)> = agents
        .iter()
        .flat_map(|&a| sources::watches(a).into_iter().map(move |w| (a, w)))
        .filter(|(_, w)| w.dir.is_dir())
        .collect();
    let dirs: Vec<(std::path::PathBuf, bool)> = watches
        .iter()
        .map(|(_, w)| (w.dir.clone(), w.recursive))
        .collect();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let Ok(ev) = res else { return };
        if !interesting(&ev.kind) {
            return;
        }
        let mut seen = Vec::new();
        for p in &ev.paths {
            if let Some(a) = owner(p, &watches)
                && !seen.contains(&a)
            {
                seen.push(a);
                let _ = tx.send(a);
            }
        }
    })
    .map_err(|e| crate::syslog::warning(&format!("live updates off: {e}")))
    .ok()?;
    let mut watching = 0;
    for (dir, recursive) in dirs {
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        match watcher.watch(&dir, mode) {
            Ok(()) => watching += 1,
            Err(e) => crate::syslog::warning(&format!("can't watch {}: {e}", dir.display())),
        }
    }
    crate::syslog::info(&format!("live updates: watching {watching} folders"));
    (watching > 0).then_some(Live { _watcher: watcher })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode};

    #[test]
    fn events_map_to_their_agent() {
        let watches = vec![
            (
                Agent::Claude,
                Watch {
                    dir: "/h/.claude/projects".into(),
                    recursive: true,
                    accept: |p| p.extension().is_some_and(|e| e == "jsonl"),
                },
            ),
            (
                Agent::Hermes,
                Watch {
                    dir: "/h/.hermes".into(),
                    recursive: false,
                    accept: |p| {
                        let n = p.file_name().unwrap().to_str().unwrap();
                        n == "state.db" || n == "state.db-wal"
                    },
                },
            ),
        ];
        let o = |p: &str| owner(Path::new(p), &watches);
        assert_eq!(o("/h/.claude/projects/-x/abc.jsonl"), Some(Agent::Claude));
        assert_eq!(o("/h/.claude/projects/-x/memory/note.md"), None);
        assert_eq!(o("/h/.hermes/state.db-wal"), Some(Agent::Hermes));
        // Readers write -shm; it must never count.
        assert_eq!(o("/h/.hermes/state.db-shm"), None);
        assert_eq!(o("/h/.hermes/logs/state.db-wal"), None);
        assert!(!interesting(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!interesting(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(interesting(&EventKind::Modify(ModifyKind::Any)));
    }
}
