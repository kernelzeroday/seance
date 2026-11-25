//! Copying text to the clipboard: `wl-copy` on Wayland, `xclip` / `xsel` on X11, and the
//! terminal's OSC 52 escape when neither is available (which also works over ssh).

use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Result, bail};

use crate::launch::which;

/// Puts `text` on the clipboard; returns what carried it, for the status line.
pub fn copy(text: &str) -> Result<&'static str> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let tools: &[(&str, &[&str], bool)] = &[
        ("wl-copy", &["--type", "text/plain;charset=utf-8"], wayland),
        ("xclip", &["-selection", "clipboard", "-in"], x11),
        ("xsel", &["--clipboard", "--input"], x11),
    ];
    let mut last_err = None;
    for (tool, args, usable) in tools {
        if !usable || which(tool).is_none() {
            continue;
        }
        match pipe(tool, args, text) {
            Ok(()) => return Ok(tool),
            Err(e) => last_err = Some(e),
        }
    }
    if let Some(e) = last_err {
        crate::syslog::warning(&format!("clipboard: {e:#}; falling back to OSC 52"));
    }
    osc52(text)?;
    Ok("terminal (OSC 52)")
}

fn pipe(tool: &str, args: &[&str], text: &str) -> Result<()> {
    // Never let the tool touch the terminal: seance is drawing on it in raw mode.
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    // wl-copy and xclip fork a server to own the selection; the parent exits right away.
    let status = child.wait()?;
    if !status.success() {
        bail!("{tool} exited with {status}");
    }
    Ok(())
}

fn osc52(text: &str) -> Result<()> {
    let mut out = std::io::stderr();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()?;
    Ok(())
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
}
