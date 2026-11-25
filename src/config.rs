//! `~/.config/seance/config.toml`. Everything is optional; a missing file means defaults.
//!
//! ```toml
//! hide = ["sd"]                 # agents left out of the list and the new-session menu
//! preview = true                # show the preview pane on start
//! mouse = true                  # clicks, wheel and drag-to-copy (false: terminal selection)
//! return = true                 # back to seance when the agent exits (false: like --once)
//!
//! [agents.claude]
//! command = "claude"            # program to run (a name on PATH or a full path)
//! args = ["--model", "opus"]    # added to every launch, new or resumed
//! ```

use std::{collections::HashMap, path::PathBuf};

use serde::Deserialize;

use crate::model::Agent;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub hide: Vec<String>,
    pub preview: Option<bool>,
    /// Capture the mouse (default true); false leaves selection to the terminal.
    pub mouse: Option<bool>,
    /// Come back to seance when the agent exits (default true); false hands the terminal
    /// over for good, as `--once` does.
    #[serde(rename = "return")]
    pub return_to_seance: Option<bool>,
    pub agents: HashMap<String, AgentConfig>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|d| d.config_dir().join("seance").join("config.toml"))
    }

    pub fn load(path: Option<PathBuf>) -> anyhow::Result<Config> {
        let Some(path) = path.or_else(Self::path) else {
            return Ok(Config::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }

    pub fn hidden(&self, a: Agent) -> bool {
        self.hide.iter().any(|h| Agent::from_key(h) == Some(a))
    }

    pub fn agent(&self, a: Agent) -> AgentConfig {
        self.agents
            .iter()
            .find(|(k, _)| Agent::from_key(k) == Some(a))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    pub fn program(&self, a: Agent) -> String {
        self.agent(a)
            .command
            .unwrap_or_else(|| a.program().to_string())
    }
}
