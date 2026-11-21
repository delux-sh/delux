//! Picker widgets, preview content, and display text.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, Padding, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{Entry, Picker, Row, RowKind, SPINNER, diagnostic_lines, host_part, layout};
use crate::app::Status;
use crate::crt::{self, Effects, Screen, Signal};
use crate::display::display;
use crate::theme;
use crate::tmux::{AgentState, AgentStatus, Session};

impl Picker {
    fn counts(&self) -> (usize, usize, usize) {
        let sessions = self
            .entries
            .iter()
            .map(|entry| entry.sessions().len())
            .sum();
        let setup = self
            .entries
            .iter()
            .filter(|entry| entry.needs_setup())
            .count();
        (sessions, self.entries.len(), setup)
    }

    pub(super) fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let tint = self.tint;
        self.terminal_visible = false;
        self.list_area = Rect::default();
        if let Some(confirmation) = &mut self.ending {
            self.preview_visible = false;
            self.list_height = 0;
            confirmation.render(frame, area, tint);
            return;
        }
        let signing_in = self.input.as_ref().is_some_and(|input| input.connect);
        let creating = self.creation.is_some();
        if creating && area.height < 5 {
            self.preview_visible = false;
            self.list_height = 0;
            if let Some(form) = &self.creation {
                form.render(frame, area, tint);
            }
            return;
        }
        if signing_in && area.height < 5 {
            self.preview_visible = false;
            self.list_height = 0;
            self.render_sign_in(frame, area);
            return;
        }
        if self.active.is_some() && self.terminal_focused && area.height < 5 {
            self.preview_visible = false;
            self.list_height = 0;
            self.render_terminal(frame, area);
            return;
        }
        let mut status_lines: Vec<Line<'static>> = Vec::new();
        if let Some(input) = &self.input
            && !input.connect
        {
            status_lines.push(Line::styled(
                format!(
                    "SSH user for {}: {}▏  (saved as user@{} in cli.json)",
                    display(&input.name, 80),
                    display(&input.text, 64),
                    display(host_part(&input.destination), 120)
                ),
                tint.accent(),
            ));
        }
        if let Some(text) = &self.add_input {
            status_lines.push(Line::styled(
                format!(
                    "Add SSH destination/alias: {}▏  (e.g. alice@build-box or ~/.ssh/config alias)",
                    display(text, 1024),
                ),
                tint.accent(),
            ));
        }
        if self.filtering || !self.filter.is_empty() {
            status_lines.push(Line::from(vec![
                Span::styled("/", tint.accent()),
                Span::raw(display(&self.filter, 200)),
                Span::styled(if self.filtering { "▏" } else { "" }, tint.accent()),
            ]));
        }
        for warning in [&self.warning, &self.state_error] {
            if !warning.is_empty() {
                status_lines.push(Line::styled(
                    format!("! {}", display(warning, 600)),
                    theme::warning(),
                ));
            }
        }
        if !self.notice.is_empty() {
            status_lines.push(Line::styled(self.notice.clone(), theme::warning()));
        }
        let [header, body, status, footer] = layout::chrome(area, status_lines.len());
        let (sessions, computers, setup) = self.counts();
        frame.render_widget(
            Paragraph::new(self.header_line(sessions, computers, setup)),
            header,
        );
        frame.render_widget(Paragraph::new(status_lines), status);

        let layout::Panes {
            list: list_area,
            preview: preview_area,
        } = layout::panes(
            body,
            self.rows.len(),
            self.active.is_some(),
            signing_in || creating || (self.active.is_some() && self.terminal_focused),
        );
        let show_terminal = !signing_in && !creating && self.shows_terminal();
        self.preview_visible = !signing_in && !creating && !show_terminal && preview_area.is_some();
        self.list_height = list_area.height;

        let sidebar_inactive = self.terminal_focused || signing_in || creating;
        let items = self.render_rows(list_area.width, sidebar_inactive);
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(if sidebar_inactive {
                theme::inactive_sidebar()
            } else {
                tint.border()
            })
            .title(Line::styled(
                format!(" Computers · {computers} "),
                if sidebar_inactive {
                    theme::inactive_sidebar()
                } else {
                    tint.heading()
                },
            ));
        if sidebar_inactive {
            block = block.style(theme::inactive_sidebar());
        } else if let Some(background) = tint.background() {
            block = block.style(Style::default().bg(background));
        }
        let items = if !items.is_empty() {
            items
        } else {
            vec![ListItem::new(Line::styled(
                self.empty_message(),
                if sidebar_inactive {
                    theme::inactive_sidebar()
                } else {
                    tint.secondary()
                },
            ))]
        };
        self.list_area = block.inner(list_area);
        self.list_state.select(self.cursor);
        let list = List::new(items)
            .block(block)
            .highlight_symbol("❯ ")
            .highlight_style(if sidebar_inactive {
                theme::inactive_selection()
            } else {
                tint.selected()
            });
        frame.render_stateful_widget(list, list_area, &mut self.list_state);
        // List highlighting otherwise overwrites semantic marker colours.
        let marker_x = self.list_area.x + 2;
        if marker_x < self.list_area.right() {
            let now = (self.services.now)();
            for (y, row) in self
                .rows
                .iter()
                .skip(self.list_state.offset())
                .take(usize::from(self.list_area.height))
                .enumerate()
            {
                if let Some(session) = self.session(*row)
                    && visible_agent(session, now).is_some()
                {
                    let marker = agent_marker(session, now, tint, sidebar_inactive);
                    frame.buffer_mut()[(marker_x, self.list_area.y + y as u16)]
                        .set_style(marker.style.remove_modifier(Modifier::REVERSED));
                }
            }
        }
        if let Some(area) = preview_area {
            if let Some(form) = &self.creation {
                self.terminal_area = Block::bordered().inner(area);
                form.render(frame, area, tint);
            } else if signing_in {
                self.render_sign_in(frame, area);
            } else if show_terminal {
                self.render_terminal(frame, area);
            } else {
                self.terminal_area = Block::bordered().inner(area);
                self.render_preview(frame, area);
            }
        }
        if self.show_scroll_hint
            && let Some(hint) = self.inactive_scroll_hint()
        {
            let area = self.terminal_area;
            if area.width >= 4 && area.height >= 3 {
                let width = (hint.width() as u16 + 6).min(area.width);
                let height = 5.min(area.height);
                let overlay = Rect::new(
                    area.x + (area.width - width) / 2,
                    area.y + (area.height - height) / 2,
                    width,
                    height,
                );
                let block = Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(tint.accent())
                    .title(" Scroll ")
                    .padding(Padding::new(1, 1, u16::from(height >= 5), 0));
                frame.render_widget(ratatui::widgets::Clear, overlay);
                frame.render_widget(
                    Paragraph::new(hint)
                        .style(tint.heading())
                        .centered()
                        .wrap(Wrap { trim: true })
                        .block(block),
                    overlay,
                );
            }
        }
        frame.render_widget(Paragraph::new(self.footer_line(footer.width)), footer);
    }

    fn render_sign_in(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(input) = &self.input else {
            return;
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(self.tint.accent())
            .title(Line::styled(
                format!(" Sign in · {} ", display(&input.name, 80)),
                self.tint.heading(),
            ));
        let content = if area.width < 3 || area.height < 3 {
            area
        } else {
            let content = block.inner(area);
            frame.render_widget(block, area);
            content
        };
        self.terminal_area = content;
        let lines = vec![
            Line::styled(
                format!("Remote username: {}▏", display(&input.text, 64)),
                self.tint.accent(),
            ),
            Line::raw(""),
            Line::raw(if input.lookup.is_some() {
                "Reading SSH settings…"
            } else {
                "Confirm the account name on this computer."
            }),
            Line::styled(
                if input.ssh_user.as_deref() == Some(input.text.as_str()) {
                    "SSH settings/default; a key does not prove the username."
                } else {
                    "Type a username or Ctrl-U to change it."
                },
                theme::muted(),
            ),
            Line::raw(""),
            Line::raw("Enter  Confirm and sign in"),
            Line::raw("Ctrl-U Clear / change username"),
            Line::raw("Esc    Back"),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), content);
    }

    fn render_terminal(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(active) = &mut self.active else {
            return;
        };
        let title = match &active.session {
            Some(session) => format!(
                " {} · {} / {} ",
                if self.terminal_focused {
                    "Active · Shift-Tab for list"
                } else {
                    "Connected · Tab to interact"
                },
                display(&active.worker.name, 80),
                display(&session.name, 80),
            ),
            None => format!(
                " {} · {} · sign in ",
                display(&active.worker.name, 80),
                display(
                    active
                        .worker
                        .destination
                        .as_deref()
                        .and_then(|destination| destination.rsplit_once('@'))
                        .map_or("SSH config", |(user, _)| user),
                    64,
                ),
            ),
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(if self.terminal_focused {
                self.tint.accent()
            } else {
                theme::muted()
            })
            .title(Line::styled(
                title,
                if self.terminal_focused {
                    self.tint.heading()
                } else {
                    theme::muted()
                },
            ));
        let content = if area.width < 3 || area.height < 3 {
            area
        } else {
            let content = block.inner(area);
            frame.render_widget(block, area);
            content
        };
        self.terminal_area = content;
        self.terminal_visible = content.width > 0 && content.height > 0;
        if !self.terminal_visible {
            return;
        }
        if let Err(error) = active.terminal.resize(content.height, content.width) {
            self.terminal_error = Some(error.to_string());
            self.terminal_visible = false;
            return;
        }
        active
            .terminal
            .render(frame, content, self.terminal_focused);
    }

    fn header_line(&self, sessions: usize, computers: usize, setup: usize) -> Line<'static> {
        let mut spans = vec![
            Span::styled(" delux", self.tint.accent()),
            Span::styled(
                format!(
                    "  {} · {}",
                    plural(sessions as i64, "session"),
                    plural(computers as i64, "computer")
                ),
                theme::muted(),
            ),
        ];
        if setup > 0 {
            spans.push(Span::styled(
                format!(" · {setup} need{} setup", if setup == 1 { "s" } else { "" }),
                theme::warning(),
            ));
        }
        if self.scanning {
            let probed = self.entries.len().saturating_sub(self.pending.len());
            spans.push(Span::styled(
                format!(
                    " · {} probing {probed}/{}",
                    SPINNER[self.frame],
                    self.entries.len()
                ),
                theme::muted(),
            ));
        }
        let waiting = self
            .entries
            .iter()
            .flat_map(Entry::sessions)
            .filter(|session| {
                session
                    .agent
                    .is_some_and(|agent| agent.state == AgentState::Waiting)
            })
            .count();
        if waiting > 0 {
            spans.push(Span::styled(
                format!(" · {waiting} waiting"),
                theme::warning(),
            ));
        }
        Line::from(spans)
    }

    pub(super) fn footer_line(&self, width: u16) -> Line<'static> {
        let hints: &[(&str, &str)] = if self.creation.is_some() {
            &[
                ("tab", "next field"),
                ("enter", "create"),
                ("esc", "back"),
                ("ctrl-u", "clear"),
                ("ctrl-c/shift-tab", "back"),
            ]
        } else if self.terminal_focused
            && self
                .active
                .as_ref()
                .is_some_and(|active| active.session.is_none())
        {
            &[
                ("shift-tab", "list · sign-in focused"),
                ("ctrl-c", "cancel sign-in"),
            ]
        } else if self.terminal_focused {
            &[
                ("shift-tab", "list · terminal focused"),
                ("tmux prefix d", "detach"),
            ]
        } else if self.input.is_some() || self.add_input.is_some() {
            if self.input.as_ref().is_some_and(|input| input.connect) {
                &[
                    ("enter", "sign in"),
                    ("ctrl-u", "change username"),
                    ("esc", "back"),
                ]
            } else {
                &[("enter", "save"), ("esc", "cancel")]
            }
        } else if self.filtering {
            &[
                ("type", "to filter"),
                ("↑↓", "move"),
                ("enter", "done"),
                ("esc", "clear"),
            ]
        } else if self
            .current_row()
            .is_some_and(|row| row.kind == RowKind::Computer)
        {
            &[
                ("enter", "connect"),
                ("q", "quit"),
                ("/", "filter"),
                ("a", "add"),
                ("u", "ssh user"),
                ("r", "rescan"),
                ("↑↓", "move"),
                ("n", "new session"),
                ("p", "tint"),
            ]
        } else {
            &[
                (
                    if self.active.is_some() {
                        "enter"
                    } else {
                        "enter/tab"
                    },
                    "open",
                ),
                ("q", "quit"),
                ("x", "end session…"),
                ("/", "filter"),
                ("a", "add"),
                ("f", "pin"),
                ("r", "rescan"),
                ("↑↓", "move"),
                ("n", "new session"),
                ("u", "ssh user"),
                ("p", "tint"),
            ]
        };
        let mut spans = Vec::new();
        let mut used = 0;
        let focus = (self.active.is_some()
            && !self.terminal_focused
            && self.input.is_none()
            && self.creation.is_none()
            && self.add_input.is_none()
            && !self.filtering)
            .then_some((
                "tab",
                if width < 80 {
                    "terminal"
                } else {
                    "terminal · list focused"
                },
            ));
        // Hints are priority ordered. Never leave a partial key or description.
        for (key, description) in focus.iter().chain(hints.iter()) {
            let gap = if spans.is_empty() { 1 } else { 2 };
            let needed = gap + key.width() + 1 + description.width();
            if used + needed > usize::from(width) {
                continue;
            }
            spans.push(Span::raw(if spans.is_empty() { " " } else { "  " }));
            spans.push(Span::styled(*key, self.tint.accent()));
            spans.push(Span::styled(format!(" {description}"), theme::muted()));
            used += needed;
        }
        Line::from(spans)
    }

    fn empty_message(&self) -> String {
        if !self.filter.is_empty() {
            "Nothing matches the filter.".into()
        } else if self.scanning {
            "Discovering computers…".into()
        } else {
            "No computers found.".into()
        }
    }

    fn render_preview(&self, frame: &mut Frame<'_>, area: Rect) {
        let static_seed = match (self.static_until, self.clock) {
            (Some(until), Some(now)) if now < until => Some(self.static_seed),
            _ => None,
        };
        crt::render(frame, area, &self.screen(), Effects { static_seed });
    }

    /// What the preview shows for the current selection.
    pub(super) fn screen(&self) -> Screen {
        let Some(row) = self.current_row() else {
            return Screen {
                label: "standby".into(),
                detail: String::new(),
                lines: vec![Line::styled("Select a session to tune in.", theme::muted())],
                signal: Signal::Standby,
            };
        };
        let entry = &self.entries[row.entry];
        if let Some(session) = self.session(row) {
            let key = (entry.worker.key.clone(), session.id.clone());
            let updating = self
                .preview_inflight
                .as_ref()
                .is_some_and(|(inflight, _)| *inflight == key);
            let cached = self
                .cached_preview(&key, session)
                .map(|entry| &entry.result);
            let (lines, signal) = match cached {
                Some(Ok(text)) => (
                    crate::ansi::styled_lines(text),
                    if updating {
                        Signal::Updating
                    } else {
                        Signal::Live
                    },
                ),
                Some(Err(error)) => (
                    vec![Line::styled(display(error, 400), theme::error())],
                    Signal::Lost,
                ),
                None => (Vec::new(), Signal::Tuning),
            };
            return Screen {
                label: format!(
                    "Preview · Enter to attach · {} / {}",
                    entry.worker.name, session.name
                ),
                detail: session_details(session, (self.services.now)()),
                lines,
                signal,
            };
        }
        self.computer_screen(entry)
    }

    fn computer_screen(&self, entry: &Entry) -> Screen {
        let status = entry.status();
        let label = format!("{} · {status}", entry.worker.name);
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(destination) = &entry.worker.destination {
            lines.push(Line::styled(
                format!("ssh {}", display(destination, 200)),
                theme::muted(),
            ));
            lines.push(Line::raw(""));
        }
        let signal = match status {
            Status::Scanning => Signal::Tuning,
            Status::Ready => {
                lines.push(Line::raw("No tmux sessions on this computer yet."));
                lines.push(Line::styled(
                    "Press Enter or n to create a session here.",
                    theme::muted(),
                ));
                Signal::Ready
            }
            _ => {
                if let Some(inventory) = &entry.inventory {
                    let (messages, banners) = diagnostic_lines(&inventory.detail);
                    lines.extend(
                        messages
                            .into_iter()
                            .map(|line| Line::raw(display(line, 400))),
                    );
                    lines.extend(
                        banners
                            .into_iter()
                            .map(|line| Line::styled(display(line, 400), theme::muted())),
                    );
                }
                let hint = access_hint(status, entry.worker.destination.is_some());
                if !hint.is_empty() {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(hint, theme::muted()));
                }
                Signal::Lost
            }
        };
        Screen {
            label,
            detail: String::new(),
            lines,
            signal,
        }
    }

    /// Session details live in the preview; rows use names and short ages.
    fn render_rows(&self, width: u16, inactive: bool) -> Vec<ListItem<'static>> {
        let content_width = usize::from(width.saturating_sub(4));
        let name_width = content_width.saturating_sub(9);
        let tint = self.tint;
        let (heading, text, secondary) = if inactive {
            let muted = theme::inactive_sidebar();
            (muted, muted, muted)
        } else {
            (tint.heading(), tint.text(), tint.secondary())
        };
        let now = (self.services.now)();
        let details = |session: &Session| format!("{:>4}", short_age(now, session.output));
        // Pad session names within each computer, and pins as their own group.
        let group = |row: Row| match row.kind {
            RowKind::Pinned => usize::MAX,
            _ => row.entry,
        };
        // Pinned rows share the room with their computer, taking at most half.
        let pinned_room = content_width.saturating_sub(11);
        let host_width = self
            .rows
            .iter()
            .filter(|row| row.kind == RowKind::Pinned)
            .map(|row| {
                let host = fit_name(&self.entries[row.entry].worker.name, pinned_room);
                UnicodeWidthStr::width(host.as_str())
            })
            .max()
            .unwrap_or(0)
            .min(pinned_room / 2);
        let pinned_name_width = name_width.min(pinned_room - host_width);
        let name_of = |row: Row| {
            let width = if row.kind == RowKind::Pinned {
                pinned_name_width
            } else {
                name_width
            };
            self.session(row)
                .map(|session| fit_name(&session.name, width))
        };
        let mut widths: HashMap<usize, usize> = HashMap::new();
        for row in self.rows.iter().copied() {
            if let Some(name) = name_of(row) {
                let current = widths.entry(group(row)).or_default();
                *current = (*current).max(UnicodeWidthStr::width(name.as_str()));
            }
        }
        let padded = |row: Row| {
            let name = name_of(row).unwrap_or_default();
            let width = widths.get(&group(row)).copied().unwrap_or(0);
            let padding = width.saturating_sub(UnicodeWidthStr::width(name.as_str()));
            format!("{name}{}", " ".repeat(padding))
        };
        let marker = |session: Option<&Session>| {
            session.map_or_else(
                || Span::raw(" "),
                |session| agent_marker(session, now, tint, inactive),
            )
        };
        self.rows
            .iter()
            .copied()
            .map(|row| {
                let entry = &self.entries[row.entry];
                let spans = match row.kind {
                    RowKind::PinnedHeader => vec![Span::styled("Pinned", heading)],
                    RowKind::Separator => Vec::new(),
                    RowKind::OtherHeader => vec![Span::styled("Other computers", secondary)],
                    RowKind::Group => vec![Span::styled(
                        fit_name(&entry.worker.name, content_width),
                        heading,
                    )],
                    RowKind::Pinned => {
                        let mut spans = vec![
                            marker(self.session(row)),
                            Span::styled("★ ", heading),
                            Span::styled(padded(row), text),
                        ];
                        let detail = self.session(row).map(|session| {
                            Span::styled(format!("  {}", details(session)), secondary)
                        });
                        let host = fit_name(&entry.worker.name, host_width);
                        spans.extend(detail);
                        spans.push(Span::styled(format!("  {host}"), secondary));
                        spans
                    }
                    RowKind::Session => {
                        let session = self.session(row);
                        let pinned = session.is_some_and(|session| {
                            self.state.is_pinned(&entry.worker.key, &session.name)
                        });
                        let mut spans = vec![
                            marker(session),
                            Span::styled(if pinned { "★ " } else { "  " }, heading),
                            Span::styled(padded(row), text),
                        ];
                        if let Some(session) = session {
                            spans.push(Span::styled(format!("  {}", details(session)), secondary));
                        }
                        spans
                    }
                    RowKind::Computer => {
                        let status = entry.status();
                        let offline = entry.worker.online == Some(false);
                        let dot = if offline {
                            "○"
                        } else if status == Status::Scanning {
                            SPINNER[self.frame]
                        } else {
                            "●"
                        };
                        let status_text = if offline {
                            "offline"
                        } else {
                            match status {
                                Status::Scanning => "probing…",
                                Status::Ready => "no sessions",
                                Status::AuthRequired => "Sign in",
                                Status::HostTrust => "Trust key",
                                Status::MissingTmux => "no tmux",
                                Status::MissingTools => "missing tools",
                                Status::ProbeFailed => "probe failed",
                                Status::Unreachable => "unreachable",
                            }
                        };
                        let style = if inactive {
                            theme::inactive_sidebar()
                        } else if offline {
                            theme::muted()
                        } else {
                            theme::status(status)
                        };
                        let name_width =
                            content_width.saturating_sub(UnicodeWidthStr::width(status_text) + 4);
                        vec![
                            Span::styled(fit_name(&entry.worker.name, name_width), heading),
                            Span::styled(format!("  {dot} "), style),
                            Span::styled(status_text, style),
                        ]
                    }
                };
                ListItem::new(Line::from(spans))
            })
            .collect()
    }
}

/// Keep the action or age visible even beside wide Unicode names.
fn fit_name(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut name = display(value, width);
    let mut used = 0;
    let mut end = 0;
    for (index, character) in name.char_indices() {
        used += character.width().unwrap_or(0);
        if used > width {
            name.truncate(end);
            name.push('…');
            break;
        }
        if used < width {
            end = index + character.len_utf8();
        }
    }
    name
}

fn visible_agent(session: &Session, now: i64) -> Option<AgentStatus> {
    session.agent.filter(|agent| {
        agent.state != AgentState::Working
            || now.saturating_sub(agent.since.unwrap_or(0).max(session.output)) <= 600
    })
}

fn agent_marker(session: &Session, now: i64, tint: theme::Tint, inactive: bool) -> Span<'static> {
    let state = visible_agent(session, now).map(|agent| agent.state);
    let style = match state {
        Some(AgentState::Waiting) => theme::warning().add_modifier(Modifier::BOLD),
        Some(AgentState::Error) => theme::error().remove_modifier(Modifier::BOLD | Modifier::DIM),
        _ if inactive => theme::inactive_sidebar().remove_modifier(Modifier::BOLD | Modifier::DIM),
        Some(AgentState::Working) => tint.secondary().remove_modifier(Modifier::BOLD),
        Some(AgentState::Done) => theme::success().remove_modifier(Modifier::BOLD | Modifier::DIM),
        None => tint.secondary(),
    };
    Span::styled(
        match state {
            Some(AgentState::Working | AgentState::Waiting) => "●",
            Some(AgentState::Done) => "✓",
            Some(AgentState::Error) => "✗",
            None => " ",
        },
        style,
    )
}

/// The active pane's command, its directory, activity age, windows and clients.
fn session_details(session: &Session, now: i64) -> String {
    let mut details = Vec::new();
    if let Some(agent) = visible_agent(session, now) {
        let state = match agent.state {
            AgentState::Waiting => "waiting",
            AgentState::Working => "working",
            AgentState::Done => "done",
            AgentState::Error => "error",
        };
        let age = agent
            .since
            .map(|since| format!(" {}", short_age(now, since)))
            .unwrap_or_default();
        details.push(format!("agent {state}{age}"));
    }
    if !session.command.is_empty() {
        details.push(display(&session.command, 40));
    }
    if let Some(directory) = directory_name(&session.path) {
        details.push(display(directory, 60));
    }
    details.push(format!("output {}", age(now, session.output)));
    details.push(format!("used {}", age(now, session.activity)));
    if session.windows > 1 {
        details.push(plural(session.windows, "window"));
    }
    details.push(if session.attached > 0 {
        format!("{} attached", session.attached)
    } else {
        "detached".to_owned()
    });
    details.join(" · ")
}

/// The last component of a pane's working directory.
fn directory_name(path: &str) -> Option<&str> {
    if path.is_empty() {
        return None;
    }
    Some(
        path.rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or("/"),
    )
}

/// A compact relative age, e.g. "now", "45s ago", "12m ago", "3h ago", "2d ago".
fn age(now: i64, then: i64) -> String {
    match short_age(now, then).as_str() {
        "now" => "now".to_owned(),
        short => format!("{short} ago"),
    }
}

/// The shortest relative age, for list rows: "now", "45s", "12m", "3h", "2d".
fn short_age(now: i64, then: i64) -> String {
    let seconds = now.saturating_sub(then).max(0);
    match seconds {
        0..5 => "now".to_owned(),
        5..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", (seconds / 86_400).min(999)),
    }
}

fn access_hint(status: Status, remote: bool) -> &'static str {
    match status {
        Status::AuthRequired if remote => {
            "Wrong user or key? Press u to set the SSH user, or set User/IdentityFile in ~/.ssh/config; enter connects interactively."
        }
        Status::HostTrust if remote => {
            "Press enter to connect interactively and verify the host key."
        }
        _ if remote => "Press enter to try connecting interactively, or r to rescan.",
        _ => "Press r to rescan.",
    }
}

fn plural(count: i64, word: &str) -> String {
    format!("{count} {word}{}", if count == 1 { "" } else { "s" })
}
