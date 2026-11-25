//! The folder picker for new sessions: browse the tree, or pick from the folders recent
//! sessions ran in. Typing filters; typing a path (`/…` or `~/…`) and Enter jumps there.

use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Browse,
    Recent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// "Start here": choose the folder being browsed.
    Here,
    Up,
    Dir(String),
    Recent(PathBuf),
}

pub enum Outcome {
    Stay,
    Chosen(PathBuf),
}

pub struct Browser {
    pub dir: PathBuf,
    pub mode: Mode,
    pub filter: String,
    pub cursor: usize,
    pub show_hidden: bool,
    pub error: Option<String>,
    subdirs: Vec<String>,
    recent: Vec<PathBuf>,
    rows: Vec<Row>,
}

impl Browser {
    pub fn new(start: &Path, recent: Vec<PathBuf>) -> Browser {
        let mut b = Browser {
            dir: start.to_path_buf(),
            mode: Mode::Browse,
            filter: String::new(),
            cursor: 0,
            show_hidden: false,
            error: None,
            subdirs: Vec::new(),
            recent: recent.into_iter().filter(|p| p.is_dir()).collect(),
            rows: Vec::new(),
        };
        b.load();
        b
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    fn load(&mut self) {
        self.error = None;
        self.subdirs = match fs::read_dir(&self.dir) {
            Ok(rd) => {
                let mut v: Vec<String> = rd
                    .flatten()
                    .filter(|e| {
                        // Follow symlinks: a link to a directory is a directory here.
                        e.path().is_dir()
                    })
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect();
                v.sort_by_key(|a| a.to_lowercase());
                v
            }
            Err(e) => {
                self.error = Some(e.to_string());
                Vec::new()
            }
        };
        self.update();
    }

    fn update(&mut self) {
        let f = self.filter.to_lowercase();
        let is_path = self.filter.starts_with('/') || self.filter.starts_with('~');
        let matches =
            |s: &str| is_path || f.split_whitespace().all(|w| s.to_lowercase().contains(w));
        self.rows = match self.mode {
            Mode::Browse => {
                let mut rows = vec![Row::Here];
                if self.dir.parent().is_some() && self.filter.is_empty() {
                    rows.push(Row::Up);
                }
                rows.extend(
                    self.subdirs
                        .iter()
                        .filter(|d| self.show_hidden || !d.starts_with('.') || !f.is_empty())
                        .filter(|d| matches(d))
                        .cloned()
                        .map(Row::Dir),
                );
                rows
            }
            Mode::Recent => self
                .recent
                .iter()
                .filter(|p| matches(&p.display().to_string()))
                .cloned()
                .map(Row::Recent)
                .collect(),
        };
        // With a filter, land on the first match rather than "start here".
        if !self.filter.is_empty() && self.mode == Mode::Browse && self.rows.len() > 1 && !is_path {
            self.cursor = self.cursor.max(1);
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    fn enter_dir(&mut self, dir: PathBuf) {
        let from = self
            .dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        let going_up = self.dir.parent() == Some(dir.as_path());
        self.dir = dir;
        self.filter.clear();
        self.mode = Mode::Browse;
        self.cursor = 0;
        self.load();
        // Going up lands on the folder we came from.
        if going_up
            && let Some(name) = from
            && let Some(i) = self.rows.iter().position(|r| *r == Row::Dir(name.clone()))
        {
            self.cursor = i;
        }
    }

    pub fn up(&mut self) {
        if let Some(p) = self.dir.parent() {
            self.enter_dir(p.to_path_buf());
        }
    }

    pub fn move_by(&mut self, d: isize) {
        let n = self.rows.len();
        if n > 0 {
            self.cursor = (self.cursor as isize + d).clamp(0, n as isize - 1) as usize;
        }
    }

    pub fn push(&mut self, c: char) {
        self.filter.push(c);
        self.cursor = 0;
        self.update();
    }

    /// Backspace: shortens the filter, or goes up a folder when there is none.
    pub fn pop(&mut self) {
        if self.filter.pop().is_none() {
            if self.mode == Mode::Browse {
                self.up();
            }
        } else {
            self.update();
        }
    }

    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.update();
    }

    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            Mode::Browse => Mode::Recent,
            Mode::Recent => Mode::Browse,
        };
        self.cursor = 0;
        self.update();
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.update();
    }

    /// The typed path, when the filter is one that names an existing folder.
    fn typed_dir(&self) -> Option<PathBuf> {
        let t = self.filter.trim();
        if !(t.starts_with('/') || t.starts_with('~')) {
            return None;
        }
        let p = crate::text::expand(t);
        p.is_dir().then_some(p)
    }

    /// →: opens the highlighted folder without choosing it.
    pub fn open(&mut self) {
        if let Some(p) = self.typed_dir() {
            return self.enter_dir(p);
        }
        match self.rows.get(self.cursor).cloned() {
            Some(Row::Dir(d)) => self.enter_dir(self.dir.join(d)),
            Some(Row::Recent(p)) => self.enter_dir(p),
            Some(Row::Up) => self.up(),
            _ => {}
        }
    }

    /// Enter: chooses "start here" or a recent folder, opens anything else.
    pub fn activate(&mut self) -> Outcome {
        if let Some(p) = self.typed_dir() {
            self.enter_dir(p);
            return Outcome::Stay;
        }
        match self.rows.get(self.cursor).cloned() {
            Some(Row::Here) => Outcome::Chosen(self.dir.clone()),
            Some(Row::Recent(p)) => Outcome::Chosen(p),
            Some(Row::Up) => {
                self.up();
                Outcome::Stay
            }
            Some(Row::Dir(d)) => {
                self.enter_dir(self.dir.join(d));
                Outcome::Stay
            }
            None => Outcome::Stay,
        }
    }

    /// The folder a new session would start in if chosen now (for the title bar).
    pub fn target(&self) -> PathBuf {
        match self.rows.get(self.cursor) {
            Some(Row::Recent(p)) => p.clone(),
            _ => self.dir.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!("seance-folders-{}", std::process::id()));
        for d in ["alpha/inner", "beta", ".hidden"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        root
    }

    #[test]
    fn browse_descend_and_choose() {
        let root = tree();
        let mut b = Browser::new(&root, vec![]);
        assert_eq!(
            b.rows(),
            &[
                Row::Here,
                Row::Up,
                Row::Dir("alpha".into()),
                Row::Dir("beta".into())
            ]
        );
        b.push('a');
        b.push('l');
        assert_eq!(b.rows()[b.cursor], Row::Dir("alpha".into()));
        assert!(matches!(b.activate(), Outcome::Stay));
        assert_eq!(b.dir, root.join("alpha"));
        b.pop(); // empty filter: up, landing on alpha
        assert_eq!(b.dir, root);
        assert_eq!(b.rows()[b.cursor], Row::Dir("alpha".into()));
        b.toggle_hidden();
        assert!(b.rows().contains(&Row::Dir(".hidden".into())));
        b.cursor = 0;
        assert!(matches!(b.activate(), Outcome::Chosen(p) if p == root));
        fs::remove_dir_all(root).unwrap();
    }
}
