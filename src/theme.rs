//! Delux's color roles. The selector panel is drawn in one monochrome tint
//! (cyan by default, or a green or amber phosphor), while the preview keeps
//! each session's real colors. Status colors are semantic and fixed:
//! green ready, yellow fixable, red failed.
//!
//! The default tint uses ANSI colors only, so the terminal theme decides the
//! exact shades; the phosphor tints are opt-in and paint their own background.

use ratatui::style::{Color, Modifier, Style};

use crate::app::Status;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tint {
    #[default]
    Cyan,
    Green,
    Amber,
}

impl Tint {
    pub fn next(self) -> Self {
        match self {
            Self::Cyan => Self::Green,
            Self::Green => Self::Amber,
            Self::Amber => Self::Cyan,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cyan => "cyan",
            Self::Green => "green phosphor",
            Self::Amber => "amber phosphor",
        }
    }

    fn colors(self) -> (Color, Color, Color) {
        match self {
            Self::Cyan => (Color::LightCyan, Color::Cyan, Color::Cyan),
            Self::Green => (
                Color::Rgb(190, 255, 200),
                Color::Rgb(51, 255, 102),
                Color::Rgb(26, 128, 51),
            ),
            Self::Amber => (
                Color::Rgb(255, 222, 150),
                Color::Rgb(255, 176, 0),
                Color::Rgb(140, 97, 0),
            ),
        }
    }

    /// The panel background; the ANSI tint keeps the terminal's own.
    pub fn background(self) -> Option<Color> {
        match self {
            Self::Cyan => None,
            Self::Green => Some(Color::Rgb(4, 18, 8)),
            Self::Amber => Some(Color::Rgb(20, 12, 0)),
        }
    }

    fn base(self, color: Color) -> Style {
        let style = Style::default().fg(color);
        match self.background() {
            Some(background) => style.bg(background),
            None => style,
        }
    }

    /// Primary text in the selector.
    pub fn text(self) -> Style {
        self.base(self.colors().1)
    }

    /// Section and computer headings.
    pub fn heading(self) -> Style {
        self.base(self.colors().0).add_modifier(Modifier::BOLD)
    }

    /// Details, counts and other secondary text.
    pub fn secondary(self) -> Style {
        let (_, _, dim) = self.colors();
        match self {
            // ANSI has no darker cyan; dimming adapts to the terminal theme.
            Self::Cyan => self.base(dim).add_modifier(Modifier::DIM),
            _ => self.base(dim),
        }
    }

    /// The selected row: a full-width bar in the tint.
    pub fn selected(self) -> Style {
        let (bright, base, _) = self.colors();
        match self.background() {
            Some(background) => Style::default()
                .fg(background)
                .bg(bright)
                .add_modifier(Modifier::BOLD),
            None => Style::default()
                .fg(base)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        }
    }

    /// The focused panel's border and title.
    pub fn border(self) -> Style {
        self.base(self.colors().1)
    }

    /// Accent for the app name and key labels in hints.
    pub fn accent(self) -> Style {
        Style::default()
            .fg(self.colors().1)
            .add_modifier(Modifier::BOLD)
    }
}

/// Borders of panels without focus, such as the preview.
pub fn muted_border() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Secondary text outside the tinted selector.
pub fn muted() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

pub fn success() -> Style {
    Style::default().fg(Color::Green)
}

pub fn warning() -> Style {
    Style::default().fg(Color::Yellow)
}

pub fn error() -> Style {
    Style::default().fg(Color::Red)
}

/// The color of a computer's status: ready, fixable from Delux, or failed.
pub fn status(status: Status) -> Style {
    match status {
        Status::Ready => success(),
        Status::AuthRequired | Status::HostTrust => warning(),
        Status::Scanning => muted(),
        Status::MissingTmux | Status::MissingTools | Status::ProbeFailed | Status::Unreachable => {
            error()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tint_uses_terminal_palette_only() {
        let tint = Tint::default();
        assert_eq!(tint.background(), None);
        for style in [
            tint.text(),
            tint.heading(),
            tint.secondary(),
            tint.selected(),
            tint.border(),
        ] {
            assert!(
                !matches!(style.fg, Some(Color::Rgb(..))) && style.bg.is_none(),
                "{style:?}"
            );
        }
    }

    #[test]
    fn tints_cycle_and_phosphors_paint_their_background() {
        assert_eq!(Tint::Cyan.next(), Tint::Green);
        assert_eq!(Tint::Green.next(), Tint::Amber);
        assert_eq!(Tint::Amber.next(), Tint::Cyan);
        for tint in [Tint::Green, Tint::Amber] {
            assert_eq!(tint.text().bg, tint.background());
            assert_eq!(
                tint.selected().fg,
                tint.background(),
                "bar text is the background"
            );
        }
    }
}
