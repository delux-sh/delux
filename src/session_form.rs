use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Rect,
    text::Line,
    widgets::{Block, BorderType, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    display::display,
    theme::{self, Tint},
    tmux::NewSession,
    workers::Worker,
};

pub enum FormAction {
    Submit(NewSession),
    Cancel,
}

pub struct SessionForm {
    pub worker: Worker,
    pub busy: bool,
    pub error: String,
    values: [String; 3],
    field: usize,
}

impl SessionForm {
    pub fn new(worker: Worker, directory: String) -> Self {
        Self {
            worker,
            busy: false,
            error: String::new(),
            values: [
                String::new(),
                if directory.is_empty() {
                    "~".into()
                } else {
                    directory
                },
                String::new(),
            ],
            field: 0,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<FormAction> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if matches!(key.code, KeyCode::Esc | KeyCode::BackTab)
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Some(FormAction::Cancel);
        }
        if self.busy {
            return None;
        }
        if key.code == KeyCode::Char('u') && key.modifiers == KeyModifiers::CONTROL {
            self.values[self.field].clear();
            self.error.clear();
            return None;
        }
        if key.modifiers.intersects(
            KeyModifiers::CONTROL
                | KeyModifiers::ALT
                | KeyModifiers::SUPER
                | KeyModifiers::HYPER
                | KeyModifiers::META,
        ) {
            return None;
        }
        match key.code {
            KeyCode::Enter => {
                let session = NewSession {
                    name: self.values[0].clone(),
                    directory: self.values[1].clone(),
                    command: self.values[2].clone(),
                };
                match session.validate() {
                    Ok(()) => {
                        self.error.clear();
                        self.busy = true;
                        return Some(FormAction::Submit(session));
                    }
                    Err(error) => self.error = error,
                }
            }
            KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % 3,
            KeyCode::Up => self.field = (self.field + 2) % 3,
            KeyCode::Backspace => {
                self.values[self.field].pop();
                self.error.clear();
            }
            KeyCode::Char(character) => self.paste(character.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        None
    }

    pub fn paste(&mut self, text: &str) {
        if self.busy {
            return;
        }
        let limit: usize = if self.field == 0 { 64 } else { 4096 };
        let value = &mut self.values[self.field];
        let remaining = limit.saturating_sub(value.chars().count());
        value.extend(text.chars().filter(|c| !c.is_control()).take(remaining));
        self.error.clear();
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, tint: Tint) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(format!(
                " New session · {} ",
                display(&self.worker.name, 80)
            ))
            .border_style(tint.border())
            .style(tint.text());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // Preserve an editable field first, then an error/status row. Help and
        // the other fields yield space on short terminals rather than hiding focus.
        let status = if !self.error.is_empty() {
            Some((display(&self.error, 4096), theme::error()))
        } else if self.busy {
            Some(("Creating… Esc cancels".to_owned(), tint.secondary()))
        } else {
            None
        };
        let status_rows = status
            .as_ref()
            .map_or(0, |(text, _)| {
                UnicodeWidthStr::width(text.as_str())
                    .div_ceil(usize::from(inner.width))
                    .clamp(1, 3) as u16
            })
            .min(inner.height.saturating_sub(1));
        let available = inner.height - status_rows;
        let field_height = if available >= 2 { 2 } else { 1 };
        let field_count = usize::from((available / field_height).min(3));
        let first = self.field.saturating_sub(field_count - 1);
        let labels = ["Name", "Directory", "Command (optional)"];
        let mut y = inner.y;
        for (index, label) in labels.iter().enumerate().skip(first).take(field_count) {
            let focused = index == self.field;
            if field_height == 2 {
                frame.render_widget(
                    Paragraph::new(*label).style(if focused {
                        tint.heading()
                    } else {
                        tint.secondary()
                    }),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
            let rendered = display(&self.values[index], 4096);
            let visible = if focused {
                suffix(&rendered, usize::from(inner.width.saturating_sub(1)))
            } else {
                &rendered
            };
            frame.render_widget(
                Paragraph::new(Line::raw(visible)).style(if focused {
                    tint.selected()
                } else {
                    tint.text()
                }),
                Rect::new(inner.x, y, inner.width, 1),
            );
            if focused && !self.busy {
                let offset =
                    UnicodeWidthStr::width(visible).min(usize::from(inner.width - 1)) as u16;
                frame.set_cursor_position((inner.x + offset, y));
            }
            y += 1;
        }
        if let Some((text, style)) = status
            && status_rows != 0
        {
            frame.render_widget(
                Paragraph::new(text).style(style).wrap(Wrap { trim: false }),
                Rect::new(inner.x, y, inner.width, status_rows),
            );
            y += status_rows;
        }
        let bottom = inner.y + inner.height;
        for hint in [
            "Directory: ~, ~/path, or an absolute path on that computer.",
            "Command: omp / claude / codex; blank uses the normal shell.",
            "When the command ends, the session may end.",
        ] {
            if y >= bottom {
                break;
            }
            frame.render_widget(
                Paragraph::new(hint).style(tint.secondary()),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
    }
}

// Keep the end of an input and its real cursor visible, accounting for wide
// characters and the escaped representation of invisible Unicode characters.
fn suffix(text: &str, width: usize) -> &str {
    let mut start = text.len();
    let mut used = 0;
    for (index, character) in text.char_indices().rev() {
        let columns = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if used + columns > width {
            break;
        }
        used += columns;
        start = index;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> SessionForm {
        SessionForm::new(
            Worker {
                key: "local".into(),
                name: "Local".into(),
                destination: None,
                online: Some(true),
            },
            "~".into(),
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn submitted_form_refuses_repeats_and_edits_but_allows_cancel() {
        let mut form = form();
        form.paste("work");
        assert!(matches!(
            form.key(key(KeyCode::Enter)),
            Some(FormAction::Submit(_))
        ));
        let values = form.values.clone();
        assert!(form.key(key(KeyCode::Enter)).is_none());
        form.key(key(KeyCode::Backspace));
        form.key(key(KeyCode::Tab));
        form.paste("ignored");
        assert_eq!(form.values, values);
        assert_eq!(form.field, 0);
        for cancel in [
            key(KeyCode::Esc),
            key(KeyCode::BackTab),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            assert!(matches!(form.key(cancel), Some(FormAction::Cancel)));
        }
    }

    #[test]
    fn paste_is_bounded_text_not_submission_or_shortcuts() {
        let mut form = form();
        form.paste("work\r\n\t\u{1b}\u{3}\u{15}");
        assert_eq!(form.values[0], "work");
        assert!(!form.busy);
        assert_eq!(form.field, 0);
        form.paste(&"界".repeat(100));
        assert_eq!(form.values[0].chars().count(), 64);
        form.key(key(KeyCode::Tab));
        form.paste(&"界".repeat(5000));
        assert_eq!(form.values[1].chars().count(), 4096);
        assert!(!form.busy);
    }

    #[test]
    fn inherited_directory_is_not_silently_rewritten() {
        let mut form = SessionForm::new(form().worker, "/work\nother".into());
        form.paste("work");
        assert!(form.key(key(KeyCode::Enter)).is_none());
        assert!(!form.busy);
    }

    #[test]
    fn invalid_submission_keeps_form_editable_and_release_does_not_submit() {
        let mut form = form();
        form.paste("bad.name");
        assert!(form.key(key(KeyCode::Enter)).is_none());
        assert!(!form.error.is_empty());
        assert!(!form.busy);
        form.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        form.paste("valid");
        let mut release = key(KeyCode::Enter);
        release.kind = KeyEventKind::Release;
        assert!(form.key(release).is_none());
        assert!(!form.busy);
        assert!(matches!(
            form.key(key(KeyCode::Enter)),
            Some(FormAction::Submit(_))
        ));
    }
}
