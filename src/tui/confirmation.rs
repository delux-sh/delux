use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use crate::{
    display::display,
    theme::{self, Tint},
    tmux::Session,
    workers::Worker,
};

pub(super) enum ConfirmationAction {
    Cancel,
    End,
}

pub(super) struct EndConfirmation {
    pub worker: Worker,
    pub session: Session,
    pub busy: bool,
    pub error: String,
    end_selected: bool,
    can_confirm: bool,
}

impl EndConfirmation {
    pub fn new(worker: Worker, session: Session) -> Self {
        Self {
            worker,
            session,
            busy: false,
            error: String::new(),
            end_selected: false,
            can_confirm: false,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<ConfirmationAction> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Some(ConfirmationAction::Cancel);
        }
        if self.busy
            || key.kind == KeyEventKind::Repeat
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down => self.end_selected = !self.end_selected,
            KeyCode::Enter => {
                if !self.end_selected {
                    return Some(ConfirmationAction::Cancel);
                }
                if self.can_confirm {
                    return Some(ConfirmationAction::End);
                }
                self.error = "Enlarge the terminal to view the full confirmation.".into();
            }
            _ => {}
        }
        None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, tint: Tint) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::warning())
            .title(" End session… ")
            .style(tint.text());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let details = [
            format!(
                "Session: {} ({})",
                display(&self.session.name, usize::MAX),
                self.session.id
            ),
            format!("Computer: {}", display(&self.worker.name, usize::MAX)),
            format!(
                "Destination: {}",
                display(
                    self.worker
                        .destination
                        .as_deref()
                        .unwrap_or("local computer"),
                    usize::MAX
                )
            ),
        ];
        let mut lines = Vec::new();
        for detail in &details {
            wrap_line(&mut lines, detail, inner.width, tint.heading());
        }
        lines.push(Line::raw(""));
        wrap_line(
            &mut lines,
            "This closes all windows and panes and can terminate running programs or agents, losing unsaved work.",
            inner.width,
            theme::warning(),
        );
        wrap_line(
            &mut lines,
            "This cannot be undone. Other sessions are not ended.",
            inner.width,
            tint.text(),
        );
        lines.push(Line::raw(""));
        if self.busy {
            wrap_line(
                &mut lines,
                "Ending session… Waiting for confirmation; do not retry.",
                inner.width,
                tint.secondary(),
            );
        } else {
            lines.push(Line::from(vec![
                Span::styled(
                    "[ Cancel ]",
                    if self.end_selected {
                        tint.text()
                    } else {
                        tint.selected()
                    },
                ),
                Span::raw("   "),
                Span::styled(
                    "[ End session ]",
                    if self.end_selected {
                        tint.selected()
                    } else {
                        theme::warning()
                    },
                ),
            ]));
        }
        let error = display(&self.error, 600);
        wrap_line(&mut lines, &error, inner.width, theme::error());
        wrap_line(
            &mut lines,
            if self.busy {
                "Esc/Ctrl-C stops waiting; the session may already have ended."
            } else {
                "Tab/arrows choose · Enter selects · Esc/Ctrl-C cancels"
            },
            inner.width,
            tint.secondary(),
        );
        // Never accept a destructive choice before its complete warning and
        // captured destination have actually been displayed.
        self.can_confirm = inner.width >= 28 && usize::from(inner.height) >= lines.len();
        frame.render_widget(Paragraph::new(lines), inner);
        if !self.can_confirm && inner.height != 0 {
            frame.render_widget(
                Paragraph::new("Enlarge terminal; Esc cancels.").style(theme::warning()),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
    }
}

fn wrap_line<'a>(lines: &mut Vec<Line<'a>>, text: &'a str, width: u16, style: Style) {
    let mut start = 0;
    let mut columns = 0;
    for (index, character) in text.char_indices() {
        let next = character.width().unwrap_or(0);
        if columns + next > usize::from(width) && index > start {
            lines.push(Line::styled(&text[start..index], style));
            start = index;
            columns = 0;
        }
        columns += next;
    }
    lines.push(Line::styled(&text[start..], style));
}
