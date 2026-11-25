//! Turning a choice into a command, and handing the terminal over to it.

use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Result, bail};

use crate::{
    config::Config,
    model::{Agent, Session},
    text::tilde,
};

#[derive(Debug, Clone)]
pub struct Plan {
    pub agent: Agent,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

/// Where a session should be reopened: its own directory if it still exists, else `fallback`.
fn session_dir(s: &Session, fallback: &Path) -> PathBuf {
    s.cwd
        .as_ref()
        .filter(|d| d.is_dir())
        .cloned()
        .unwrap_or_else(|| fallback.to_path_buf())
}

pub fn resume(cfg: &Config, s: &Session, fallback: &Path) -> Plan {
    let mut args = cfg.agent(s.agent).args;
    args.extend(s.agent.resume_args(s));
    Plan {
        agent: s.agent,
        program: cfg.program(s.agent),
        args,
        cwd: session_dir(s, fallback),
    }
}

pub fn new(cfg: &Config, agent: Agent, dir: &Path) -> Plan {
    let mut args = cfg.agent(agent).args;
    args.extend(agent.new_args());
    Plan {
        agent,
        program: cfg.program(agent),
        args,
        cwd: dir.to_path_buf(),
    }
}

fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

impl Plan {
    /// `cd DIR && PROGRAM ARGS…`, for printing or for a shell to eval.
    pub fn shell(&self) -> String {
        let mut cmd = vec![quote(&self.program)];
        cmd.extend(self.args.iter().map(|a| quote(a)));
        format!(
            "cd {} && {}",
            quote(&self.cwd.display().to_string()),
            cmd.join(" ")
        )
    }

    /// The command as shown on screen, with the directory home-relative.
    pub fn display(&self) -> String {
        let mut cmd = vec![self.program.clone()];
        cmd.extend(self.args.iter().map(|a| quote(a)));
        format!("{}  in {}", cmd.join(" "), tilde(&self.cwd))
    }

    /// Runs the agent in the foreground and waits for it to exit, so seance can come back.
    ///
    /// While it runs, seance ignores SIGINT and SIGQUIT (Ctrl+C belongs to the agent, as
    /// with a shell's `system()`); the child gets them back at their defaults. Ctrl+Z is
    /// left alone: it stops seance and the agent together and the shell's `fg` resumes both.
    pub fn run(&self) -> Result<std::process::ExitStatus> {
        if which(&self.program).is_none() {
            bail!("{} is not installed (not found on PATH)", self.program);
        }
        crate::syslog::info(&format!("run {}: {}", self.agent.key(), self.shell()));
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .current_dir(&self.cwd)
            .env("PWD", &self.cwd);
        // SAFETY: only async-signal-safe calls (signal(2)) between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                Ok(())
            });
        }
        let (old_int, old_quit) = unsafe {
            (
                libc::signal(libc::SIGINT, libc::SIG_IGN),
                libc::signal(libc::SIGQUIT, libc::SIG_IGN),
            )
        };
        let status = cmd.spawn().and_then(|mut child| child.wait());
        unsafe {
            libc::signal(libc::SIGINT, old_int);
            libc::signal(libc::SIGQUIT, old_quit);
        }
        status.map_err(|e| anyhow::anyhow!("could not run {}: {e}", self.program))
    }

    /// Replaces this process with the agent. Only returns on failure.
    pub fn exec(&self) -> Result<()> {
        if which(&self.program).is_none() {
            bail!("{} is not installed (not found on PATH)", self.program);
        }
        crate::syslog::info(&format!("launch {}: {}", self.agent.key(), self.shell()));
        let err = Command::new(&self.program)
            .args(&self.args)
            .current_dir(&self.cwd)
            // Shells and prompts that track the directory see the agent's.
            .env("PWD", &self.cwd)
            .exec();
        bail!("could not run {}: {err}", self.program)
    }
}

/// The full path of `program` on PATH (or `program` itself if it is a path that exists).
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = crate::text::expand(program);
        return p.is_file().then_some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(program))
            .find(|p| {
                use std::os::unix::fs::PermissionsExt;
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting() {
        let p = Plan {
            agent: Agent::Claude,
            program: "claude".into(),
            args: vec!["--resume".into(), "a b".into(), "it's".into()],
            cwd: "/tmp/x y".into(),
        };
        assert_eq!(
            p.shell(),
            r"cd '/tmp/x y' && claude --resume 'a b' 'it'\''s'"
        );
    }
}
