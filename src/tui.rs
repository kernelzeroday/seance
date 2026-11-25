//! The picker: agent tabs, a filterable session list, a live preview of the highlighted
//! session, and the new-session flow (agent menu, then an optional folder browser).

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{Receiver, Sender, channel},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    config::Config,
    folders::{self, Browser, Row},
    launch::{self, Plan},
    model::{Agent, Role, Session, Turn},
    select::{self, PLine, Pos},
    sources::{self, FileCache},
    text::{ago, fit, now, one_line, tilde, wrap},
    watch,
};

const ACCENT: Color = Color::Rgb(255, 160, 200);
const DIM: Color = Color::Rgb(120, 124, 140);
const FAINT: Color = Color::Rgb(70, 72, 84);
const SELECT_BG: Color = Color::Rgb(40, 41, 54);
const TEXT: Color = Color::Rgb(220, 222, 230);
/// Mouse-selected preview text.
const SELECT_TEXT_BG: Color = Color::Rgb(90, 70, 110);

type Key = (Agent, String);

/// A rendered preview: session, width, lines, and the transcript they came from.
type Rendered = (Key, u16, Arc<Vec<PLine>>, Arc<Vec<Turn>>);

/// Ctrl+C / Ctrl+D must be pressed twice within this long to quit.
const QUIT_WINDOW: Duration = Duration::from_millis(1500);
/// How long an ordinary notice stays in the footer.
const NOTICE_TTL: Duration = Duration::from_secs(6);

fn key(s: &Session) -> Key {
    (s.agent, s.id.clone())
}

pub struct Start {
    pub cfg: Config,
    pub agent: Option<Agent>,
    pub filter: String,
    pub here_only: bool,
    /// Open straight into the new-session menu.
    pub new: bool,
    pub launch_dir: PathBuf,
    /// Capture the mouse (clicks, wheel, drag-to-copy in the preview).
    pub mouse: bool,
}

enum Overlay {
    Agents(AgentMenu),
    Folder { agent: Agent, browser: Browser },
    Help,
}

struct AgentMenu {
    rows: Vec<Agent>,
    filter: String,
    cursor: usize,
}

impl AgentMenu {
    fn visible(&self) -> Vec<Agent> {
        let f = self.filter.to_lowercase();
        self.rows
            .iter()
            .copied()
            .filter(|a| {
                f.is_empty() || a.key().contains(&f) || a.describe().to_lowercase().contains(&f)
            })
            .collect()
    }
}

enum Preview {
    Loading,
    Ready(Arc<Vec<Turn>>),
    Failed(String),
}

/// Where the last frame put things, for mapping mouse positions back to them.
#[derive(Default, Clone)]
struct Hits {
    tabs: Vec<(Rect, Option<Agent>)>,
    /// The session rows (two screen lines each) and the first row shown.
    list: Rect,
    list_top: usize,
    /// The whole preview pane, and the transcript part of it.
    preview: Rect,
    body: Rect,
    body_lines: usize,
    /// An open menu's frame, its rows, and the index of the first row shown.
    overlay: Option<Rect>,
    rows: Option<(Rect, usize)>,
}

/// A drag selection in the preview, tied to the session it was made in.
struct Sel {
    key: Key,
    anchor: Pos,
    head: Pos,
    dragging: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Info,
    Bad,
}

struct App {
    cfg: Config,
    launch_dir: PathBuf,
    sessions: Vec<Session>,
    installed: HashMap<Agent, bool>,
    tab: Option<Agent>,
    filter: String,
    here_only: bool,
    /// Indices into `sessions`; row 0 of the list is the pinned "new session" row.
    visible: Vec<usize>,
    cursor: usize,
    list_top: usize,
    show_preview: bool,
    previews: HashMap<Key, Preview>,
    /// Lines scrolled up from the bottom of the preview.
    preview_scroll: usize,
    preview_lines: Option<Rendered>,
    hits: RefCell<Hits>,
    sel: Option<Sel>,
    /// One cache for the whole run, so live rescans only re-read files that changed.
    cache: Arc<FileCache>,
    /// The inotify watches; None when live updates are unavailable.
    live: Option<watch::Live>,
    live_rx: Receiver<Agent>,
    /// Agents with changes not yet rescanned: (first change, last change).
    dirty: HashMap<Agent, (Instant, Instant)>,
    /// Agents being scanned right now.
    in_flight: HashSet<Agent>,
    last_click: Option<(Instant, u16, u16)>,
    overlay: Option<Overlay>,
    status: Option<(String, Tone, Instant, Duration)>,
    scanning: usize,
    scan_rx: Receiver<(Agent, Result<Vec<Session>, String>)>,
    scan_tx: Sender<(Agent, Result<Vec<Session>, String>)>,
    want_tx: Sender<Session>,
    got_rx: Receiver<(Key, Result<Vec<Turn>, String>)>,
    plan: Option<Plan>,
    quit: bool,
    /// Set once the user moves the cursor; until then (and until the first scan is done)
    /// new scan results keep it on the top row.
    touched: bool,
    /// The first full scan has finished: from then on the cursor always stays on the same
    /// session while live updates reorder the list.
    settled: bool,
    /// The last Ctrl+C / Ctrl+D and when, so a second one in time quits.
    quit_armed: Option<(char, Instant)>,
}

pub fn run(start: Start) -> Result<Option<Plan>> {
    let (scan_tx, scan_rx) = channel();
    let (want_tx, want_rx) = channel::<Session>();
    let (got_tx, got_rx) = channel();
    std::thread::spawn(move || preview_worker(want_rx, got_tx));
    let (live_tx, live_rx) = channel();
    let agents: Vec<Agent> = Agent::ALL
        .into_iter()
        .filter(|a| !start.cfg.hidden(*a))
        .collect();
    let live = watch::start(&agents, live_tx);

    let installed = Agent::ALL
        .into_iter()
        .map(|a| (a, launch::which(&start.cfg.program(a)).is_some()))
        .collect();
    let mut app = App {
        show_preview: start.cfg.preview.unwrap_or(true),
        cfg: start.cfg,
        launch_dir: start.launch_dir,
        sessions: Vec::new(),
        installed,
        tab: start.agent,
        filter: start.filter,
        here_only: start.here_only,
        visible: Vec::new(),
        cursor: 1,
        list_top: 0,
        previews: HashMap::new(),
        preview_scroll: 0,
        preview_lines: None,
        hits: RefCell::default(),
        sel: None,
        cache: Arc::new(FileCache::load()),
        live,
        live_rx,
        dirty: HashMap::new(),
        in_flight: HashSet::new(),
        last_click: None,
        overlay: None,
        status: None,
        scanning: 0,
        scan_rx,
        scan_tx,
        want_tx,
        got_rx,
        plan: None,
        quit: false,
        touched: false,
        settled: false,
        quit_armed: None,
    };
    app.rescan();
    if start.new {
        app.open_new(start.agent);
    }

    // Draw on stderr so stdout stays free for `--print` (`eval "$(seance --print)"`).
    let mut terminal = init_terminal(start.mouse)?;
    let result = (|| -> Result<()> {
        // Redraw only when something changed: idle, seance just waits for input and
        // checks its channels a few times a second.
        let mut redraw = true;
        while !app.quit {
            redraw |= app.pump();
            // The scanning spinner animates.
            redraw |= app.scanning > 0;
            if redraw {
                terminal.draw(|f| app.draw(f))?;
                redraw = false;
            }
            let busy = app.scanning > 0 || !app.dirty.is_empty();
            let wait = Duration::from_millis(if busy { 60 } else { 250 });
            if event::poll(wait)? {
                redraw = true;
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.key(k),
                    Event::Mouse(m) => app.mouse(m),
                    Event::Paste(s) => {
                        for c in s.chars().filter(|c| !c.is_control()) {
                            app.type_char(c);
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    restore_terminal();
    app.cache.save();
    result?;
    Ok(app.plan)
}

/// Loads transcripts one at a time, always the most recently asked-for one first.
fn preview_worker(rx: Receiver<Session>, tx: Sender<(Key, Result<Vec<Turn>, String>)>) {
    while let Ok(mut s) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            s = newer;
        }
        let r = sources::transcript(&s).map_err(|e| format!("{e:#}"));
        if tx.send((key(&s), r)).is_err() {
            break;
        }
    }
}

impl App {
    /// Scans every agent (on start and ^R).
    fn rescan(&mut self) {
        let agents: Vec<Agent> = Agent::ALL
            .into_iter()
            .filter(|a| !self.cfg.hidden(*a))
            .collect();
        for a in agents {
            self.scan(a);
        }
    }

    /// Scans one agent in the background, unless it is being scanned already (then it is
    /// marked dirty and scanned again once that finishes).
    fn scan(&mut self, a: Agent) {
        if !self.in_flight.insert(a) {
            let now = Instant::now();
            self.dirty.entry(a).or_insert((now, now));
            return;
        }
        self.scanning += 1;
        let (tx, cache) = (self.scan_tx.clone(), self.cache.clone());
        std::thread::spawn(move || {
            let r = sources::scan(a, &cache).map_err(|e| format!("{e:#}"));
            let _ = tx.send((a, r));
        });
    }

    /// Live updates: collects change events and rescans each agent once its burst of
    /// writes settles (or at least every 1.5 s while it keeps writing).
    fn pump_live(&mut self) -> bool {
        let now = Instant::now();
        while let Ok(a) = self.live_rx.try_recv() {
            let e = self.dirty.entry(a).or_insert((now, now));
            e.1 = now;
        }
        let due: Vec<Agent> = self
            .dirty
            .iter()
            .filter(|(a, (first, last))| {
                !self.in_flight.contains(a)
                    && (last.elapsed() >= Duration::from_millis(300)
                        || first.elapsed() >= Duration::from_millis(1500))
            })
            .map(|(a, _)| *a)
            .collect();
        let any = !due.is_empty();
        for a in due {
            self.dirty.remove(&a);
            self.scan(a);
        }
        any
    }

    /// Takes in finished scans and transcripts.
    /// Returns whether anything changed that the screen should show.
    fn pump(&mut self) -> bool {
        let mut act = self.pump_live();
        let mut changed = false;
        let mut updated: Vec<Session> = Vec::new();
        // Which session is selected, read before the scan results below rebuild
        // `self.sessions`: afterwards the cursor's index points at a different session.
        let shown = self.selected().map(key);
        while let Ok((agent, r)) = self.scan_rx.try_recv() {
            act = true;
            self.scanning = self.scanning.saturating_sub(1);
            self.in_flight.remove(&agent);
            match r {
                Ok(list) => {
                    // Sessions that are new or changed since the last scan of this agent.
                    let before: HashMap<Key, (i64, usize)> = self
                        .sessions
                        .iter()
                        .filter(|s| s.agent == agent)
                        .map(|s| (key(s), (s.updated, s.messages)))
                        .collect();
                    updated.extend(
                        list.iter()
                            .filter(|s| before.get(&key(s)) != Some(&(s.updated, s.messages)))
                            .cloned(),
                    );
                    self.sessions.retain(|s| s.agent != agent);
                    self.sessions.extend(list);
                    changed = true;
                }
                Err(e) => crate::syslog::warning(&format!("{}: {e}", agent.key())),
            }
        }
        if !updated.is_empty() {
            for s in updated {
                let k = key(&s);
                if Some(&k) == shown.as_ref() {
                    // The open preview: fetch it again and swap it in when it arrives, so
                    // a running conversation streams in without a "loading…" flash.
                    if matches!(self.previews.get(&k), Some(Preview::Ready(_))) {
                        let _ = self.want_tx.send(s);
                    } else {
                        self.previews.remove(&k);
                    }
                } else {
                    self.previews.remove(&k);
                }
            }
        }
        if changed {
            let keep = shown.clone().filter(|_| self.touched || self.settled);
            let scroll = self.preview_scroll;
            self.sessions.sort_by_key(|s| std::cmp::Reverse(s.updated));
            self.refilter();
            match keep {
                Some(k) => {
                    if let Some(pos) = self
                        .visible
                        .iter()
                        .position(|&i| key(&self.sessions[i]) == k)
                    {
                        self.cursor = pos + 1;
                    }
                }
                None => {
                    self.cursor = 1.min(self.visible.len());
                    self.list_top = 0;
                }
            }
            // Same session still shown: a live update must not yank the preview's scroll.
            if self.selected().map(key) == shown {
                self.preview_scroll = scroll;
            }
            if self.scanning == 0 {
                self.settled = true;
            }
            // Stale previews of sessions that changed on disk.
            self.previews
                .retain(|_, p| !matches!(p, Preview::Failed(_)));
        }
        while let Ok((k, r)) = self.got_rx.try_recv() {
            act = true;
            self.previews.insert(
                k,
                match r {
                    Ok(t) => Preview::Ready(Arc::new(t)),
                    Err(e) => Preview::Failed(e),
                },
            );
        }
        if self.show_preview
            && let Some(s) = self.selected()
            && !self.previews.contains_key(&key(s))
        {
            act = true;
            let s = s.clone();
            self.previews.insert(key(&s), Preview::Loading);
            let _ = self.want_tx.send(s);
        }
        if let Some((_, _, t, ttl)) = &self.status
            && t.elapsed() > *ttl
        {
            self.status = None;
            act = true;
        }
        act
    }

    fn in_here(&self, s: &Session) -> bool {
        s.cwd
            .as_ref()
            .is_some_and(|c| c.starts_with(&self.launch_dir))
    }

    fn refilter(&mut self) {
        let words: Vec<String> = self
            .filter
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        self.visible = (0..self.sessions.len())
            .filter(|&i| {
                let s = &self.sessions[i];
                if self.tab.is_some_and(|t| t != s.agent) || (self.here_only && !self.in_here(s)) {
                    return false;
                }
                let hay = s.haystack();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect();
        self.cursor = self.cursor.min(self.visible.len());
        if self.cursor == 0 && !self.visible.is_empty() && !self.filter.is_empty() {
            self.cursor = 1;
        }
        self.preview_scroll = 0;
    }

    fn selected(&self) -> Option<&Session> {
        self.cursor
            .checked_sub(1)
            .and_then(|i| self.visible.get(i))
            .map(|&i| &self.sessions[i])
    }

    /// Agents with sessions, in the fixed order, for the tab bar.
    fn tabs(&self) -> Vec<(Option<Agent>, usize)> {
        let mut counts: HashMap<Agent, usize> = HashMap::new();
        for s in &self.sessions {
            if !self.here_only || self.in_here(s) {
                *counts.entry(s.agent).or_default() += 1;
            }
        }
        let total = counts.values().sum();
        let mut tabs = vec![(None, total)];
        for a in Agent::ALL {
            if let Some(&n) = counts.get(&a) {
                tabs.push((Some(a), n));
            } else if self.tab == Some(a) {
                tabs.push((Some(a), 0));
            }
        }
        tabs
    }

    fn cycle_tab(&mut self, d: isize) {
        let tabs = self.tabs();
        let pos = tabs.iter().position(|(a, _)| *a == self.tab).unwrap_or(0) as isize;
        let next = (pos + d).rem_euclid(tabs.len() as isize) as usize;
        self.tab = tabs[next].0;
        self.cursor = 1;
        self.list_top = 0;
        self.refilter();
    }

    fn notice(&mut self, msg: impl Into<String>, tone: Tone) {
        self.status = Some((msg.into(), tone, Instant::now(), NOTICE_TTL));
    }

    fn type_char(&mut self, c: char) {
        match &mut self.overlay {
            Some(Overlay::Agents(m)) => {
                m.filter.push(c);
                m.cursor = 0;
            }
            Some(Overlay::Folder { browser, .. }) => browser.push(c),
            Some(Overlay::Help) => self.overlay = None,
            None => {
                self.filter.push(c);
                self.cursor = 1;
                self.list_top = 0;
                self.refilter();
            }
        }
    }

    fn open_new(&mut self, prefer: Option<Agent>) {
        let mut rows: Vec<Agent> = Agent::ALL
            .into_iter()
            .filter(|a| !self.cfg.hidden(*a))
            .collect();
        // Installed agents first, keeping the usual order.
        rows.sort_by_key(|a| !self.installed.get(a).copied().unwrap_or(false));
        let cursor = prefer
            .and_then(|p| rows.iter().position(|a| *a == p))
            .unwrap_or(0);
        self.overlay = Some(Overlay::Agents(AgentMenu {
            rows,
            filter: String::new(),
            cursor,
        }));
    }

    fn recent_dirs(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        let mut out = vec![self.launch_dir.clone()];
        seen.insert(self.launch_dir.clone());
        for s in &self.sessions {
            if let Some(c) = &s.cwd
                && seen.insert(c.clone())
            {
                out.push(c.clone());
            }
            if out.len() >= 40 {
                break;
            }
        }
        out
    }

    fn start_new(&mut self, agent: Agent, dir: &Path) {
        if !self.installed.get(&agent).copied().unwrap_or(false) {
            self.notice(
                format!(
                    "{} is not installed (no `{}` on PATH)",
                    agent.key(),
                    self.cfg.program(agent)
                ),
                Tone::Bad,
            );
            return;
        }
        self.plan = Some(launch::new(&self.cfg, agent, dir));
        self.quit = true;
    }

    fn resume_selected(&mut self) {
        let Some(s) = self.selected().cloned() else {
            return self.open_new(self.tab);
        };
        if !self.installed.get(&s.agent).copied().unwrap_or(false) {
            let program = self.cfg.program(s.agent);
            return self.notice(
                format!(
                    "{} is not installed (no `{program}` on PATH)",
                    s.agent.key()
                ),
                Tone::Bad,
            );
        }
        if let Some(c) = &s.cwd
            && !c.is_dir()
        {
            self.notice(
                format!(
                    "{} is gone; resuming from {}",
                    tilde(c),
                    tilde(&self.launch_dir)
                ),
                Tone::Info,
            );
        }
        self.plan = Some(launch::resume(&self.cfg, &s, &self.launch_dir));
        self.quit = true;
    }

    // -----------------------------------------------------------------------------------------
    // Mouse

    fn mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        let hits = self.hits.borrow().clone();
        let at = Position { x, y };
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let d: isize = if m.kind == MouseEventKind::ScrollUp {
                    -1
                } else {
                    1
                };
                match &mut self.overlay {
                    Some(Overlay::Agents(menu)) => {
                        let n = menu.visible().len() as isize;
                        menu.cursor = (menu.cursor as isize + d).clamp(0, (n - 1).max(0)) as usize;
                    }
                    Some(Overlay::Folder { browser, .. }) => browser.move_by(d),
                    Some(Overlay::Help) => {}
                    None if hits.preview.contains(at) => {
                        self.preview_scroll = if d < 0 {
                            self.preview_scroll + 3
                        } else {
                            self.preview_scroll.saturating_sub(3)
                        };
                    }
                    None => self.select_row(self.cursor as isize + d),
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let double = self.last_click.is_some_and(|(t, cx, cy)| {
                    t.elapsed() < Duration::from_millis(400) && cy == y && cx.abs_diff(x) <= 1
                });
                self.last_click = Some((Instant::now(), x, y));
                if self.overlay.is_some() {
                    return self.click_overlay(at, double, &hits);
                }
                if let Some((_, tab)) = hits.tabs.iter().find(|(r, _)| r.contains(at)) {
                    self.tab = *tab;
                    self.cursor = 1;
                    self.list_top = 0;
                    self.refilter();
                    return;
                }
                if hits.list.contains(at) {
                    let idx = hits.list_top + ((y - hits.list.y) / 2) as usize;
                    if idx <= self.visible.len() {
                        if double && idx == self.cursor {
                            self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                        } else {
                            self.select_row(idx as isize);
                        }
                    }
                    return;
                }
                if hits.body.contains(at)
                    && let Some(k) = self.selected().map(key)
                {
                    let pos = self.preview_pos(&hits, x, y);
                    if double {
                        self.select_turn(k, pos.0);
                    } else {
                        self.sel = Some(Sel {
                            key: k,
                            anchor: pos,
                            head: pos,
                            dragging: true,
                        });
                    }
                    return;
                }
                self.sel = None;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if !self.sel.as_ref().is_some_and(|s| s.dragging) || hits.body.height == 0 {
                    return;
                }
                // Dragging past the top or bottom scrolls the transcript along.
                let body = hits.body;
                if y < body.y {
                    self.preview_scroll += 1;
                } else if y >= body.y + body.height {
                    self.preview_scroll = self.preview_scroll.saturating_sub(1);
                }
                let max_scroll = hits.body_lines.saturating_sub(body.height as usize);
                self.preview_scroll = self.preview_scroll.min(max_scroll);
                let yy = y.clamp(body.y, body.y + body.height - 1);
                let xx = x.clamp(body.x, body.x + body.width.saturating_sub(1));
                let pos = self.preview_pos(&hits, xx, yy);
                if let Some(sel) = &mut self.sel {
                    sel.head = pos;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(sel) = &mut self.sel else { return };
                if !sel.dragging {
                    return;
                }
                sel.dragging = false;
                if sel.anchor == sel.head {
                    // A plain click: no selection.
                    self.sel = None;
                } else {
                    self.copy_selection();
                }
            }
            _ => {}
        }
    }

    /// The transcript position under screen cell (x, y) of the preview body.
    fn preview_pos(&self, hits: &Hits, x: u16, y: u16) -> Pos {
        let h = hits.body.height as usize;
        let scroll = self.preview_scroll.min(hits.body_lines.saturating_sub(h));
        let start = hits.body_lines.saturating_sub(h + scroll);
        let line =
            (start + y.saturating_sub(hits.body.y) as usize).min(hits.body_lines.saturating_sub(1));
        (line, x.saturating_sub(hits.body.x))
    }

    fn select_row(&mut self, idx: isize) {
        let rows = self.visible.len() + 1;
        self.touched = true;
        self.sel = None;
        let idx = idx.clamp(0, rows as isize - 1) as usize;
        if idx != self.cursor {
            self.preview_scroll = 0;
        }
        self.cursor = idx;
    }

    fn select_turn(&mut self, k: Key, line: usize) {
        let Some((_, _, lines, _)) = &self.preview_lines else {
            return;
        };
        if let Some((a, b)) = select::turn_span(lines, line) {
            self.sel = Some(Sel {
                key: k,
                anchor: a,
                head: b,
                dragging: false,
            });
            self.copy_selection();
        }
    }

    fn copy_selection(&mut self) {
        let (Some(sel), Some((pk, _, lines, _))) = (&self.sel, &self.preview_lines) else {
            return;
        };
        if *pk != sel.key {
            return;
        }
        let text = select::extract(lines, sel.anchor, sel.head);
        if text.is_empty() {
            return;
        }
        let n = text.lines().count();
        match crate::clipboard::copy(&text) {
            Ok(via) => self.notice(
                format!("copied {n} line{} via {via}", if n == 1 { "" } else { "s" }),
                Tone::Info,
            ),
            Err(e) => self.notice(format!("copy failed: {e:#}"), Tone::Bad),
        }
    }

    fn click_overlay(&mut self, at: Position, double: bool, hits: &Hits) {
        if matches!(self.overlay, Some(Overlay::Help)) {
            self.overlay = None;
            return;
        }
        // A click outside the menu closes it.
        if hits.overlay.is_some_and(|r| !r.contains(at)) {
            self.overlay = None;
            return;
        }
        let Some((rows, top)) = hits.rows else { return };
        if !rows.contains(at) {
            return;
        }
        let idx = top + (at.y - rows.y) as usize;
        let hit = match &mut self.overlay {
            Some(Overlay::Agents(menu)) if idx < menu.visible().len() => {
                let same = menu.cursor == idx;
                menu.cursor = idx;
                same
            }
            Some(Overlay::Folder { browser, .. }) if idx < browser.rows().len() => {
                let same = browser.cursor == idx;
                browser.cursor = idx;
                same
            }
            _ => return,
        };
        if double && hit {
            self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
    }

    /// Quit on the second Ctrl+C (or Ctrl+D) in quick succession; the first one only says so.
    fn quit_key(&mut self, ch: char) {
        if self
            .quit_armed
            .is_some_and(|(prev, at)| prev == ch && at.elapsed() < QUIT_WINDOW)
        {
            self.quit = true;
            return;
        }
        self.quit_armed = Some((ch, Instant::now()));
        let key = ch.to_ascii_uppercase();
        self.status = Some((
            format!("press Ctrl+{key} again to quit"),
            Tone::Info,
            Instant::now(),
            QUIT_WINDOW,
        ));
    }

    /// Clears the filter of whatever has focus: an overlay's, or the list's.
    fn clear_filter(&mut self) {
        match &mut self.overlay {
            Some(Overlay::Agents(m)) => {
                m.filter.clear();
                m.cursor = 0;
            }
            Some(Overlay::Folder { browser, .. }) => browser.clear_filter(),
            Some(Overlay::Help) => {}
            None => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.refilter();
                }
            }
        }
    }

    fn copy_command(&mut self) {
        let Some(s) = self.selected() else { return };
        let cmd = launch::resume(&self.cfg, s, &self.launch_dir).shell();
        match crate::clipboard::copy(&cmd) {
            Ok(via) => self.notice(format!("copied via {via}: {cmd}"), Tone::Info),
            Err(e) => self.notice(format!("copy failed: {e:#}"), Tone::Bad),
        }
    }

    /// Copies the highlighted session's conversation, as the preview shows it.
    fn copy_preview(&mut self) {
        let Some(s) = self.selected().cloned() else {
            return;
        };
        let turns = match self.previews.get(&key(&s)) {
            Some(Preview::Ready(t)) => t.clone(),
            // Not loaded yet (or the preview is hidden): read it now.
            _ => match sources::transcript(&s) {
                Ok(t) => Arc::new(t),
                Err(e) => return self.notice(format!("can't read this session: {e:#}"), Tone::Bad),
            },
        };
        let text = crate::model::plain(&s, &turns);
        match crate::clipboard::copy(&text) {
            Ok(via) => self.notice(
                format!(
                    "copied the conversation ({} lines) via {via}",
                    text.lines().count()
                ),
                Tone::Info,
            ),
            Err(e) => self.notice(format!("copy failed: {e:#}"), Tone::Bad),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Keys

    fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && let KeyCode::Char(ch @ ('c' | 'd')) = k.code {
            // Like a shell: the first Ctrl+C clears what you typed.
            if ch == 'c' {
                self.clear_filter();
            }
            return self.quit_key(ch);
        }
        match self.overlay.take() {
            Some(Overlay::Help) => {}
            Some(Overlay::Agents(m)) => self.key_agents(m, k),
            Some(Overlay::Folder { agent, browser }) => self.key_folder(agent, browser, k),
            None => self.key_list(k),
        }
    }

    fn key_agents(&mut self, mut m: AgentMenu, k: KeyEvent) {
        let vis = m.visible();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return,
            KeyCode::Up => m.cursor = m.cursor.saturating_sub(1),
            KeyCode::Down => m.cursor = (m.cursor + 1).min(vis.len().saturating_sub(1)),
            KeyCode::Char('p') if ctrl => m.cursor = m.cursor.saturating_sub(1),
            KeyCode::Char('n') if ctrl => {
                m.cursor = (m.cursor + 1).min(vis.len().saturating_sub(1))
            }
            KeyCode::Backspace => {
                m.filter.pop();
                m.cursor = 0;
            }
            KeyCode::Enter => {
                if let Some(&a) = vis.get(m.cursor) {
                    let dir = self.launch_dir.clone();
                    self.start_new(a, &dir);
                    if self.quit {
                        return;
                    }
                }
            }
            KeyCode::Tab | KeyCode::Right => {
                if let Some(&a) = vis.get(m.cursor) {
                    let browser = Browser::new(&self.launch_dir, self.recent_dirs());
                    self.overlay = Some(Overlay::Folder { agent: a, browser });
                    return;
                }
            }
            KeyCode::Char(c) if !ctrl => {
                m.filter.push(c);
                m.cursor = 0;
            }
            _ => {}
        }
        self.overlay = Some(Overlay::Agents(m));
    }

    fn key_folder(&mut self, agent: Agent, mut b: Browser, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => {
                if b.filter.is_empty() {
                    self.open_new(Some(agent));
                    return;
                }
                b.clear_filter();
            }
            KeyCode::Up => b.move_by(-1),
            KeyCode::Down => b.move_by(1),
            KeyCode::PageUp => b.move_by(-10),
            KeyCode::PageDown => b.move_by(10),
            KeyCode::Home => b.cursor = 0,
            KeyCode::Right => b.open(),
            KeyCode::Left => b.up(),
            KeyCode::Backspace => b.pop(),
            KeyCode::Tab | KeyCode::BackTab => b.toggle_mode(),
            KeyCode::Char('t') if ctrl => b.toggle_hidden(),
            KeyCode::Char('s') if ctrl => {
                let dir = b.target();
                self.start_new(agent, &dir);
            }
            KeyCode::Enter => {
                if let folders::Outcome::Chosen(dir) = b.activate() {
                    self.start_new(agent, &dir);
                }
            }
            KeyCode::Char(c) if !ctrl => b.push(c),
            _ => {}
        }
        if !self.quit {
            self.overlay = Some(Overlay::Folder { agent, browser: b });
        }
    }

    fn key_list(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let rows = self.visible.len() + 1;
        let move_to = |app: &mut App, c: isize| {
            app.touched = true;
            app.sel = None;
            app.cursor = c.clamp(0, rows as isize - 1) as usize;
            app.preview_scroll = 0;
        };
        match k.code {
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    self.quit = true;
                } else {
                    self.filter.clear();
                    self.refilter();
                }
            }
            KeyCode::Enter => {
                if self.cursor == 0 {
                    self.open_new(self.tab);
                } else {
                    self.resume_selected();
                }
            }
            KeyCode::Up if shift => self.preview_scroll += 1,
            KeyCode::Down if shift => self.preview_scroll = self.preview_scroll.saturating_sub(1),
            KeyCode::Up => move_to(self, self.cursor as isize - 1),
            KeyCode::Down => move_to(self, self.cursor as isize + 1),
            KeyCode::Char('p') if ctrl => move_to(self, self.cursor as isize - 1),
            KeyCode::Char('n') if ctrl => {
                self.open_new(self.selected().map(|s| s.agent).or(self.tab))
            }
            KeyCode::Char('k') if ctrl => move_to(self, self.cursor as isize - 1),
            KeyCode::Char('j') if ctrl => move_to(self, self.cursor as isize + 1),
            KeyCode::Home => move_to(self, 0),
            KeyCode::End => move_to(self, rows as isize - 1),
            KeyCode::PageUp => self.preview_scroll += 15,
            KeyCode::PageDown => self.preview_scroll = self.preview_scroll.saturating_sub(15),
            KeyCode::Tab | KeyCode::Right => self.cycle_tab(1),
            KeyCode::BackTab | KeyCode::Left => self.cycle_tab(-1),
            KeyCode::Backspace => {
                self.filter.pop();
                self.refilter();
            }
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Char('o') if ctrl => {
                self.here_only = !self.here_only;
                self.refilter();
                self.notice(
                    if self.here_only {
                        format!("only sessions under {}", tilde(&self.launch_dir))
                    } else {
                        "sessions from every folder".to_string()
                    },
                    Tone::Info,
                );
            }
            KeyCode::Char('v') if ctrl => self.show_preview = !self.show_preview,
            KeyCode::Char('r') if ctrl => {
                self.previews.clear();
                self.rescan();
            }
            KeyCode::Char('y') if ctrl => self.copy_preview(),
            KeyCode::Char('e') if ctrl => self.copy_command(),
            KeyCode::Char('f') if ctrl => {
                // New session with the highlighted agent, choosing a folder.
                let agent = self
                    .selected()
                    .map(|s| s.agent)
                    .or(self.tab)
                    .unwrap_or(Agent::Claude);
                let start = self
                    .selected()
                    .and_then(|s| s.cwd.clone())
                    .filter(|d| d.is_dir())
                    .unwrap_or_else(|| self.launch_dir.clone());
                let browser = Browser::new(&start, self.recent_dirs());
                self.overlay = Some(Overlay::Folder { agent, browser });
            }
            KeyCode::F(1) => self.overlay = Some(Overlay::Help),
            KeyCode::Char('?') if self.filter.is_empty() => self.overlay = Some(Overlay::Help),
            KeyCode::Char(c) if !ctrl => self.type_char(c),
            _ => {}
        }
    }

    // -----------------------------------------------------------------------------------------
    // Drawing

    fn draw(&mut self, f: &mut Frame) {
        *self.hits.borrow_mut() = Hits::default();
        let area = f.area();
        f.render_widget(Block::new().style(Style::default().fg(TEXT)), area);
        let [header, tabs, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_header(f, header);
        self.draw_tabs(f, tabs);
        let wide = body.width >= 96;
        if self.show_preview && wide {
            let [list, preview] =
                Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)])
                    .areas(body);
            self.draw_list(f, list);
            self.draw_preview(f, preview);
        } else if self.show_preview && body.height >= 24 {
            let [list, preview] =
                Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(body);
            self.draw_list(f, list);
            self.draw_preview(f, preview);
        } else {
            self.draw_list(f, body);
        }
        self.draw_footer(f, footer);
        match &self.overlay {
            Some(Overlay::Agents(m)) => self.draw_agents(f, m),
            Some(Overlay::Folder { agent, browser }) => self.draw_folder(f, *agent, browser),
            Some(Overlay::Help) => {
                draw_help(f);
                self.hits.borrow_mut().overlay = Some(f.area());
            }
            None => {}
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let mut spans = vec![
            Span::styled(
                " seance ",
                Style::default().fg(Color::Black).bg(ACCENT).bold(),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{} sessions", self.sessions.len()),
                Style::default().fg(DIM),
            ),
        ];
        if self.scanning > 0 {
            let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let i = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as usize / 90)
                .unwrap_or(0)
                % spin.len();
            spans.push(Span::styled(
                format!("  {} scanning", spin[i]),
                Style::default().fg(ACCENT),
            ));
        }
        if self.live.is_some() {
            spans.push(Span::styled(
                "  ● live",
                Style::default().fg(Color::Rgb(120, 220, 150)),
            ));
        }
        let right = if self.here_only {
            format!("only under {} ", tilde(&self.launch_dir))
        } else {
            format!("{} ", tilde(&self.launch_dir))
        };
        let left_w: usize = spans.iter().map(|s| s.content.width()).sum();
        let pad = (area.width as usize).saturating_sub(left_w + right.width());
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(
            right,
            Style::default().fg(if self.here_only { ACCENT } else { DIM }),
        ));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let tabs = self.tabs();
        let labels: Vec<(String, Option<Agent>)> = tabs
            .iter()
            .map(|(a, n)| (format!(" {} {n} ", a.map(|a| a.key()).unwrap_or("all")), *a))
            .collect();
        // Scroll the bar so the active tab is visible.
        let active = tabs.iter().position(|(a, _)| *a == self.tab).unwrap_or(0);
        let width = area.width as usize;
        let mut first = 0;
        loop {
            let w: usize = labels[first..=active]
                .iter()
                .map(|(l, _)| l.width() + 1)
                .sum::<usize>()
                + 1;
            if w <= width || first == active {
                break;
            }
            first += 1;
        }
        let mut spans = vec![Span::raw(" ")];
        if first > 0 {
            spans[0] = Span::styled("‹", Style::default().fg(DIM));
        }
        let mut x = area.x + 1;
        for (i, (label, a)) in labels.iter().enumerate().skip(first) {
            let lw = label.width() as u16;
            if x < area.x + area.width {
                let width = lw.min(area.x + area.width - x);
                self.hits.borrow_mut().tabs.push((
                    Rect {
                        x,
                        y: area.y,
                        width,
                        height: 1,
                    },
                    *a,
                ));
            }
            x = x.saturating_add(lw + 1);
            let color = a.map(|a| a.color()).unwrap_or(ACCENT);
            let style = if i == active {
                Style::default().fg(Color::Black).bg(color).bold()
            } else {
                Style::default().fg(color)
            };
            spans.push(Span::styled(label.clone(), style));
            spans.push(Span::raw(" "));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(FAINT));
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height < 3 {
            return;
        }
        // Search line.
        let search = if self.filter.is_empty() {
            Line::from(vec![
                Span::styled(" ⌕ ", Style::default().fg(ACCENT)),
                Span::styled("type to filter", Style::default().fg(FAINT)),
            ])
        } else {
            let n = self.visible.len();
            Line::from(vec![
                Span::styled(" ⌕ ", Style::default().fg(ACCENT)),
                Span::styled(self.filter.clone(), Style::default().fg(TEXT).bold()),
                Span::styled("▏", Style::default().fg(ACCENT)),
                Span::styled(
                    format!("  {n} match{}", if n == 1 { "" } else { "es" }),
                    Style::default().fg(DIM),
                ),
            ])
        };
        f.render_widget(Paragraph::new(search), Rect { height: 1, ..inner });
        let list = Rect {
            y: inner.y + 2,
            height: inner.height.saturating_sub(2),
            ..inner
        };
        let per_page = (list.height as usize / 2).max(1);
        if self.cursor < self.list_top {
            self.list_top = self.cursor;
        } else if self.cursor >= self.list_top + per_page {
            self.list_top = self.cursor + 1 - per_page;
        }
        {
            let mut h = self.hits.borrow_mut();
            h.list = list;
            h.list_top = self.list_top;
        }
        let now = now();
        let w = list.width as usize;
        for (row, idx) in (self.list_top..=self.visible.len())
            .take(per_page)
            .enumerate()
        {
            let rect = Rect {
                y: list.y + row as u16 * 2,
                height: 2,
                ..list
            };
            let sel = idx == self.cursor;
            let bg = if sel { SELECT_BG } else { Color::Reset };
            let (bar_color, l1, l2) = if idx == 0 {
                (
                    ACCENT,
                    Line::from(vec![
                        Span::styled("＋ ", Style::default().fg(ACCENT).bold()),
                        Span::styled("New session", Style::default().fg(ACCENT).bold()),
                    ]),
                    Line::from(Span::styled(
                        "   any agent, here or in a folder you pick",
                        Style::default().fg(DIM),
                    )),
                )
            } else {
                let s = &self.sessions[self.visible[idx - 1]];
                let when = ago(s.updated, now);
                let title_w = w.saturating_sub(when.width() + 5);
                let title = fit(&s.title, title_w);
                let pad = w.saturating_sub(3 + title.width() + when.width() + 1);
                let installed = self.installed.get(&s.agent).copied().unwrap_or(false);
                let l1 = Line::from(vec![
                    Span::styled("● ", Style::default().fg(s.agent.color())),
                    Span::styled(
                        title,
                        if sel {
                            Style::default().fg(Color::White).bold()
                        } else {
                            Style::default().fg(TEXT)
                        },
                    ),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(when, Style::default().fg(DIM)),
                ]);
                let mut meta = vec![Span::styled(
                    format!("   {}", s.agent.key()),
                    Style::default().fg(if installed { s.agent.color() } else { DIM }),
                )];
                let mut rest = Vec::new();
                if let Some(c) = &s.cwd {
                    rest.push(tilde(c));
                }
                if let Some(b) = &s.branch {
                    rest.push(format!("⎇ {b}"));
                }
                if s.messages > 0 {
                    rest.push(format!("{} msg", s.messages));
                }
                if let Some(m) = &s.model {
                    rest.push(m.clone());
                }
                let rest = format!("  {}", rest.join(" · "));
                let used = meta[0].content.width();
                meta.push(Span::styled(
                    fit(&rest, w.saturating_sub(used + 1)),
                    Style::default().fg(DIM),
                ));
                (s.agent.color(), l1, Line::from(meta))
            };
            let bar = if sel { "▌" } else { " " };
            let lines = vec![
                Line::from(
                    std::iter::once(Span::styled(bar, Style::default().fg(bar_color)))
                        .chain(l1.spans)
                        .collect::<Vec<_>>(),
                ),
                Line::from(
                    std::iter::once(Span::styled(bar, Style::default().fg(bar_color)))
                        .chain(l2.spans)
                        .collect::<Vec<_>>(),
                ),
            ];
            f.render_widget(Paragraph::new(lines).style(Style::default().bg(bg)), rect);
        }
        if self.visible.is_empty() && self.scanning == 0 && list.height > 3 {
            let msg = if self.sessions.is_empty() {
                "no sessions found"
            } else {
                "nothing matches"
            };
            f.render_widget(
                Paragraph::new(Span::styled(format!("   {msg}"), Style::default().fg(DIM))),
                Rect {
                    y: list.y + 3,
                    height: 1,
                    ..list
                },
            );
        }
        // Scroll position.
        let total = self.visible.len() + 1;
        if total > per_page && area.height > 4 {
            let track = (area.height - 2) as usize;
            let pos = 1 + (self.list_top * track) / total;
            let len = ((per_page * track) / total).max(1);
            for i in 0..len.min(track) {
                let y = area.y + (pos + i).min(track) as u16;
                if let Some(c) = f.buffer_mut().cell_mut((area.x + area.width - 1, y)) {
                    c.set_symbol("┃").set_fg(DIM);
                }
            }
        }
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(FAINT));
        let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
        f.render_widget(block, area);
        self.hits.borrow_mut().preview = area;
        let Some(s) = self.selected().cloned() else {
            let lines = vec![
                Line::raw(""),
                Line::from(Span::styled(
                    "＋ New session",
                    Style::default().fg(ACCENT).bold(),
                )),
                Line::raw(""),
                Line::from(Span::styled(
                    "Enter picks an agent and starts it here.",
                    Style::default().fg(TEXT),
                )),
                Line::from(Span::styled(
                    "In that menu, Tab opens the folder browser instead.",
                    Style::default().fg(DIM),
                )),
                Line::raw(""),
                Line::from(Span::styled(
                    format!("here = {}", tilde(&self.launch_dir)),
                    Style::default().fg(DIM),
                )),
            ];
            f.render_widget(Paragraph::new(lines), inner);
            return;
        };
        let w = inner.width as usize;
        // Header: title, then the facts.
        let mut head: Vec<Line> = wrap(&s.title, w)
            .into_iter()
            .take(2)
            .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::White).bold())))
            .collect();
        let mut facts = vec![Span::styled(
            format!("● {}", s.agent.key()),
            Style::default().fg(s.agent.color()).bold(),
        )];
        let mut push = |t: String| {
            facts.push(Span::styled("  ·  ", Style::default().fg(FAINT)));
            facts.push(Span::styled(t, Style::default().fg(DIM)));
        };
        push(match ago(s.updated, now()).as_str() {
            "now" => "just now".to_string(),
            a => format!("{a} ago"),
        });
        if let Some(c) = &s.cwd {
            push(tilde(c));
        }
        if let Some(m) = &s.model {
            push(m.clone());
        }
        head.push(Line::from(facts));
        let plan = launch::resume(&self.cfg, &s, &self.launch_dir);
        head.push(Line::from(vec![
            Span::styled("↵ ", Style::default().fg(ACCENT)),
            Span::styled(
                fit(&plan.display(), w.saturating_sub(2)),
                Style::default().fg(FAINT),
            ),
        ]));
        head.push(Line::from(Span::styled(
            "─".repeat(w),
            Style::default().fg(FAINT),
        )));
        let head_h = head.len() as u16;
        f.render_widget(
            Paragraph::new(head),
            Rect {
                height: head_h.min(inner.height),
                ..inner
            },
        );
        let body = Rect {
            y: inner.y + head_h,
            height: inner.height.saturating_sub(head_h),
            ..inner
        };
        if body.height == 0 {
            return;
        }
        let k = key(&s);
        let note = |text: String, color: Color| {
            Arc::new(vec![PLine {
                line: Line::from(Span::styled(text, Style::default().fg(color))),
                text: String::new(),
                gutter: 0,
                width: 0,
                soft: false,
                turn: 0,
                header: false,
            }])
        };
        let lines = match self.previews.get(&k) {
            Some(Preview::Ready(turns)) => match &self.preview_lines {
                Some((pk, pw, l, src))
                    if *pk == k && *pw == body.width && Arc::ptr_eq(src, turns) =>
                {
                    l.clone()
                }
                old => {
                    let l = Arc::new(render_turns(turns, &s, body.width as usize));
                    match old {
                        // The same session grew (a live update): if the user had scrolled
                        // up, keep the same lines in view; the bottom keeps following.
                        Some((pk, pw, ol, _)) if *pk == k && *pw == body.width => {
                            if self.preview_scroll > 0 {
                                self.preview_scroll += l.len().saturating_sub(ol.len());
                            }
                        }
                        // A new layout invalidates any selection made on the old one.
                        _ => self.sel = None,
                    }
                    self.preview_lines = Some((k.clone(), body.width, l.clone(), turns.clone()));
                    l
                }
            },
            Some(Preview::Failed(e)) => note(format!("can't read this session: {e}"), Color::Red),
            _ => note("loading…".into(), DIM),
        };
        if self.sel.as_ref().is_some_and(|sel| sel.key != k) {
            self.sel = None;
        }
        let h = body.height as usize;
        let max_scroll = lines.len().saturating_sub(h);
        self.preview_scroll = self.preview_scroll.min(max_scroll);
        let start = lines.len().saturating_sub(h + self.preview_scroll);
        let shown: Vec<Line> = lines
            .iter()
            .skip(start)
            .take(h)
            .map(|l| l.line.clone())
            .collect();
        f.render_widget(Paragraph::new(shown), body);
        if let Some(sel) = &self.sel {
            for i in start..(start + h).min(lines.len()) {
                if let Some((c0, c1)) = select::highlight(&lines, sel.anchor, sel.head, i) {
                    let y = body.y + (i - start) as u16;
                    for c in c0..c1.min(body.width) {
                        if let Some(cell) = f.buffer_mut().cell_mut((body.x + c, y)) {
                            cell.set_bg(SELECT_TEXT_BG).set_fg(Color::White);
                        }
                    }
                }
            }
        }
        {
            let mut hits = self.hits.borrow_mut();
            hits.body = body;
            hits.body_lines = lines.len();
        }
        if self.preview_scroll > 0 {
            let tag = format!(" ↑{} ", self.preview_scroll);
            let x = area.x + area.width.saturating_sub(tag.width() as u16 + 2);
            f.render_widget(
                Paragraph::new(Span::styled(tag, Style::default().fg(ACCENT))),
                Rect {
                    x,
                    y: area.y + area.height - 1,
                    width: 8,
                    height: 1,
                },
            );
        }
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        if let Some((msg, tone, _, _)) = &self.status {
            let color = if *tone == Tone::Bad {
                Color::Red
            } else {
                ACCENT
            };
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}", one_line(msg, 300)),
                    Style::default().fg(color),
                )),
                area,
            );
            return;
        }
        let keys: &[(&str, &str)] = match &self.overlay {
            Some(Overlay::Agents(_)) => {
                &[("↵", "start here"), ("⇥", "pick folder"), ("esc", "back")]
            }
            Some(Overlay::Folder { .. }) => &[
                ("↵", "open / choose"),
                ("→←", "in / up"),
                ("⇥", "recent"),
                ("^S", "start in this one"),
                ("^T", "hidden"),
                ("esc", "back"),
            ],
            _ => &[
                ("↵", "resume"),
                ("^N", "new"),
                ("^F", "new in folder"),
                ("⇥", "agent"),
                ("^O", "here only"),
                ("PgUp/Dn", "scroll"),
                ("drag", "copy"),
                ("^Y", "copy all"),
                ("^V", "preview"),
                ("?", "help"),
            ],
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, d) in keys {
            spans.push(Span::styled(*k, Style::default().fg(ACCENT)));
            spans.push(Span::styled(format!(" {d}   "), Style::default().fg(DIM)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_agents(&self, f: &mut Frame, m: &AgentMenu) {
        let vis = m.visible();
        let counts: HashMap<Agent, usize> =
            self.sessions.iter().fold(HashMap::new(), |mut h, s| {
                *h.entry(s.agent).or_default() += 1;
                h
            });
        let area = centered(f.area(), 64, vis.len() as u16 + 5);
        f.render_widget(Clear, area);
        let title = format!(" new session in {} ", tilde(&self.launch_dir));
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(title, Style::default().fg(ACCENT).bold()));
        let inner = block.inner(area);
        f.render_widget(block, area);
        {
            let mut hits = self.hits.borrow_mut();
            hits.overlay = Some(area);
            let rows = Rect {
                y: inner.y + 2,
                height: inner.height.saturating_sub(2),
                ..inner
            };
            hits.rows = Some((rows, 0));
        }
        let mut lines = vec![Line::from(vec![
            Span::styled(" ⌕ ", Style::default().fg(ACCENT)),
            if m.filter.is_empty() {
                Span::styled("agent", Style::default().fg(FAINT))
            } else {
                Span::styled(m.filter.clone(), Style::default().bold())
            },
        ])];
        lines.push(Line::raw(""));
        for (i, a) in vis.iter().enumerate() {
            let sel = i == m.cursor;
            let ok = self.installed.get(a).copied().unwrap_or(false);
            let n = counts.get(a).copied().unwrap_or(0);
            let style = if sel {
                Style::default().bg(SELECT_BG)
            } else {
                Style::default()
            };
            let right = if !ok {
                "not installed".to_string()
            } else if n > 0 {
                format!("{n} sessions")
            } else {
                String::new()
            };
            let left = format!("{:<9} {}", a.key(), a.describe());
            let pad = (inner.width as usize).saturating_sub(4 + left.width() + right.width() + 1);
            lines.push(
                Line::from(vec![
                    Span::styled(if sel { "▌" } else { " " }, Style::default().fg(a.color())),
                    Span::styled(
                        "● ",
                        Style::default().fg(if ok { a.color() } else { FAINT }),
                    ),
                    Span::styled(
                        left,
                        if !ok {
                            Style::default().fg(FAINT)
                        } else if sel {
                            Style::default().fg(Color::White).bold()
                        } else {
                            Style::default().fg(TEXT)
                        },
                    ),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(right, Style::default().fg(DIM)),
                ])
                .style(style),
            );
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_folder(&self, f: &mut Frame, agent: Agent, b: &Browser) {
        let fa = f.area();
        let area = centered(
            fa,
            fa.width.saturating_sub(8).min(90),
            fa.height.saturating_sub(4).min(30),
        );
        f.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(agent.color()))
            .title(Line::from(vec![
                Span::styled(
                    format!(" ● {} ", agent.key()),
                    Style::default().fg(agent.color()).bold(),
                ),
                Span::styled("new session in ", Style::default().fg(DIM)),
                Span::styled(
                    format!("{} ", tilde(&b.target())),
                    Style::default().fg(Color::White).bold(),
                ),
            ]));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let w = inner.width as usize;
        let tab = |name: &str, on: bool| {
            if on {
                Span::styled(
                    format!(" {name} "),
                    Style::default().fg(Color::Black).bg(agent.color()).bold(),
                )
            } else {
                Span::styled(format!(" {name} "), Style::default().fg(DIM))
            }
        };
        let mut lines = vec![
            Line::from(vec![
                Span::raw(" "),
                tab("browse", b.mode == folders::Mode::Browse),
                Span::raw(" "),
                tab("recent", b.mode == folders::Mode::Recent),
                Span::styled(
                    if b.show_hidden {
                        "   showing hidden"
                    } else {
                        ""
                    },
                    Style::default().fg(FAINT),
                ),
            ]),
            Line::from(vec![
                Span::styled(" ⌕ ", Style::default().fg(ACCENT)),
                if b.filter.is_empty() {
                    Span::styled(
                        "filter, or type a path like ~/src",
                        Style::default().fg(FAINT),
                    )
                } else {
                    Span::styled(b.filter.clone(), Style::default().bold())
                },
            ]),
            Line::raw(""),
        ];
        if let Some(e) = &b.error {
            lines.push(Line::from(Span::styled(
                format!(" {e}"),
                Style::default().fg(Color::Red),
            )));
        }
        let room = (inner.height as usize).saturating_sub(lines.len());
        let top = b.cursor.saturating_sub(room.saturating_sub(1));
        {
            let mut hits = self.hits.borrow_mut();
            hits.overlay = Some(area);
            let rows = Rect {
                y: inner.y + lines.len() as u16,
                height: room as u16,
                ..inner
            };
            hits.rows = Some((rows, top));
        }
        for (i, row) in b.rows().iter().enumerate().skip(top).take(room) {
            let sel = i == b.cursor;
            let (icon, text, color) = match row {
                Row::Here => ("✓ ", format!("start here   {}", tilde(&b.dir)), ACCENT),
                Row::Up => ("↰ ", "..".to_string(), DIM),
                Row::Dir(d) => ("▸ ", format!("{d}/"), TEXT),
                Row::Recent(p) => ("◷ ", tilde(p), TEXT),
            };
            let style = if sel {
                Style::default().fg(Color::White).bg(SELECT_BG).bold()
            } else {
                Style::default().fg(color)
            };
            lines.push(
                Line::from(vec![
                    Span::styled(
                        if sel { "▌" } else { " " },
                        Style::default().fg(agent.color()),
                    ),
                    Span::styled(
                        icon,
                        Style::default().fg(if sel { agent.color() } else { color }),
                    ),
                    Span::styled(fit(&text, w.saturating_sub(4)), style),
                ])
                .style(if sel {
                    Style::default().bg(SELECT_BG)
                } else {
                    Style::default()
                }),
            );
        }
        f.render_widget(Paragraph::new(lines), inner);
    }
}

type Term = ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stderr>>;

fn init_terminal(mouse: bool) -> Result<Term> {
    use crossterm::{
        event::{EnableBracketedPaste, EnableMouseCapture},
        execute,
        terminal::{EnterAlternateScreen, enable_raw_mode},
    };
    // Put the terminal back if anything panics while it is in raw mode.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        hook(info);
    }));
    enable_raw_mode()?;
    execute!(
        std::io::stderr(),
        EnterAlternateScreen,
        EnableBracketedPaste
    )?;
    if mouse {
        execute!(std::io::stderr(), EnableMouseCapture)?;
    }
    Ok(ratatui::Terminal::new(
        ratatui::backend::CrosstermBackend::new(std::io::stderr()),
    )?)
}

fn restore_terminal() {
    use crossterm::{
        cursor::Show,
        event::{DisableBracketedPaste, DisableMouseCapture},
        execute,
        terminal::{LeaveAlternateScreen, disable_raw_mode},
    };
    let _ = disable_raw_mode();
    let _ = execute!(
        std::io::stderr(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 3,
        width: w,
        height: h,
    }
}

/// The transcript as styled, pre-wrapped lines that remember their plain text for copying.
fn render_turns(turns: &[Turn], s: &Session, width: usize) -> Vec<PLine> {
    let mut out: Vec<PLine> = Vec::new();
    let body_w = width.saturating_sub(2);
    // One screen line: `gutter` is the decoration span, `text` what it shows and copies.
    let push = |out: &mut Vec<PLine>,
                gutter: Span<'static>,
                text: String,
                copy: String,
                style: Style,
                soft: bool,
                turn: usize,
                header: bool| {
        let g = gutter.content.width() as u16;
        let w = g + text.width() as u16;
        out.push(PLine {
            line: Line::from(vec![gutter, Span::styled(text, style)]),
            text: copy,
            gutter: g,
            width: w,
            soft,
            turn,
            header,
        });
    };
    for (n, t) in turns.iter().enumerate() {
        match t.role {
            Role::User => {
                push(
                    &mut out,
                    Span::styled("› ", Style::default().fg(ACCENT).bold()),
                    "you".into(),
                    "you".into(),
                    Style::default().fg(ACCENT).bold(),
                    false,
                    n,
                    true,
                );
                for raw in t.text.lines() {
                    for (j, l) in wrap(raw, body_w).into_iter().enumerate() {
                        push(
                            &mut out,
                            Span::styled("│ ", Style::default().fg(ACCENT)),
                            l.clone(),
                            l,
                            Style::default().fg(Color::White),
                            j > 0,
                            n,
                            false,
                        );
                    }
                }
            }
            Role::Assistant => {
                let color = s.agent.color();
                push(
                    &mut out,
                    Span::styled("● ", Style::default().fg(color).bold()),
                    s.agent.key().into(),
                    s.agent.key().into(),
                    Style::default().fg(color).bold(),
                    false,
                    n,
                    true,
                );
                let mut in_code = false;
                for raw in t.text.lines() {
                    if raw.trim_start().starts_with("```") {
                        in_code = !in_code;
                        push(
                            &mut out,
                            Span::raw("  "),
                            raw.to_string(),
                            raw.to_string(),
                            Style::default().fg(FAINT),
                            false,
                            n,
                            false,
                        );
                        continue;
                    }
                    let style = if in_code {
                        Style::default().fg(Color::Rgb(180, 200, 220))
                    } else {
                        Style::default().fg(TEXT)
                    };
                    let shown = if in_code {
                        raw.to_string()
                    } else {
                        markdownish(raw)
                    };
                    for (j, l) in wrap(&shown, body_w).into_iter().enumerate() {
                        push(
                            &mut out,
                            Span::raw("  "),
                            l.clone(),
                            l,
                            style,
                            j > 0,
                            n,
                            false,
                        );
                    }
                }
            }
            Role::Tool => {
                let lines: Vec<&str> = t.text.lines().collect();
                let shown = lines.len().min(6);
                for l in &lines[..shown] {
                    push(
                        &mut out,
                        Span::styled("  ⚙ ", Style::default().fg(FAINT)),
                        fit(l, body_w.saturating_sub(2)),
                        l.to_string(),
                        Style::default().fg(DIM),
                        false,
                        n,
                        false,
                    );
                }
                if lines.len() > shown {
                    let more = format!("… {} more tool calls", lines.len() - shown);
                    push(
                        &mut out,
                        Span::raw("    "),
                        more.clone(),
                        more,
                        Style::default().fg(FAINT),
                        false,
                        n,
                        false,
                    );
                }
            }
            Role::Info => {
                let text = one_line(&t.text, body_w);
                push(
                    &mut out,
                    Span::styled("  · ", Style::default().fg(FAINT)),
                    text.clone(),
                    t.text.clone(),
                    Style::default().fg(FAINT).add_modifier(Modifier::ITALIC),
                    false,
                    n,
                    false,
                );
            }
        }
        push(
            &mut out,
            Span::raw(""),
            String::new(),
            String::new(),
            Style::default(),
            false,
            n,
            false,
        );
    }
    if out.is_empty() {
        push(
            &mut out,
            Span::raw(""),
            "(no messages)".into(),
            String::new(),
            Style::default().fg(DIM),
            false,
            0,
            false,
        );
    }
    out
}

/// Strips the noisiest markdown marks for a cleaner plain-text read.
fn markdownish(l: &str) -> String {
    let t = l.replace("**", "");
    if let Some(rest) = t.trim_start().strip_prefix("#") {
        return rest.trim_start_matches('#').trim().to_string();
    }
    t
}

fn draw_help(f: &mut Frame) {
    let rows: &[(&str, &str)] = &[
        ("type", "filter by title, prompt, folder, model or id"),
        ("↑ ↓  ^K ^J", "move"),
        ("Enter", "resume the session (or start a new one on ＋)"),
        ("Tab ← →", "switch agent tab"),
        ("^N", "new session: pick an agent, Enter starts it here"),
        ("", "  Tab in that menu opens the folder browser"),
        (
            "^F",
            "new session with this agent in a folder you browse to",
        ),
        ("^O", "only sessions under the current folder"),
        ("PgUp PgDn", "scroll the preview (Shift+↑↓ by line)"),
        ("^V", "show or hide the preview"),
        (
            "mouse",
            "click selects, double-click resumes, wheel scrolls",
        ),
        (
            "drag",
            "select preview text and copy it (double-click: a message)",
        ),
        ("^Y", "copy the conversation (wl-copy, xclip/xsel, OSC 52)"),
        ("^E", "copy the resume command"),
        ("^R", "rescan"),
        ("^U", "clear the filter"),
        ("Esc", "clear the filter, then quit"),
        (
            "^C ^C",
            "quit (twice quickly; the first ^C clears the filter; ^D ^D too)",
        ),
    ];
    let area = centered(f.area(), 74, rows.len() as u16 + 4);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(" keys ", Style::default().fg(ACCENT).bold()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines = vec![Line::raw("")];
    for (k, d) in rows {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<12}"), Style::default().fg(ACCENT)),
            Span::styled(*d, Style::default().fg(TEXT)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}
