//! Keyboard, mouse, paste, and computer/right-panel form handling.

use std::io;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};

use super::{Command, Picker, RowKind, UserInput};
use crate::app::{Choice, Status};
use crate::display::display;
use crate::exit_codes;
use crate::session_form::{FormAction, SessionForm};
use crate::workers::{validate_destination, with_user};

impl Picker {
    pub(super) fn key(&mut self, key: KeyEvent) -> Option<Command> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        self.show_scroll_hint = false;
        if self.creation.is_some() {
            return self.creation_key(key);
        }
        if key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
        {
            if self.input.as_ref().is_some_and(|input| input.connect) {
                self.input = None;
                self.notice.clear();
            }
            self.terminal_focused = false;
            return None;
        }
        if self.terminal_focused {
            if key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.session.is_none())
            {
                return self.finish_terminal(Ok(exit_codes::INTERRUPTED));
            }
            if self.terminal_visible
                && let Some(active) = &mut self.active
                && let Err(error) = active.terminal.send_key(key)
            {
                if error.kind() == io::ErrorKind::WouldBlock {
                    self.notice = format!("Input not sent: {}", display(&error.to_string(), 200));
                    return None;
                }
                return self.finish_terminal(Err(error.to_string()));
            }
            return None;
        }
        if self.input.as_ref().is_some_and(|input| input.connect)
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.input = None;
            self.notice.clear();
            return None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.code = exit_codes::INTERRUPTED;
            self.quit();
            return None;
        }
        if self.input.is_some() {
            return self.input_key(key);
        }
        if self.add_input.is_some() {
            return self.add_input_key(key);
        }
        if self.filtering {
            match key.code {
                KeyCode::Enter => {
                    // Accepting the filter commits its row; only cancelling
                    // returns to the row selected before filtering.
                    self.filtering = false;
                    self.remember();
                    return None;
                }
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.rebuild();
                    return None;
                }
                KeyCode::Backspace => {
                    if self.filter.pop().is_some() {
                        self.rebuild();
                    }
                    return None;
                }
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {}
                _ => {
                    if let KeyCode::Char(ch) = key.code
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        self.filter.push(ch);
                        self.rebuild();
                    }
                    return None;
                }
            }
        }
        // Modified letters are not the corresponding normal-mode shortcuts.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-self.page()),
            KeyCode::PageDown => self.move_cursor(self.page()),
            KeyCode::Home | KeyCode::Char('g') => self.move_cursor(-(self.rows.len() as isize)),
            KeyCode::End | KeyCode::Char('G') => self.move_cursor(self.rows.len() as isize),
            KeyCode::Tab => {
                if self.active.is_some() {
                    self.terminal_focused = true;
                } else if self
                    .current_row()
                    .is_some_and(|row| self.session(row).is_some())
                {
                    return self.activate();
                }
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild();
                }
            }
            KeyCode::Char('f') => self.toggle_pin(),
            KeyCode::Char('n') => self.start_creation(),
            KeyCode::Char('u') => return self.start_user_input(false),
            KeyCode::Char('a') => {
                if let Some(reason) = self.services.add_computer_unavailable {
                    self.notice = reason.into();
                } else {
                    self.add_input = Some(String::new());
                    self.notice.clear();
                }
            }
            KeyCode::Char('p') => {
                self.tint = self.tint.next();
                self.notice = format!("Selector tint: {}.", self.tint.label());
            }
            KeyCode::Char('r') => {
                self.notice.clear();
                return Some(self.rescan(true));
            }
            KeyCode::Char('q') => self.quit(),
            KeyCode::Enter => return self.activate(),
            _ => {}
        }
        None
    }

    pub(super) fn start_creation(&mut self) {
        if self.creation_pending.is_some() {
            self.notice = "Waiting for the previous creation request to stop…".into();
            return;
        }
        let Some(row) = self.current_row() else {
            self.notice = "Select a computer before creating a session.".into();
            return;
        };
        let entry = &self.entries[row.entry];
        if entry.status() != Status::Ready {
            self.notice = if entry.worker.destination.is_some() {
                "Connect to this computer with Enter before creating a session.".into()
            } else {
                "This computer is not ready; check tmux and press r to rescan.".into()
            };
            return;
        }
        let directory = self
            .session(row)
            .map_or("", |session| session.path.as_str());
        self.creation = Some(SessionForm::new(entry.worker.clone(), directory.to_owned()));
        self.terminal_focused = false;
        self.notice.clear();
    }

    fn creation_key(&mut self, key: KeyEvent) -> Option<Command> {
        let action = self.creation.as_mut()?.key(key)?;
        match action {
            FormAction::Cancel => {
                self.creation = None;
                if let Some(cancel) = &self.creation_pending {
                    cancel.cancel();
                    self.notice =
                        "Stopping creation; a session may already have been created.".into();
                } else {
                    self.notice.clear();
                }
                None
            }
            FormAction::Submit(request) => {
                let form = self.creation.as_mut()?;
                if self.scanning {
                    form.busy = false;
                    form.error = "Wait for the rescan to finish before creating a session.".into();
                    return None;
                }
                if !self.entries.iter().any(|entry| {
                    entry.worker.key == form.worker.key
                        && entry.worker.destination == form.worker.destination
                        && entry.status() == Status::Ready
                }) {
                    form.busy = false;
                    form.error = "Computer is no longer ready. Esc to return and reconnect.".into();
                    return None;
                }
                let cancel = self.cancel.child();
                self.creation_pending = Some(cancel.clone());
                Some(Command::Create {
                    worker: form.worker.clone(),
                    request,
                    cancel,
                })
            }
        }
    }

    pub(super) fn terminal_mouse_event(&self, mut event: MouseEvent) -> Option<MouseEvent> {
        if !matches!(
            event.kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) || !self.terminal_focused
            || !self.terminal_visible
            || self.active.is_none()
            || self.input.is_some()
            || self.creation.is_some()
            || self.add_input.is_some()
            || self.filtering
            || !self
                .terminal_area
                .contains((event.column, event.row).into())
        {
            return None;
        }
        event.column -= self.terminal_area.x;
        event.row -= self.terminal_area.y;
        Some(event)
    }

    pub(super) fn mouse(&mut self, event: MouseEvent) -> Option<Command> {
        self.show_scroll_hint = false;
        if let Some(event) = self.terminal_mouse_event(event) {
            let active = self.active.as_mut()?;
            match active.terminal.scroll(event) {
                Ok(true) => {}
                Ok(false) => {
                    self.notice.clear();
                    self.notice.push_str(
                        "Scroll: tmux prefix+[ for copy mode, or set -g mouse on on that computer.",
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.notice = format!("Scroll not sent: {}", display(&error.to_string(), 200));
                }
                Err(error) => return self.finish_terminal(Err(error.to_string())),
            }
        } else if !self.terminal_focused
            && self.input.is_none()
            && self.creation.is_none()
            && self.add_input.is_none()
            && self.list_area.contains((event.column, event.row).into())
        {
            match event.kind {
                MouseEventKind::ScrollUp => self.move_cursor(-1),
                MouseEventKind::ScrollDown => self.move_cursor(1),
                _ => {}
            }
        } else if matches!(
            event.kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) && self
            .terminal_area
            .contains((event.column, event.row).into())
            && self.inactive_scroll_hint().is_some()
        {
            self.show_scroll_hint = true;
        }
        None
    }

    pub(super) fn inactive_scroll_hint(&self) -> Option<&'static str> {
        if self.terminal_focused
            || self.input.is_some()
            || self.creation.is_some()
            || self.add_input.is_some()
            || self.filtering
        {
            return None;
        }
        if self.terminal_visible {
            Some("Press Tab to interact")
        } else if self.preview_visible {
            Some(
                if self
                    .current_row()
                    .is_some_and(|row| self.session(row).is_some())
                {
                    "Press Enter to attach"
                } else {
                    "Select a session first"
                },
            )
        } else {
            None
        }
    }

    pub(super) fn paste(&mut self, text: &str) -> Option<Command> {
        if let Some(form) = &mut self.creation {
            form.paste(text);
            return None;
        }
        if self.terminal_focused {
            if self.terminal_visible
                && let Some(active) = &mut self.active
                && let Err(error) = active.terminal.paste(text)
            {
                if error.kind() == io::ErrorKind::WouldBlock {
                    self.notice = format!("Paste not sent: {}", display(&error.to_string(), 200));
                    return None;
                }
                return self.finish_terminal(Err(error.to_string()));
            }
        } else if let Some(input) = &mut self.input {
            input.stop_lookup();
            input.text.extend(
                text.chars()
                    .filter(|ch| !ch.is_control())
                    .take(64usize.saturating_sub(input.text.chars().count())),
            );
        } else if let Some(input) = &mut self.add_input {
            input.extend(text.chars().filter(|ch| !ch.is_control()));
        } else if self.filtering {
            self.filter
                .extend(text.chars().filter(|ch| !ch.is_control()));
            self.rebuild();
        }
        None
    }

    fn add_input_key(&mut self, key: KeyEvent) -> Option<Command> {
        let text = self.add_input.as_mut()?;
        match key.code {
            KeyCode::Esc => {
                self.add_input = None;
                self.notice.clear();
            }
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
            KeyCode::Enter => {
                if let Err(error) = validate_destination(text) {
                    self.notice = display(&error, 400);
                    return None;
                }
                // "local" is reserved even when --no-local hides its row.
                if text.as_str() == "local"
                    || self.entries.iter().any(|entry| {
                        entry.worker.name == *text
                            || entry.worker.key == *text
                            || entry.worker.destination.as_deref() == Some(text.as_str())
                    })
                {
                    self.notice =
                        "That computer name or SSH destination already exists; choose another."
                            .into();
                    return None;
                }
                // Like --host DEST, the destination itself is the computer's name.
                if let Err(error) = (self.services.add_mapping)(text, text) {
                    self.notice = format!("Could not save cli.json: {}", display(&error, 400));
                    return None;
                }
                self.notice = format!("Saved {} in cli.json; rescanning.", display(text, 200));
                self.select_added = self.add_input.take();
                return Some(self.rescan(false));
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !ch.is_control() =>
            {
                text.push(ch);
            }
            _ => {}
        }
        None
    }

    fn input_key(&mut self, key: KeyEvent) -> Option<Command> {
        let input = self.input.as_mut()?;
        match key.code {
            KeyCode::Esc => {
                self.input = None;
                self.notice.clear();
            }
            KeyCode::Backspace => {
                input.stop_lookup();
                input.text.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.stop_lookup();
                input.text.clear();
            }
            KeyCode::Enter => {
                if input.lookup.is_some() {
                    self.notice =
                        "Reading SSH settings; wait or type a username to override.".into();
                    return None;
                }
                if input.connect && input.text.trim().is_empty() {
                    self.notice = "Enter the username on the remote computer.".into();
                    return None;
                }
                let using_ssh_user =
                    input.connect && input.ssh_user.as_deref() == Some(input.text.as_str());
                let destination = if using_ssh_user {
                    input.destination.clone()
                } else {
                    match with_user(&input.destination, input.text.trim()) {
                        Ok(destination) => destination,
                        Err(error) => {
                            self.notice = display(&error, 400);
                            return None;
                        }
                    }
                };
                let entry_index = if input.connect {
                    let Some(index) = self.entries.iter().position(|entry| {
                        entry.worker.key == input.key
                            && entry.worker.destination.as_deref() == Some(&input.destination)
                    }) else {
                        self.notice =
                            "Computer configuration changed; cancel and try again.".into();
                        return None;
                    };
                    Some(index)
                } else {
                    None
                };
                if (destination != input.destination || !input.connect)
                    && let Err(error) = (self.services.save_mapping)(&input.key, &destination)
                {
                    self.notice = format!("Could not save cli.json: {}", display(&error, 400));
                    return None;
                }
                let input = self.input.take()?;
                if let Some(index) = entry_index {
                    let entry = &mut self.entries[index];
                    entry.worker.destination = Some(destination);
                    if let Some(inventory) = &mut entry.inventory {
                        inventory.worker = entry.worker.clone();
                        inventory.sessions.clear();
                    }
                    let worker = entry.worker.clone();
                    self.previews.retain(|(key, _), _| *key != worker.key);
                    self.notice = format!(
                        "Signing in as {}. Verify any host fingerprint through a trusted channel.",
                        display(&input.text, 64),
                    );
                    return Some(Command::Launch(Choice {
                        worker,
                        session: None,
                    }));
                }
                self.notice = format!(
                    "Saved {} → {} in cli.json; rescanning.",
                    display(&input.name, 80),
                    display(&destination, 200)
                );
                return Some(self.rescan(false));
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && input.text.chars().count() < 64 =>
            {
                input.stop_lookup();
                input.text.push(ch);
            }
            _ => {}
        }
        None
    }

    pub(super) fn start_user_input(&mut self, connect: bool) -> Option<Command> {
        let row = self.current_row()?;
        if !matches!(
            row.kind,
            RowKind::Computer | RowKind::Session | RowKind::Pinned
        ) {
            return None;
        }
        let worker = &self.entries[row.entry].worker;
        let Some(destination) = &worker.destination else {
            self.notice = "The local computer uses your own account; nothing to map.".into();
            return None;
        };
        let current = destination
            .rsplit_once('@')
            .map(|(user, _)| user.to_owned())
            .unwrap_or_default();
        let lookup = (connect && current.is_empty()).then(|| self.cancel.child());
        let command = lookup.as_ref().map(|cancel| Command::ResolveUser {
            worker: worker.clone(),
            cancel: cancel.clone(),
        });
        self.input = Some(UserInput {
            key: worker.key.clone(),
            name: worker.name.clone(),
            destination: destination.clone(),
            text: current,
            connect,
            ssh_user: None,
            lookup,
        });
        self.notice.clear();
        command
    }
}
