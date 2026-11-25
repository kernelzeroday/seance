//! Mouse selection in the preview. The preview is pre-rendered into [`PLine`]s that keep, next
//! to the styled line, the plain text it shows: copying a selection takes that text, without
//! the gutters (`│ `, `⚙ `) and borders a terminal's own selection would drag along, and joins
//! wrapped lines back into the lines they came from.

use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

pub struct PLine {
    pub line: Line<'static>,
    /// What copying this line yields (the full text, even where the screen clips it).
    pub text: String,
    /// Columns of decoration before the text.
    pub gutter: u16,
    /// Columns the line takes on screen, gutter included.
    pub width: u16,
    /// A wrapped continuation of the line above (joined with a space when copied).
    pub soft: bool,
    /// Which turn of the transcript the line belongs to.
    pub turn: usize,
    /// The turn's "› you" / "● agent" header.
    pub header: bool,
}

/// A point in the preview: (line index in the rendered transcript, screen column in the body).
pub type Pos = (usize, u16);

pub fn ordered(a: Pos, b: Pos) -> (Pos, Pos) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The part of `s` between display columns `from` (inclusive) and `to` (exclusive).
fn slice_cols(s: &str, from: u16, to: u16) -> String {
    let mut out = String::new();
    let mut col = 0u16;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0) as u16;
        if col >= to {
            break;
        }
        if col + w > from {
            out.push(ch);
        }
        col += w;
    }
    out
}

/// The text between two points, inclusive of both.
pub fn extract(lines: &[PLine], a: Pos, b: Pos) -> String {
    let (a, b) = ordered(a, b);
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate().take(b.0 + 1).skip(a.0) {
        let c0 = if i == a.0 { a.1 } else { 0 };
        let c1 = if i == b.0 {
            b.1.saturating_add(1)
        } else {
            u16::MAX
        };
        // Past the end of what the screen shows means "to the end of the line".
        let c1 = if c1 >= l.width { u16::MAX } else { c1 };
        let piece = slice_cols(
            &l.text,
            c0.saturating_sub(l.gutter),
            c1.saturating_sub(l.gutter),
        );
        if i > a.0 {
            if l.soft {
                out.push(' ');
                out.push_str(piece.trim_start());
                continue;
            }
            out.push('\n');
        }
        out.push_str(&piece);
    }
    out.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_string()
}

/// The columns of line `i` to highlight for a selection from `a` to `b`, as `[start, end)`.
pub fn highlight(lines: &[PLine], a: Pos, b: Pos, i: usize) -> Option<(u16, u16)> {
    let (a, b) = ordered(a, b);
    if i < a.0 || i > b.0 {
        return None;
    }
    let l = lines.get(i)?;
    let start = if i == a.0 { a.1 } else { 0 }.max(l.gutter);
    let end = if i == b.0 {
        b.1.saturating_add(1)
    } else {
        u16::MAX
    }
    .min(l.width);
    (start < end).then_some((start, end))
}

/// The body of the turn line `i` belongs to (its header left out), for a double-click.
pub fn turn_span(lines: &[PLine], i: usize) -> Option<(Pos, Pos)> {
    let turn = lines.get(i)?.turn;
    let body: Vec<usize> = (0..lines.len())
        .filter(|&j| lines[j].turn == turn && !lines[j].header && !lines[j].text.is_empty())
        .collect();
    Some(((*body.first()?, 0), (*body.last()?, u16::MAX - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pl(text: &str, gutter: u16, soft: bool, turn: usize, header: bool) -> PLine {
        PLine {
            line: Line::raw(""),
            text: text.into(),
            gutter,
            width: gutter + text.chars().count() as u16,
            soft,
            turn,
            header,
        }
    }

    fn sample() -> Vec<PLine> {
        vec![
            pl("you", 2, false, 0, true),
            pl("the quick brown", 2, false, 0, false),
            pl("fox jumps", 2, true, 0, false),
            pl("second line", 2, false, 0, false),
            pl("", 0, false, 0, false),
            pl("claude", 2, false, 1, true),
            pl("reply", 2, false, 1, false),
        ]
    }

    #[test]
    fn extract_skips_gutters_and_rejoins_wraps() {
        let l = sample();
        // From "quick" to the end of "second": the gutter columns 0-1 never count.
        assert_eq!(extract(&l, (1, 6), (3, 7)), "quick brown fox jumps\nsecond");
        // Clicking in the gutter starts at the text.
        assert_eq!(extract(&l, (1, 0), (1, 4)), "the");
        // Backwards drags work the same.
        assert_eq!(extract(&l, (2, 4), (1, 12)), "brown fox");
    }

    #[test]
    fn highlight_stays_on_text() {
        let l = sample();
        assert_eq!(highlight(&l, (1, 6), (3, 7), 1), Some((6, 17)));
        assert_eq!(highlight(&l, (1, 6), (3, 7), 2), Some((2, 11)));
        assert_eq!(highlight(&l, (1, 6), (3, 7), 3), Some((2, 8)));
        assert_eq!(highlight(&l, (1, 6), (3, 7), 5), None);
    }

    #[test]
    fn double_click_takes_the_turn() {
        let l = sample();
        let (a, b) = turn_span(&l, 2).unwrap();
        assert_eq!(extract(&l, a, b), "the quick brown fox jumps\nsecond line");
        let (a, b) = turn_span(&l, 5).unwrap();
        assert_eq!(extract(&l, a, b), "reply");
    }
}
