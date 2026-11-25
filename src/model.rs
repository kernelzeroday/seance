//! The agents seance knows, the sessions they leave behind, and the turns of a transcript.

use std::path::PathBuf;

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Agy,
    Omp,
    Pi,
    Hermes,
    Fx,
    Goose,
    Opencode,
    Sparkle,
    Sd,
}

impl Agent {
    pub const ALL: [Agent; 11] = [
        Agent::Claude,
        Agent::Codex,
        Agent::Agy,
        Agent::Omp,
        Agent::Pi,
        Agent::Hermes,
        Agent::Fx,
        Agent::Goose,
        Agent::Opencode,
        Agent::Sparkle,
        Agent::Sd,
    ];

    /// The short name used on screen, on the command line and in the config.
    pub fn key(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Agy => "agy",
            Agent::Omp => "omp",
            Agent::Pi => "pi",
            Agent::Hermes => "hermes",
            Agent::Fx => "fx",
            Agent::Goose => "goose",
            Agent::Opencode => "opencode",
            Agent::Sparkle => "sparkle",
            Agent::Sd => "sd",
        }
    }

    pub fn from_key(s: &str) -> Option<Agent> {
        let s = s.to_ascii_lowercase();
        let alias = match s.as_str() {
            "claude-code" | "cc" => "claude",
            "antigravity" | "gemini" => "agy",
            "oh-my-pi" => "omp",
            "goose-agent" => "goose",
            "sd-tui" => "sd",
            other => other,
        };
        Agent::ALL.into_iter().find(|a| a.key() == alias)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "OpenAI Codex CLI",
            Agent::Agy => "Antigravity CLI",
            Agent::Omp => "oh-my-pi",
            Agent::Pi => "pi coding agent",
            Agent::Hermes => "Hermes agent",
            Agent::Fx => "fx coding agent",
            Agent::Goose => "goose",
            Agent::Opencode => "OpenCode",
            Agent::Sparkle => "sparkle chat",
            Agent::Sd => "sd (stable-diffusion.cpp)",
        }
    }

    pub fn color(self) -> Color {
        let (r, g, b) = match self {
            Agent::Claude => (217, 119, 87),
            Agent::Codex => (127, 209, 174),
            Agent::Agy => (110, 168, 254),
            Agent::Omp => (247, 198, 107),
            Agent::Pi => (179, 146, 240),
            Agent::Hermes => (255, 123, 114),
            Agent::Fx => (102, 217, 239),
            Agent::Goose => (201, 168, 124),
            Agent::Opencode => (200, 205, 215),
            Agent::Sparkle => (255, 156, 203),
            Agent::Sd => (86, 212, 160),
        };
        Color::Rgb(r, g, b)
    }

    /// The program that runs the agent, before any config override.
    pub fn program(self) -> &'static str {
        self.key()
    }

    /// Arguments that start a fresh session.
    pub fn new_args(self) -> Vec<String> {
        let v: &[&str] = match self {
            Agent::Sparkle => &["chat"],
            Agent::Goose => &["session"],
            _ => &[],
        };
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Arguments that reopen `s`.
    pub fn resume_args(self, s: &Session) -> Vec<String> {
        let id = s.id.clone();
        let path = s
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| id.clone());
        let v: Vec<String> = match self {
            Agent::Claude => vec!["--resume".into(), id],
            Agent::Codex => vec!["resume".into(), id],
            Agent::Agy => vec!["--conversation".into(), id],
            Agent::Omp => vec!["--resume".into(), path],
            Agent::Pi => vec!["--session".into(), path],
            Agent::Hermes => vec!["--resume".into(), id],
            Agent::Fx => vec!["session".into(), "resume".into(), id],
            Agent::Goose => vec![
                "session".into(),
                "--resume".into(),
                "--session-id".into(),
                id,
            ],
            Agent::Opencode => vec!["--session".into(), id],
            Agent::Sparkle => vec!["chat".into(), "--cid".into(), id],
            Agent::Sd => vec!["--resume".into(), id],
        };
        v
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub agent: Agent,
    pub id: String,
    /// The agent's own title, or the first prompt when it has none.
    pub title: String,
    /// The first thing the user typed, when known and different from the title.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Unix seconds of the last activity.
    pub updated: i64,
    #[serde(default)]
    pub messages: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The file or database the session lives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

impl Session {
    /// Everything the filter matches against, lowercased.
    pub fn haystack(&self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.agent.key(),
            self.title,
            self.prompt,
            self.cwd
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            self.model.as_deref().unwrap_or(""),
            self.id
        )
        .to_lowercase()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
    /// Notes such as model switches or errors.
    Info,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub role: Role,
    pub text: String,
}

impl Turn {
    pub fn new(role: Role, text: impl Into<String>) -> Turn {
        Turn {
            role,
            text: text.into(),
        }
    }
}

/// A session's conversation as plain text: a header line, then `[who]` blocks.
pub fn plain(s: &Session, turns: &[Turn]) -> String {
    let mut out = format!(
        "{} · {} · {}\n",
        s.agent.key(),
        s.title,
        s.cwd
            .as_ref()
            .map(|c| crate::text::tilde(c))
            .unwrap_or_default()
    );
    for t in turns {
        let who = match t.role {
            Role::User => "you",
            Role::Assistant => s.agent.key(),
            Role::Tool => "tool",
            Role::Info => "·",
        };
        out.push_str(&format!("\n[{who}]\n{}\n", t.text));
    }
    out
}
