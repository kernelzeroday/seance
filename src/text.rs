//! Small text helpers: one-line titles, home-relative paths, relative times, word wrapping.

use std::path::Path;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Text the user typed, minus the scaffolding agents wrap around it.
pub fn is_scaffolding(s: &str) -> bool {
    let t = s.trim_start();
    t.is_empty()
        || t.starts_with('<')
        || t.starts_with("# AGENTS.md")
        || t.starts_with("Caveat:")
        || t.starts_with("[Request interrupted")
}

/// Removes `<system-reminder>…</system-reminder>` style blocks and similar tags.
pub fn strip_tags(s: &str) -> String {
    let mut out = s.to_string();
    for tag in [
        "system-reminder",
        "command-message",
        "local-command-stdout",
        "fx-turn-context",
        "turn-context",
        "environment_context",
    ] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        while let Some(a) = out.find(&open) {
            match out[a..].find(&close) {
                Some(b) => out.replace_range(a..a + b + close.len(), ""),
                None => {
                    out.truncate(a);
                    break;
                }
            }
        }
    }
    // Claude slash commands: keep the command itself.
    if let (Some(a), Some(b)) = (out.find("<command-name>"), out.find("</command-name>"))
        && a < b
    {
        let cmd = out[a + 14..b].to_string();
        return cmd;
    }
    out.trim().to_string()
}

/// The first line of `s`, whitespace collapsed, at most `max` characters.
pub fn one_line(s: &str, max: usize) -> String {
    let joined: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&joined, max)
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Clips to a display width, adding an ellipsis when cut.
pub fn fit(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

pub fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(Into::into)
}

/// `/home/me/x` → `~/x`.
pub fn tilde(p: &Path) -> String {
    if let Some(h) = home()
        && let Ok(rest) = p.strip_prefix(&h)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.display())
        };
    }
    p.display().to_string()
}

/// `~/x` → `/home/me/x`.
pub fn expand(s: &str) -> std::path::PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(h) = home()
    {
        return h.join(rest);
    }
    if s == "~"
        && let Some(h) = home()
    {
        return h;
    }
    s.into()
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "now", "5m", "3h", "2d", "3w", then "Mar 2025"-style.
pub fn ago(ts: i64, now: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..60 => "now".into(),
        60..3600 => format!("{}m", d / 60),
        3600..86400 => format!("{}h", d / 3600),
        86400..1_209_600 => format!("{}d", d / 86400),
        1_209_600..5_184_000 => format!("{}w", d / 604_800),
        _ => format!("{}mo", d / 2_592_000),
    }
}

/// Parses RFC 3339 / ISO 8601 timestamps such as `2026-11-25T06:39:45.909Z` or with an
/// offset, to Unix seconds. Good enough for sorting; no chrono needed.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut t = days * 86400 + h * 3600 + mi * 60 + sec;
    // Offset: find the last '+' or '-' after the time, or 'Z'.
    let tail = &s[19..];
    if let Some(pos) = tail.rfind(['+', '-']) {
        let off = &tail[pos..];
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let digits: String = off[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() >= 4 {
            let oh: i64 = digits[0..2].parse().ok()?;
            let om: i64 = digits[2..4].parse().ok()?;
            t -= sign * (oh * 3600 + om * 60);
        }
    }
    Some(t)
}

/// SQLite `YYYY-MM-DD HH:MM:SS` (UTC).
pub fn parse_sql_time(s: &str) -> Option<i64> {
    if s.len() >= 19 {
        parse_iso(&format!("{}T{}Z", &s[0..10], &s[11..19]))
    } else {
        None
    }
}

/// Word-wraps `text` to `width` columns, keeping explicit line breaks.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for raw in text.lines() {
        let raw = raw.trim_end().replace('\t', "    ");
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let indent: String = raw.chars().take_while(|c| *c == ' ').collect();
        let mut line = String::new();
        let mut w = 0;
        for word in raw.split(' ').filter(|w| !w.is_empty()) {
            let ww = word.width();
            if w > 0 && w + 1 + ww > width {
                out.push(std::mem::take(&mut line));
                w = 0;
            }
            if ww > width {
                // Hard-break very long words (paths, URLs).
                for ch in word.chars() {
                    let cw = ch.width().unwrap_or(0);
                    if w + cw > width {
                        out.push(std::mem::take(&mut line));
                        w = 0;
                    }
                    line.push(ch);
                    w += cw;
                }
                continue;
            }
            if w > 0 {
                line.push(' ');
                w += 1;
            } else if !indent.is_empty() && indent.len() + ww <= width {
                // Indented text (lists, code) stays indented on every wrapped line.
                line.push_str(&indent);
                w += indent.len();
            }
            line.push_str(word);
            w += ww;
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_times() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-11-25T06:39:45.909Z"), Some(1_795_588_785));
        assert_eq!(
            parse_iso("2026-11-21T11:34:11.720464604-08:00"),
            parse_iso("2026-11-21T19:34:11Z")
        );
        assert_eq!(parse_sql_time("1970-01-02 00:00:00"), Some(86400));
    }

    #[test]
    fn tags_are_stripped() {
        assert_eq!(
            strip_tags("hi <system-reminder>x</system-reminder> there"),
            "hi  there"
        );
        assert_eq!(
            strip_tags("<command-message>m</command-message><command-name>/model</command-name>"),
            "/model"
        );
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("aaa bbb ccc", 8), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("x\n\ny", 8), vec!["x", "", "y"]);
        assert_eq!(wrap("abcdefghijkl", 8), vec!["abcdefgh", "ijkl"]);
    }

    #[test]
    fn relative() {
        assert_eq!(ago(100, 130), "now");
        assert_eq!(ago(0, 7200), "2h");
        assert_eq!(ago(0, 3 * 86400), "3d");
    }
}
