//! The preview panel: a rounded box titled with the selection and a status
//! light, showing the session's real colors, SMPTE-style color bars while
//! tuning or without signal, and brief static when the selection changes.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::display::display;
use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Live,
    /// A reachable computer rather than a session.
    Ready,
    Updating,
    Tuning,
    /// No picture: the preview failed or the worker is unreachable.
    Lost,
    Standby,
}

/// What the screen shows; the caller builds it from the picker's selection.
pub struct Screen {
    pub label: String,
    /// Shown in the bottom border, e.g. the session's command and directory.
    pub detail: String,
    pub lines: Vec<Line<'static>>,
    pub signal: Signal,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Effects {
    /// While set, the screen shows static seeded by this value.
    pub static_seed: Option<u64>,
}

fn status(signal: Signal) -> (&'static str, Style) {
    match signal {
        Signal::Live => ("● LIVE", theme::success()),
        Signal::Ready => ("● READY", theme::success()),
        Signal::Updating => ("● UPDATING", theme::warning()),
        Signal::Tuning => ("● TUNING", theme::warning()),
        Signal::Lost => ("● NO SIGNAL", theme::error()),
        Signal::Standby => ("○ STANDBY", theme::muted()),
    }
}

pub fn render(frame: &mut Frame<'_>, area: Rect, screen: &Screen, effects: Effects) {
    let (light, light_style) = status(screen.signal);
    // Corners, spaces and the status light share the top border with the label.
    let room = usize::from(area.width).saturating_sub(light.width() + 6);
    let label = truncate(&display(&screen.label, 120), room);
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::muted_border())
        .title(Line::styled(format!(" {label} "), theme::muted()))
        .title(Line::from(vec![Span::styled(light, light_style), Span::raw(" ")]).right_aligned());
    if !screen.detail.is_empty() {
        let room = usize::from(area.width).saturating_sub(4);
        block = block.title_bottom(Line::styled(
            format!(" {} ", truncate(&display(&screen.detail, 240), room)),
            theme::muted(),
        ));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some(seed) = effects.static_seed {
        noise(frame.buffer_mut(), inner, seed);
        return;
    }
    let tuning = matches!(screen.signal, Signal::Tuning | Signal::Lost);
    if tuning && inner.height >= 8 && inner.width >= 14 {
        let bars = (inner.height / 2).min(6);
        color_bars(
            frame.buffer_mut(),
            Rect {
                height: bars,
                ..inner
            },
        );
        let caption = if screen.signal == Signal::Tuning {
            "░ TUNING… ░"
        } else {
            "░ NO SIGNAL ░"
        };
        let below = Rect {
            y: inner.y + bars + 1,
            height: inner.height - bars - 1,
            ..inner
        };
        let mut content = vec![
            Line::styled(caption, light_style).alignment(Alignment::Center),
            Line::raw(""),
        ];
        content.extend(screen.lines.iter().cloned());
        frame.render_widget(Paragraph::new(content), below);
        return;
    }
    // Show the end of the output, where the latest activity is.
    let skip = screen.lines.len().saturating_sub(usize::from(inner.height));
    frame.render_widget(
        Paragraph::new(screen.lines.iter().skip(skip).cloned().collect::<Vec<_>>()),
        inner,
    );
}

fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut result = String::new();
    for ch in text.chars() {
        if result.width() + ch.to_string().width() + 1 > width {
            break;
        }
        result.push(ch);
    }
    if width > 0 {
        result.push('…');
    }
    result
}

/// Seven bars in descending luminance: gray, yellow, cyan, green, magenta, red, blue.
const BARS: [(u8, u8, u8); 7] = [
    (192, 192, 192),
    (192, 192, 0),
    (0, 192, 192),
    (0, 192, 0),
    (192, 0, 192),
    (192, 0, 0),
    (0, 0, 192),
];

fn color_bars(buffer: &mut Buffer, area: Rect) {
    let count = BARS.len() as u16;
    for (i, &(r, g, b)) in BARS.iter().enumerate() {
        let i = i as u16;
        let start = area.x + area.width * i / count;
        let end = area.x + area.width * (i + 1) / count;
        for y in area.y..area.bottom() {
            for x in start..end {
                buffer[(x, y)].set_symbol(" ").set_bg(Color::Rgb(r, g, b));
            }
        }
    }
}

/// Static: xorshift noise over shade characters in grays.
fn noise(buffer: &mut Buffer, area: Rect, seed: u64) {
    const GRAIN: [&str; 5] = [" ", "░", "░", "▒", "▓"];
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let grain = GRAIN[(state % GRAIN.len() as u64) as usize];
            let level = 90 + ((state >> 8) % 3) as u8 * 60;
            buffer[(x, y)]
                .set_symbol(grain)
                .set_fg(Color::Rgb(level, level, level));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn draw(width: u16, height: u16, screen: &Screen, effects: Effects) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), screen, effects))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn screen(signal: Signal, lines: &[&str]) -> Screen {
        Screen {
            label: "devbox / project".into(),
            detail: String::new(),
            lines: lines
                .iter()
                .map(|line| Line::raw((*line).to_owned()))
                .collect(),
            signal,
        }
    }

    #[test]
    fn title_carries_only_label_and_status_light() {
        let buffer = draw(
            60,
            10,
            &screen(Signal::Live, &["hello"]),
            Effects::default(),
        );
        let view = text(&buffer);
        let top = view.lines().next().unwrap();
        assert!(top.starts_with("╭ devbox / project "), "{view}");
        assert!(!top.contains("CH "), "{view}");
        assert!(top.ends_with("● LIVE ╮"), "{view}");
        assert!(view.contains("hello"));
        assert_eq!(buffer[(0, 0)].fg, Color::DarkGray, "muted border");
    }

    #[test]
    fn tuning_and_no_signal_show_color_bars_and_a_caption() {
        let buffer = draw(
            60,
            16,
            &screen(Signal::Lost, &["Session changed"]),
            Effects::default(),
        );
        let view = text(&buffer);
        assert!(
            view.contains("NO SIGNAL") && view.contains("Session changed"),
            "{view}"
        );
        let backgrounds: Vec<Color> = (1..59).map(|x| buffer[(x, 1)].bg).collect();
        assert_eq!(backgrounds.first(), Some(&Color::Rgb(192, 192, 192)));
        assert_eq!(backgrounds.last(), Some(&Color::Rgb(0, 0, 192)));
        assert!(
            text(&draw(
                60,
                16,
                &screen(Signal::Tuning, &[]),
                Effects::default()
            ))
            .contains("TUNING")
        );
        let live = draw(60, 16, &screen(Signal::Live, &["out"]), Effects::default());
        assert_ne!(
            live[(1, 1)].bg,
            Color::Rgb(192, 192, 192),
            "live shows no bars"
        );
    }

    #[test]
    fn static_replaces_the_picture_while_changing_channel() {
        let effects = Effects {
            static_seed: Some(42),
        };
        let view = text(&draw(60, 14, &screen(Signal::Live, &["picture"]), effects));
        assert!(!view.contains("picture"), "{view}");
        assert!(view.contains('▒') || view.contains('▓'), "{view}");
    }

    #[test]
    fn long_labels_are_truncated_before_the_status_light() {
        let mut long = screen(Signal::Lost, &[]);
        long.label = "x".repeat(200);
        let view = text(&draw(50, 12, &long, Effects::default()));
        let top = view.lines().next().unwrap();
        assert!(top.contains('…') && top.contains("● NO SIGNAL"), "{top}");
    }
}
