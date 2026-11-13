//! Converts captured terminal output into styled ratatui lines. Only SGR
//! (color and text attribute) sequences are interpreted; every other escape
//! sequence is dropped and remaining control characters are escaped, while
//! graphic emoji sequences are preserved.

use std::iter::Peekable;
use std::str::Chars;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::display::preview_display;

const TAB_STOP: usize = 8;

/// Styled lines with tabs expanded to 8-column stops. Styles carry across
/// line breaks, as they do in a terminal.
pub fn styled_lines(text: &str) -> Vec<Line<'static>> {
    let mut parser = Parser::default();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => parser.escape(&mut chars),
            '\n' => parser.end_line(),
            '\t' => {
                let spaces = TAB_STOP - parser.column % TAB_STOP;
                parser.text.extend(std::iter::repeat_n(' ', spaces));
                parser.column += spaces;
            }
            _ => {
                parser.text.push(ch);
                parser.column += ch.width().unwrap_or(0);
            }
        }
    }
    parser.flush();
    if !parser.spans.is_empty() || parser.lines.is_empty() {
        parser.end_line();
    }
    parser.lines
}

/// Whether a captured line shows nothing but whitespace.
pub fn is_blank(line: &str) -> bool {
    styled_lines(line)
        .iter()
        .all(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
}

#[derive(Default)]
struct Parser {
    style: Style,
    text: String,
    column: usize,
    following_graphic: bool,
    spans: Vec<Span<'static>>,
    lines: Vec<Line<'static>>,
}

impl Parser {
    fn flush(&mut self) {
        if !self.text.is_empty() {
            let text = preview_display(&self.text, &mut self.following_graphic);
            self.text.clear();
            self.spans.push(Span::styled(text, self.style));
        }
    }

    fn end_line(&mut self) {
        self.flush();
        self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        self.column = 0;
        self.following_graphic = false;
    }

    /// Consumes one escape sequence after ESC, applying it only if it is SGR.
    fn escape(&mut self, chars: &mut Peekable<Chars<'_>>) {
        match chars.next() {
            Some('[') => {
                let mut parameters = String::new();
                let mut intermediate = false;
                while let Some(&ch) = chars.peek() {
                    match ch {
                        '\x30'..='\x3f' if !intermediate => parameters.push(ch),
                        '\x20'..='\x2f' => intermediate = true,
                        '\x40'..='\x7e' => {
                            chars.next();
                            if ch == 'm' && !intermediate {
                                self.flush();
                                self.style = apply_sgr(self.style, &parameters);
                            }
                            return;
                        }
                        // A malformed sequence ends here; the byte stays text.
                        _ => return,
                    }
                    chars.next();
                }
            }
            // OSC, DCS, SOS, PM and APC strings end with BEL or ST (ESC \).
            Some(']' | 'P' | 'X' | '^' | '_') => {
                while let Some(ch) = chars.next() {
                    if ch == '\x07' || (ch == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        return;
                    }
                }
            }
            // ESC, intermediates (e.g. `(` in ESC ( B), then one final byte.
            Some('\x20'..='\x2f') => {
                for ch in chars.by_ref() {
                    if !('\x20'..='\x2f').contains(&ch) {
                        return;
                    }
                }
            }
            // Other two-character escapes carry nothing to show.
            _ => {}
        }
    }
}

fn indexed(code: u16) -> Color {
    Color::Indexed(code as u8)
}

/// Reads an extended color after 38/48: `5;n` (256 colors) or `2;r;g;b`.
fn extended_color(values: &mut impl Iterator<Item = Option<u16>>) -> Option<Color> {
    let byte = |value: Option<u16>| value.and_then(|value| u8::try_from(value).ok());
    match values.next()?? {
        5 => byte(values.next()?).map(Color::Indexed),
        2 => {
            let (r, g, b) = (values.next()?, values.next()?, values.next()?);
            Some(Color::Rgb(byte(r)?, byte(g)?, byte(b)?))
        }
        _ => None,
    }
}

fn apply_sgr(mut style: Style, parameters: &str) -> Style {
    // Colon sub-parameters (e.g. 38:2::r:g:b) are rare in tmux output; treat
    // them like semicolons after dropping an empty color-space field.
    let normalized = parameters.replace("::", ":").replace(':', ";");
    let mut values = normalized.split(';').map(|value| {
        if value.is_empty() {
            Some(0)
        } else {
            value.parse::<u16>().ok()
        }
    });
    while let Some(value) = values.next() {
        let Some(code) = value else { continue };
        style = match code {
            0 => Style::default(),
            1 => style.add_modifier(Modifier::BOLD),
            2 => style.add_modifier(Modifier::DIM),
            3 => style.add_modifier(Modifier::ITALIC),
            4 | 21 => style.add_modifier(Modifier::UNDERLINED),
            7 => style.add_modifier(Modifier::REVERSED),
            9 => style.add_modifier(Modifier::CROSSED_OUT),
            22 => style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style.remove_modifier(Modifier::ITALIC),
            24 => style.remove_modifier(Modifier::UNDERLINED),
            27 => style.remove_modifier(Modifier::REVERSED),
            29 => style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style.fg(indexed(code - 30)),
            38 => match extended_color(&mut values) {
                Some(color) => style.fg(color),
                None => style,
            },
            39 => style.fg(Color::Reset),
            40..=47 => style.bg(indexed(code - 40)),
            48 => match extended_color(&mut values) {
                Some(color) => style.bg(color),
                None => style,
            },
            49 => style.bg(Color::Reset),
            90..=97 => style.fg(indexed(code - 90 + 8)),
            100..=107 => style.bg(indexed(code - 100 + 8)),
            // Blink, hidden text, fonts and other attributes are not shown.
            _ => style,
        };
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(line: &Line<'_>) -> Vec<(String, Style)> {
        line.spans
            .iter()
            .map(|span| (span.content.to_string(), span.style))
            .collect()
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    #[test]
    fn sgr_sequences_become_styles() {
        let lines = styled_lines(
            "\x1b[1;31mred bold\x1b[0m plain \x1b[38;5;208morange\x1b[39m \x1b[48;2;1;2;3mbg\x1b[m end",
        );
        assert_eq!(lines.len(), 1);
        let red_bold = Style::default()
            .fg(Color::Indexed(1))
            .add_modifier(Modifier::BOLD);
        assert_eq!(
            spans(&lines[0]),
            vec![
                ("red bold".into(), red_bold),
                (" plain ".into(), Style::default()),
                ("orange".into(), Style::default().fg(Color::Indexed(208))),
                (" ".into(), Style::default().fg(Color::Reset)),
                (
                    "bg".into(),
                    Style::default().fg(Color::Reset).bg(Color::Rgb(1, 2, 3))
                ),
                (" end".into(), Style::default()),
            ]
        );
        let bright = styled_lines("\x1b[92mok\x1b[22;2mdim");
        assert_eq!(bright[0].spans[0].style.fg, Some(Color::Indexed(10)));
        assert!(
            bright[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn styles_carry_across_lines_and_tabs_expand_by_width() {
        let lines = styled_lines("\x1b[32mgreen\nstill\x1b[0m\n東\tx\n");
        assert_eq!(lines.len(), 3, "a trailing newline adds no empty line");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Indexed(2)));
        // 東 is two columns wide, so the tab advances six columns.
        assert_eq!(text(&lines[2]), "東      x");
    }

    #[test]
    fn non_sgr_sequences_and_controls_never_reach_the_terminal() {
        let hostile = concat!(
            "a\x1b[2Jb\x1b[1;1Hc\x1b]0;title\x07d\x1b]8;;https://x\x1b\\e",
            "\x1bPdcs\x1b\\f\x1b[?1049hg\x1b[1 qh\x1b(Bi\x07j\u{9b}k\x1b"
        );
        let shown = text(&styled_lines(hostile)[0]);
        assert_eq!(shown, r"abcdefghi\x07j\x9bk");
    }

    #[test]
    fn malformed_and_out_of_range_parameters_are_ignored() {
        let lines = styled_lines("\x1b[38;5;999mx\x1b[38;2;1mY\x1b[99999mz\x1b[31");
        assert_eq!(text(&lines[0]), "xYz");
        assert!(lines[0].spans.iter().all(|span| span.style.fg.is_none()));
    }

    #[test]
    fn emoji_presentation_and_joiners_follow_graphics_only() {
        for (input, expected) in [
            (
                "⚠\u{fe0e} ⚠\u{fe0f} 👩\u{200d}💻",
                "⚠\u{fe0e} ⚠\u{fe0f} 👩\u{200d}💻",
            ),
            ("👩\u{fe0f}\u{200d}💻", "👩\u{fe0f}\u{200d}💻"),
            ("\u{fe0e}\u{fe0f}\u{200d}", r"\ufe0e\ufe0f\u200d"),
            (" \u{fe0f}\t\u{200d}", " \\ufe0f       \\u200d"),
            ("a\x07\u{fe0f}", r"a\x07\ufe0f"),
            ("a\u{200d}\u{200d}", "a\u{200d}\\u200d"),
            ("a\u{fe00}", r"a\ufe00"),
        ] {
            assert_eq!(text(&styled_lines(input)[0]), expected, "{input:?}");
        }
        let lines = styled_lines("⚠\x1b[31m\u{fe0f}\n\u{fe0f}");
        assert_eq!(
            text(&lines[0]),
            "⚠\u{fe0f}",
            "SGR preserves graphic context"
        );
        assert_eq!(text(&lines[1]), r"\ufe0f", "newlines reset graphic context");
    }

    #[test]
    fn blank_detection_ignores_escape_sequences() {
        assert!(is_blank(""));
        assert!(is_blank("  \x1b[0m \x1b[49m  "));
        assert!(!is_blank("\x1b[31m x"));
    }
}
