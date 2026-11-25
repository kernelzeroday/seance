//! seance: one picker for the sessions every coding agent leaves behind. Browse them, preview
//! them, resume one, or start a new session with any agent in any folder.

mod clipboard;
mod config;
mod folders;
mod launch;
mod model;
mod search;
mod select;
mod sources;
mod syslog;
mod text;
mod tui;
mod watch;

use std::{io::Write, path::PathBuf};

/// `println!` that ends the command quietly when stdout is closed (`seance list | head`).
macro_rules! out {
    ($($arg:tt)*) => {
        if writeln!(std::io::stdout().lock(), $($arg)*).is_err() {
            return Ok(());
        }
    };
}

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::{
    config::Config,
    launch::Plan,
    model::{Agent, Session},
    sources::FileCache,
};

#[derive(Parser)]
#[command(
    name = "seance",
    version,
    about = "Pick up any coding agent's session where you left it, or start a new one",
    after_help = "Agents: claude claurst codex agy omp pi hermes fx goose opencode sparkle sd\n\
                  Config: ~/.config/seance/config.toml    Logs: journalctl -t seance\n\n\
                  Shell integration (stay in the session's folder after the agent exits):\n  \
                  sea() { local c; c=$(seance --print \"$@\") && eval \"$c\"; }\n\n\
                  For scripts and agents (no terminal needed, safe while the TUI runs):\n  \
                  seance search QUERY --json    sessions whose title or conversation match\n  \
                  seance show ID --json --last 20\n  \
                  seance list --json -a claude -H\n\
                  JSON results carry `resume`: the shell command that reopens the session."
)]
struct Cli {
    /// Start with this filter typed in
    filter: Vec<String>,
    /// Open on one agent's tab
    #[arg(short, long, value_name = "AGENT")]
    agent: Option<String>,
    /// Only sessions under the current folder
    #[arg(short = 'H', long)]
    here: bool,
    /// Open straight into the new-session menu
    #[arg(short, long)]
    new: bool,
    /// Leave the mouse to the terminal (no clicks, wheel or drag-to-copy)
    #[arg(long)]
    no_mouse: bool,
    /// Hand the terminal to the agent for good instead of coming back when it exits
    #[arg(long)]
    once: bool,
    /// Print the chosen command as `cd DIR && CMD` instead of running it
    #[arg(short, long, global = true)]
    print: bool,
    /// Config file (default ~/.config/seance/config.toml)
    #[arg(short, long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List sessions, newest first
    #[command(alias = "ls")]
    List {
        /// Only this agent
        #[arg(short, long, value_name = "AGENT")]
        agent: Option<String>,
        /// Only sessions under the current folder
        #[arg(short = 'H', long)]
        here: bool,
        /// At most this many
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// One JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Search session titles and what was said in them (all words must match)
    #[command(alias = "s")]
    Search {
        #[arg(required = true)]
        query: Vec<String>,
        /// Only this agent
        #[arg(short, long, value_name = "AGENT")]
        agent: Option<String>,
        /// Only sessions under the current folder
        #[arg(short = 'H', long)]
        here: bool,
        /// At most this many
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// One JSON object per line, with a snippet and the resume command
        #[arg(long)]
        json: bool,
    },
    /// Resume a session by id (a unique prefix is enough)
    Resume { id: String },
    /// Print a session's whole conversation
    Show {
        id: String,
        /// Copy it to the clipboard (wl-copy, xclip/xsel, or OSC 52) instead of printing
        #[arg(short = 'y', long)]
        copy: bool,
        /// One JSON object: the session, its resume command and its turns
        #[arg(long)]
        json: bool,
        /// Only the last N turns
        #[arg(short = 'l', long, value_name = "N")]
        last: Option<usize>,
    },
    /// Start a new session with AGENT in DIR (default: here)
    New { agent: String, dir: Option<PathBuf> },
    /// Show which agents are installed and how many sessions each has
    Agents,
}

fn parse_agent(s: &str) -> Result<Agent> {
    Agent::from_key(s).with_context(|| {
        let all: Vec<&str> = Agent::ALL.iter().map(|a| a.key()).collect();
        format!("unknown agent {s:?} (one of: {})", all.join(", "))
    })
}

fn scan_all(cfg: &Config) -> Vec<Session> {
    let cache = FileCache::load();
    let mut all: Vec<Session> = std::thread::scope(|scope| {
        let handles: Vec<_> = Agent::ALL
            .into_iter()
            .filter(|a| !cfg.hidden(*a))
            .map(|a| {
                let cache = &cache;
                scope.spawn(move || (a, sources::scan(a, cache)))
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .flat_map(|(a, r)| match r {
                Ok(v) => v,
                Err(e) => {
                    syslog::warning(&format!("{}: {e:#}", a.key()));
                    vec![]
                }
            })
            .collect()
    });
    cache.save();
    all.sort_by_key(|s| std::cmp::Reverse(s.updated));
    all
}

/// A session as JSON for scripts and agents, with the command that reopens it.
fn session_json(cfg: &Config, s: &Session, here: &std::path::Path) -> serde_json::Value {
    let mut v = serde_json::to_value(s).unwrap_or_default();
    let plan = launch::resume(cfg, s, here);
    v["resume"] = serde_json::json!(plan.shell());
    v
}

/// The one session whose id starts with `id`.
fn find<'a>(all: &'a [Session], id: &str) -> Result<&'a Session> {
    let hits: Vec<&Session> = all.iter().filter(|s| s.id.starts_with(id)).collect();
    match hits.as_slice() {
        [s] => Ok(s),
        [] => bail!("no session id starts with {id:?}"),
        many => bail!(
            "{id:?} matches {} sessions ({}); give more of the id",
            many.len(),
            many.iter()
                .take(4)
                .map(|s| format!("{} {}", s.agent.key(), s.id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The footer message for an agent that exited: (text, is it a failure).
fn exit_notice(agent: &str, st: std::process::ExitStatus) -> (String, bool) {
    use std::os::unix::process::ExitStatusExt;
    match (st.code(), st.signal()) {
        // Ctrl+C is how many agents are closed; that is not a failure.
        (Some(0 | 130), _) | (None, Some(libc::SIGINT)) => (format!("back from {agent}"), false),
        (Some(code), _) => (format!("{agent} exited with code {code}"), true),
        (None, Some(sig)) => (format!("{agent} was killed by signal {sig}"), true),
        (None, None) => (format!("{agent} exited"), true),
    }
}

/// Whether the picker can run: it draws on stderr and reads keys from the terminal.
fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal() && std::fs::File::open("/dev/tty").is_ok()
}

fn run_plan(plan: Plan, print: bool) -> Result<()> {
    if print {
        println!("{}", plan.shell());
        return Ok(());
    }
    eprintln!("\x1b[38;2;255;160;200m↪\x1b[0m {}", plan.display());
    plan.exec()
}

fn main() {
    if let Err(e) = real_main() {
        syslog::err(&format!("{e:#}"));
        eprintln!("seance: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.clone())?;
    let here =
        std::env::current_dir().unwrap_or_else(|_| text::home().unwrap_or_else(|| "/".into()));
    match cli.cmd {
        None if !interactive() => {
            bail!(
                "no terminal to draw the picker on. From scripts and agents use \
                 `seance search QUERY`, `seance list` or `seance show ID` (all take --json); \
                 see `seance --help`"
            )
        }
        None => {
            let agent = cli.agent.as_deref().map(parse_agent).transpose()?;
            let mouse = !cli.no_mouse && cfg.mouse.unwrap_or(true);
            let comeback = !cli.once && !cli.print && cfg.return_to_seance.unwrap_or(true);
            let (mut filter, mut new, mut notice) = (cli.filter.join(" "), cli.new, None);
            loop {
                let plan = tui::run(tui::Start {
                    mouse,
                    cfg: cfg.clone(),
                    agent,
                    filter: std::mem::take(&mut filter),
                    here_only: cli.here,
                    new: std::mem::take(&mut new),
                    launch_dir: here.clone(),
                    notice: notice.take(),
                })?;
                let Some(p) = plan else {
                    // Nothing chosen: a failing status keeps `eval "$(seance --print)"` a no-op.
                    if cli.print {
                        std::process::exit(1);
                    }
                    return Ok(());
                };
                if !comeback {
                    return run_plan(p, cli.print);
                }
                // Run the agent, then come back to the picker with how it went.
                eprintln!("\x1b[38;2;255;160;200m↪\x1b[0m {}", p.display());
                notice = Some(match p.run() {
                    Ok(st) => exit_notice(p.agent.key(), st),
                    Err(e) => (format!("{e:#}"), true),
                });
            }
        }
        Some(Cmd::List {
            agent,
            here: here_only,
            limit,
            json,
        }) => {
            let agent = agent.as_deref().map(parse_agent).transpose()?;
            let now = text::now();
            for s in scan_all(&cfg)
                .into_iter()
                .filter(|s| agent.is_none_or(|a| a == s.agent))
                .filter(|s| !here_only || s.cwd.as_ref().is_some_and(|c| c.starts_with(&here)))
                .take(limit)
            {
                if json {
                    out!("{}", session_json(&cfg, &s, &here));
                } else {
                    out!(
                        "{:<9} {:>4}  {:<38}  {}  {}",
                        s.agent.key(),
                        text::ago(s.updated, now),
                        text::fit(&s.id, 38),
                        s.cwd
                            .as_ref()
                            .map(|c| text::tilde(c))
                            .unwrap_or_else(|| "-".into()),
                        text::fit(&s.title, 70),
                    );
                }
            }
            Ok(())
        }
        Some(Cmd::Resume { id }) => {
            let all = scan_all(&cfg);
            let s = find(&all, &id)?;
            run_plan(launch::resume(&cfg, s, &here), cli.print)
        }
        Some(Cmd::Show {
            id,
            copy,
            json,
            last,
        }) => {
            let all = scan_all(&cfg);
            let s = find(&all, &id)?;
            let mut turns = sources::raw_transcript(s)?;
            if let Some(n) = last {
                turns.drain(..turns.len().saturating_sub(n));
            }
            if json {
                let mut v = session_json(&cfg, s, &here);
                v["turns"] = turns
                    .iter()
                    .map(|t| {
                        let role = match t.role {
                            model::Role::User => "user",
                            model::Role::Assistant => "assistant",
                            model::Role::Tool => "tool",
                            model::Role::Info => "info",
                        };
                        serde_json::json!({ "role": role, "text": t.text })
                    })
                    .collect();
                out!("{v}");
                return Ok(());
            }
            let text = model::plain(s, &turns);
            if copy {
                let via = clipboard::copy(&text)?;
                eprintln!("copied {} lines via {via}", text.lines().count());
            } else {
                out!("{}", text.trim_end());
            }
            Ok(())
        }
        Some(Cmd::Search {
            query,
            agent,
            here: here_only,
            limit,
            json,
        }) => {
            let agent = agent.as_deref().map(parse_agent).transpose()?;
            let all = scan_all(&cfg);
            let mut index = search::Index::load();
            if index.refresh(&all) > 0 {
                index.save();
            }
            let words = search::words(&query.join(" "));
            let now = text::now();
            let mut shown = 0;
            for s in all
                .iter()
                .filter(|s| agent.is_none_or(|a| a == s.agent))
                .filter(|s| !here_only || s.cwd.as_ref().is_some_and(|c| c.starts_with(&here)))
            {
                let doc = index.get(s);
                let Some(hit) = search::hit(s, doc, &words) else {
                    continue;
                };
                let snippet = doc
                    .filter(|_| hit == search::Hit::Text)
                    .and_then(|d| search::snippet(d, &words, 110));
                if json {
                    let mut v = session_json(&cfg, s, &here);
                    v["matched"] = serde_json::json!(if hit == search::Hit::Meta {
                        "details"
                    } else {
                        "text"
                    });
                    if let Some(sn) = &snippet {
                        v["snippet"] = serde_json::json!(sn);
                    }
                    out!("{v}");
                } else {
                    out!(
                        "{:<9} {:>4}  {:<38}  {}  {}",
                        s.agent.key(),
                        text::ago(s.updated, now),
                        text::fit(&s.id, 38),
                        s.cwd
                            .as_ref()
                            .map(|c| text::tilde(c))
                            .unwrap_or_else(|| "-".into()),
                        text::fit(&s.title, 70),
                    );
                    if let Some(sn) = &snippet {
                        out!("          {sn}");
                    }
                }
                shown += 1;
                if shown >= limit {
                    break;
                }
            }
            if shown == 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(Cmd::New { agent, dir }) => {
            let agent = parse_agent(&agent)?;
            let dir = dir.map(|d| here.join(d)).unwrap_or(here);
            if !dir.is_dir() {
                bail!("{} is not a folder", dir.display());
            }
            run_plan(launch::new(&cfg, agent, &dir), cli.print)
        }
        Some(Cmd::Agents) => {
            let all = scan_all(&cfg);
            for a in Agent::ALL {
                let n = all.iter().filter(|s| s.agent == a).count();
                let program = cfg.program(a);
                let at = launch::which(&program)
                    .map(|p| text::tilde(&p))
                    .unwrap_or_else(|| "not installed".into());
                let hidden = if cfg.hidden(a) { "  (hidden)" } else { "" };
                out!(
                    "{:<9} {:>5} sessions  {:<28} {}{hidden}",
                    a.key(),
                    n,
                    a.describe(),
                    at
                );
            }
            Ok(())
        }
    }
}
