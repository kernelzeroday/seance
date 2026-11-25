//! Diagnostics go to the system log (journald or syslog, through /dev/log), not the screen:
//! `journalctl -t seance` shows them. If there is no log socket, messages are dropped.

use std::{os::unix::net::UnixDatagram, sync::OnceLock};

#[derive(Clone, Copy)]
pub enum Level {
    Err = 3,
    Warning = 4,
    Info = 6,
}

fn socket() -> Option<&'static UnixDatagram> {
    static SOCKET: OnceLock<Option<UnixDatagram>> = OnceLock::new();
    SOCKET
        .get_or_init(|| {
            let s = UnixDatagram::unbound().ok()?;
            s.connect("/dev/log").ok()?;
            Some(s)
        })
        .as_ref()
}

/// One RFC 3164 record per line, facility "user", tag "seance".
fn record(level: Level, line: &str) -> String {
    format!(
        "<{}>seance[{}]: {line}",
        8 + level as u8,
        std::process::id()
    )
}

pub fn log(level: Level, msg: &str) {
    let Some(s) = socket() else { return };
    for line in msg.lines().filter(|l| !l.trim().is_empty()) {
        let _ = s.send(record(level, line).as_bytes());
    }
}

pub fn err(msg: &str) {
    log(Level::Err, msg);
}
pub fn warning(msg: &str) {
    log(Level::Warning, msg);
}
pub fn info(msg: &str) {
    log(Level::Info, msg);
}

#[cfg(test)]
mod tests {
    #[test]
    fn records_carry_priority_and_tag() {
        let r = super::record(super::Level::Warning, "hello");
        assert!(r.starts_with("<12>seance["));
        assert!(r.ends_with("]: hello"));
    }
}
