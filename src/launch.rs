//! Turning a choice into a command, and handing the terminal over to it.

use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
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
        let status = cmd
            .spawn()
            .and_then(|child| wait_child(child.id() as libc::pid_t, follow_stop));
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

/// Waits for `pid` to exit, calling `on_stop(pid)` each time it is stopped instead.
///
/// A plain `wait()` only returns when the agent exits. Many TUIs implement Ctrl+Z by
/// sending SIGTSTP to themselves alone (the tty can't: they run with ISIG off), and
/// anything touching the terminal from the background gets SIGTTOU/SIGTTIN. Either
/// way only the agent stops, seance sits in `wait()`, and the shell never gets the
/// terminal back: the tab looks dead and Ctrl+C is ignored. Watching for stops
/// (WUNTRACED) lets seance react.
pub(crate) fn wait_child(
    pid: libc::pid_t,
    mut on_stop: impl FnMut(libc::pid_t),
) -> std::io::Result<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    loop {
        let mut st: libc::c_int = 0;
        // SAFETY: plain syscall on a pid we spawned; st is a valid out pointer.
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WUNTRACED) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if libc::WIFSTOPPED(st) {
            crate::syslog::info(&format!(
                "agent {pid} stopped by signal {}",
                libc::WSTOPSIG(st)
            ));
            on_stop(pid);
            continue;
        }
        return Ok(std::process::ExitStatus::from_raw(st));
    }
}

/// What seance does when the agent stops itself: stop too, the way the whole job would
/// on a terminal Ctrl+Z, so the shell takes the terminal back and lists seance as a
/// stopped job. `fg` continues seance's process group, the agent included.
///
/// The agent is continued here too, but only if it stays in the stop we were told about:
/// when only seance was continued (`kill -CONT <seance>`), or in an orphaned process
/// group, where the kernel drops SIGTSTP and `raise` returns at once. Sending SIGCONT
/// when `fg` already did would be a bug: the agent may suspend again right away, and a
/// late SIGCONT cancels that new stop (the kernel discards its report, so seance never
/// hears of it and the tab hangs). `fg`'s killpg wakes the group one process at a time,
/// so seance can run before the agent gets its SIGCONT; it gives that a moment first.
///
/// The same race runs the other way when the agent suspends the whole process group
/// (Claude Code does): seance is stopped by that signal itself, and after `fg` it may
/// see the agent's stop before the agent is continued. Waiting briefly before stopping
/// keeps seance from stopping a second time, which would make `fg` look like a no-op.
fn follow_stop(pid: libc::pid_t) {
    const GRACE: Duration = Duration::from_millis(100);
    if changes_within(pid, GRACE) {
        return;
    }
    // SAFETY: plain signal syscall.
    unsafe { libc::raise(libc::SIGTSTP) };
    if !changes_within(pid, GRACE) {
        // SAFETY: plain signal syscall.
        unsafe { libc::kill(pid, libc::SIGCONT) };
    }
}

/// Whether `pid` leaves its reaped stop (continued, stopped again, exited) within `grace`.
fn changes_within(pid: libc::pid_t, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if state_changed_since_stop(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Whether `pid` has an unreported state change (continued, stopped again, or exited)
/// since its last stop was reaped. Peeks with WNOWAIT, so wait_child still sees it.
fn state_changed_since_stop(pid: libc::pid_t) -> bool {
    // SAFETY: waitid fills `info`; zeroed is a valid siginfo_t.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let r = libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WCONTINUED | libc::WSTOPPED | libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        // On error (it can't be waited for) don't poke it: report "changed".
        r != 0 || info.si_pid() != 0
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

    // --- wait_child / follow_stop -------------------------------------------------
    //
    // These run real processes. Children go in their own process group: a group whose
    // leader's parent (this test) is in another group of the same session is never
    // orphaned, so the kernel delivers SIGTSTP to it however the tests are run.

    use std::os::unix::process::ExitStatusExt as _;

    fn sh(script: &str) -> libc::pid_t {
        Command::new("sh")
            .args(["-c", script])
            .process_group(0)
            .spawn()
            .expect("spawn sh")
            .id() as libc::pid_t
    }

    /// An `on_stop` that plays the shell's `fg`: it continues the agent and counts.
    fn resume_counting(stops: &mut Vec<libc::pid_t>) -> impl FnMut(libc::pid_t) + '_ {
        |pid| {
            stops.push(pid);
            unsafe { libc::kill(pid, libc::SIGCONT) };
        }
    }

    fn never_stops(_: libc::pid_t) {
        panic!("child was reported stopped but never stops");
    }

    /// Polls `/proc/<pid>/stat` until the process is in state `want` (e.g. 'T').
    fn wait_state(pid: libc::pid_t, want: char) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            // The state follows the ")" closing the command name.
            if stat.rsplit_once(") ").and_then(|(_, r)| r.chars().next()) == Some(want) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "pid {pid} never reached state {want}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn wait_child_returns_exit_code() {
        let st = wait_child(sh("exit 3"), never_stops).unwrap();
        assert_eq!(st.code(), Some(3));
    }

    #[test]
    fn wait_child_returns_success() {
        let st = wait_child(sh("true"), never_stops).unwrap();
        assert!(st.success());
    }

    #[test]
    fn wait_child_reports_death_by_signal() {
        let st = wait_child(sh("kill -TERM $$"), never_stops).unwrap();
        assert_eq!(st.code(), None);
        assert_eq!(st.signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn wait_child_reports_sigint_like_ctrl_c() {
        // How most agents are closed; main::exit_notice treats it as a normal return.
        let st = wait_child(sh("kill -INT $$"), never_stops).unwrap();
        assert_eq!(st.signal(), Some(libc::SIGINT));
    }

    /// The bug: an agent that suspends itself (Ctrl+Z in a raw-mode TUI) used to leave
    /// seance blocked in wait() forever.
    #[test]
    fn self_suspend_with_sigtstp_is_reported() {
        let pid = sh("kill -TSTP $$; exit 7");
        let mut stops = vec![];
        let st = wait_child(pid, resume_counting(&mut stops)).unwrap();
        assert_eq!(stops, vec![pid]);
        assert_eq!(
            st.code(),
            Some(7),
            "agent keeps running after it is continued"
        );
    }

    #[test]
    fn sigstop_is_reported() {
        let pid = sh("kill -STOP $$; exit 4");
        let mut stops = vec![];
        let st = wait_child(pid, resume_counting(&mut stops)).unwrap();
        assert_eq!(stops.len(), 1);
        assert_eq!(st.code(), Some(4));
    }

    /// What an agent that writes to the terminal from the background gets.
    #[test]
    fn sigttou_and_sigttin_stops_are_reported() {
        let pid = sh("kill -TTOU $$; kill -TTIN $$; exit 0");
        let mut stops = vec![];
        let st = wait_child(pid, resume_counting(&mut stops)).unwrap();
        assert_eq!(stops.len(), 2);
        assert!(st.success());
    }

    #[test]
    fn repeated_suspends_are_each_reported() {
        let pid = sh("for i in 1 2 3 4 5; do kill -TSTP $$; done; exit 9");
        let mut stops = vec![];
        let st = wait_child(pid, resume_counting(&mut stops)).unwrap();
        assert_eq!(stops, vec![pid; 5]);
        assert_eq!(st.code(), Some(9));
    }

    #[test]
    fn stop_from_outside_is_reported() {
        let pid = sh("sleep 0.3; exit 2");
        // Stop it from another process, as `kill -STOP` or a debugger would.
        let stopper = std::thread::spawn(move || {
            wait_state(pid, 'S');
            unsafe { libc::kill(pid, libc::SIGSTOP) };
        });
        let mut stops = vec![];
        let st = wait_child(pid, resume_counting(&mut stops)).unwrap();
        stopper.join().unwrap();
        assert_eq!(stops.len(), 1);
        assert_eq!(st.code(), Some(2));
    }

    #[test]
    fn killed_while_stopped_is_reported_as_killed() {
        let pid = sh("kill -TSTP $$; exit 0");
        let mut stops = 0;
        let st = wait_child(pid, |p| {
            stops += 1;
            // Give up on it instead of resuming, like `kill %1` on a stopped job.
            unsafe { libc::kill(p, libc::SIGKILL) };
        })
        .unwrap();
        assert_eq!(stops, 1);
        assert_eq!(st.signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn continued_without_on_stop_resuming_still_finishes() {
        // Someone else (the shell's `fg`) continues the agent; on_stop only records.
        let pid = sh("kill -TSTP $$; exit 6");
        let mut stops = 0;
        let st = wait_child(pid, |p| {
            stops += 1;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                unsafe { libc::kill(p, libc::SIGCONT) };
            });
        })
        .unwrap();
        assert_eq!(stops, 1);
        assert_eq!(st.code(), Some(6));
    }

    #[test]
    fn wait_child_reaps_the_process() {
        let pid = sh("exit 0");
        wait_child(pid, never_stops).unwrap();
        // Reaped: no zombie left, and waiting again finds no such child.
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        let err = wait_child(pid, never_stops).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ECHILD));
    }

    #[test]
    fn wait_child_rejects_a_process_that_is_not_ours() {
        let err = wait_child(1, never_stops).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ECHILD));
    }

    #[test]
    fn wait_child_retries_after_a_signal_interrupts_it() {
        // A handler installed without SA_RESTART makes waitpid fail with EINTR.
        extern "C" fn noop(_: libc::c_int) {}
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = noop as extern "C" fn(libc::c_int) as usize;
            sa.sa_flags = 0;
            libc::sigaction(libc::SIGUSR2, &sa, std::ptr::null_mut());
        }
        let pid = sh("sleep 0.3; exit 5");
        // pthread_kill so the signal lands on the waiting thread, not another test's.
        let waiter = unsafe { libc::pthread_self() };
        let poker = std::thread::spawn(move || {
            for _ in 0..5 {
                std::thread::sleep(Duration::from_millis(30));
                unsafe { libc::pthread_kill(waiter, libc::SIGUSR2) };
            }
        });
        let st = wait_child(pid, never_stops).unwrap();
        poker.join().unwrap();
        assert_eq!(st.code(), Some(5));
    }

    // --- state_changed_since_stop ----------------------------------------------------

    /// Spawns a child that stops itself and reaps that stop, as wait_child does.
    fn stopped_child() -> libc::pid_t {
        let pid = sh("kill -STOP $$; exit 0");
        let mut st = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut st, libc::WUNTRACED) }, pid);
        assert!(libc::WIFSTOPPED(st));
        pid
    }

    fn finish(pid: libc::pid_t) {
        unsafe { libc::kill(pid, libc::SIGCONT) };
        wait_child(pid, resume_counting(&mut vec![])).unwrap();
    }

    #[test]
    fn no_change_while_still_in_the_reaped_stop() {
        let pid = stopped_child();
        assert!(!state_changed_since_stop(pid));
        // Peeking twice changes nothing.
        assert!(!state_changed_since_stop(pid));
        finish(pid);
    }

    #[test]
    fn change_seen_once_someone_else_continued_it() {
        let pid = stopped_child();
        unsafe { libc::kill(pid, libc::SIGCONT) };
        wait_state(pid, 'Z');
        assert!(state_changed_since_stop(pid));
        assert!(
            wait_child(pid, never_stops).unwrap().success(),
            "peek must not reap"
        );
    }

    #[test]
    fn change_seen_when_it_stopped_again() {
        let pid = sh("kill -STOP $$; kill -STOP $$; exit 0");
        let mut st = 0;
        unsafe { libc::waitpid(pid, &mut st, libc::WUNTRACED) };
        unsafe { libc::kill(pid, libc::SIGCONT) };
        // Wait for the second stop without reaping it.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            if stat
                .rsplit_once(") ")
                .is_some_and(|(_, r)| r.starts_with('T'))
                && state_changed_since_stop(pid)
            {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        // The unreaped second stop is still there for wait_child.
        let mut stops = vec![];
        wait_child(pid, resume_counting(&mut stops)).unwrap();
        assert_eq!(stops.len(), 1);
    }

    #[test]
    fn change_seen_when_it_exited() {
        let pid = sh("exit 0");
        wait_state(pid, 'Z');
        assert!(state_changed_since_stop(pid));
        assert!(wait_child(pid, never_stops).unwrap().success());
    }

    #[test]
    fn not_our_child_counts_as_changed_so_it_is_left_alone() {
        assert!(state_changed_since_stop(1));
    }

    // --- follow_stop end to end -----------------------------------------------------
    //
    // follow_stop stops the process that calls it, so it runs in a copy of this test
    // binary standing in for seance: the outer test plays the shell, sees "seance" stop
    // as a job, and continues it with `fg`'s killpg(SIGCONT).

    const HELPER_ENV: &str = "SEANCE_TEST_FOLLOW_STOP";

    /// Not a real test: the stand-in for seance that the tests below run. It runs the
    /// agent script in its own process group (the "job"), waits with follow_stop and
    /// exits with the agent's code.
    #[test]
    #[ignore = "helper process for follow_stop tests"]
    fn follow_stop_helper() {
        let Ok(script) = std::env::var(HELPER_ENV) else {
            return;
        };
        let child = Command::new("sh").args(["-c", &script]).spawn().unwrap();
        let st = wait_child(child.id() as libc::pid_t, follow_stop).unwrap();
        std::process::exit(st.code().unwrap_or(100 + st.signal().unwrap_or(0)));
    }

    /// Starts the helper as a job of its own and returns (pid, pgid).
    fn spawn_helper(script: &str) -> libc::pid_t {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "launch::tests::follow_stop_helper",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_ENV, script)
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
            .id() as libc::pid_t
    }

    /// waitpid(WUNTRACED) on the helper, as the shell does for a foreground job.
    fn shell_wait(pid: libc::pid_t) -> libc::c_int {
        let mut st = 0;
        loop {
            let r = unsafe { libc::waitpid(pid, &mut st, libc::WUNTRACED) };
            if r == pid {
                return st;
            }
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::Interrupted
            );
        }
    }

    #[test]
    fn follow_stop_hands_the_terminal_back_and_fg_resumes_both() {
        let helper = spawn_helper("kill -TSTP $$; exit 7");

        // The shell sees its job stop (it would print "Stopped seance" and prompt).
        let st = shell_wait(helper);
        assert!(libc::WIFSTOPPED(st), "seance must stop when the agent does");
        assert_eq!(libc::WSTOPSIG(st), libc::SIGTSTP);

        // `fg`: continue the whole job.
        unsafe { libc::kill(-helper, libc::SIGCONT) };
        let st = shell_wait(helper);
        assert!(libc::WIFEXITED(st));
        assert_eq!(
            libc::WEXITSTATUS(st),
            7,
            "agent resumed and its exit code came back"
        );
    }

    #[test]
    fn follow_stop_resumes_the_agent_when_only_seance_is_continued() {
        // A plain `kill -CONT <seance pid>` instead of the shell's killpg.
        let helper = spawn_helper("kill -TSTP $$; exit 3");
        let st = shell_wait(helper);
        assert!(libc::WIFSTOPPED(st));
        unsafe { libc::kill(helper, libc::SIGCONT) };
        let st = shell_wait(helper);
        assert!(libc::WIFEXITED(st));
        assert_eq!(libc::WEXITSTATUS(st), 3);
    }

    #[test]
    fn follow_stop_survives_several_suspend_and_fg_rounds() {
        let helper = spawn_helper("for i in 1 2 3; do kill -TSTP $$; done; exit 11");
        for round in 1..=3 {
            let st = shell_wait(helper);
            assert!(
                libc::WIFSTOPPED(st),
                "round {round}: seance should be stopped"
            );
            unsafe { libc::kill(-helper, libc::SIGCONT) };
        }
        let st = shell_wait(helper);
        assert!(libc::WIFEXITED(st));
        assert_eq!(libc::WEXITSTATUS(st), 11);
    }

    /// The race: right after `fg` the agent suspends again before seance runs. Many
    /// fast rounds, each of which must still stop seance.
    #[test]
    fn follow_stop_never_loses_a_suspend_that_races_with_fg() {
        let rounds = 40;
        let helper = spawn_helper(&format!(
            "i=0; while [ $i -lt {rounds} ]; do kill -TSTP $$; i=$((i+1)); done; exit 12"
        ));
        for round in 1..=rounds {
            let st = shell_wait(helper);
            assert!(libc::WIFSTOPPED(st), "round {round}: suspend was lost");
            unsafe { libc::kill(-helper, libc::SIGCONT) };
        }
        let st = shell_wait(helper);
        assert!(libc::WIFEXITED(st));
        assert_eq!(libc::WEXITSTATUS(st), 12);
    }

    /// Without job control (an orphaned process group, e.g. seance started by a script
    /// in its own session) the kernel drops seance's SIGTSTP. seance must then continue
    /// the agent itself instead of leaving it stopped forever.
    #[test]
    fn follow_stop_continues_the_agent_when_seance_cannot_stop() {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "launch::tests::follow_stop_helper",
            "--ignored",
            "--test-threads=1",
        ])
        // SIGSTOP: the agent stops for sure; only seance's SIGTSTP is dropped.
        .env(HELPER_ENV, "kill -STOP $$; exit 8")
        .stdout(std::process::Stdio::null());
        // A new session: the helper's group has no parent in this session, so orphaned.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let helper = cmd.spawn().unwrap().id() as libc::pid_t;
        let started = Instant::now();
        let st = shell_wait(helper);
        assert!(libc::WIFEXITED(st), "seance must not hang or stop here");
        assert_eq!(libc::WEXITSTATUS(st), 8);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// How Claude Code suspends: SIGTSTP to the whole process group, so the tty-style
    /// stop hits seance directly too. Every `fg` must resume both: seance can wake
    /// before the agent gets its SIGCONT and must not take the agent's stop as a new
    /// suspend and stop again (`fg` would then look like it did nothing).
    #[test]
    fn group_suspend_needs_exactly_one_fg_per_suspend() {
        let rounds = 40;
        let helper = spawn_helper(&format!(
            "i=0; while [ $i -lt {rounds} ]; do kill -TSTP 0; i=$((i+1)); done; exit 13"
        ));
        let mut stops = 0;
        loop {
            let st = shell_wait(helper);
            if libc::WIFEXITED(st) {
                assert_eq!(libc::WEXITSTATUS(st), 13);
                break;
            }
            assert!(libc::WIFSTOPPED(st));
            stops += 1;
            assert!(
                stops <= rounds,
                "seance stopped again after fg: {stops} stops for {rounds} suspends"
            );
            unsafe { libc::kill(-helper, libc::SIGCONT) };
        }
        assert_eq!(stops, rounds);
    }

    #[test]
    fn follow_stop_is_not_involved_when_the_agent_just_exits() {
        let helper = spawn_helper("exit 0");
        let st = shell_wait(helper);
        assert!(
            libc::WIFEXITED(st),
            "no stop for an agent that never suspends"
        );
        assert_eq!(libc::WEXITSTATUS(st), 0);
    }

    #[test]
    fn follow_stop_passes_on_an_agent_killed_after_resuming() {
        let helper = spawn_helper("kill -TSTP $$; kill -TERM $$");
        let st = shell_wait(helper);
        assert!(libc::WIFSTOPPED(st));
        unsafe { libc::kill(-helper, libc::SIGCONT) };
        let st = shell_wait(helper);
        assert!(libc::WIFEXITED(st));
        assert_eq!(libc::WEXITSTATUS(st), 100 + libc::SIGTERM);
    }
}
