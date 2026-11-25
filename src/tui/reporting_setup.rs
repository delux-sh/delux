//! Reporting setup is a modal over the picker, never a terminal launch.
use ratatui::Frame;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use super::{Command, Entry, Picker, attention};
use crate::cancel::Cancel;
use crate::display::display;
use crate::reporting::{Inspection, Response, Target};
use crate::workers::Worker;

pub(super) struct Computer {
    worker: Worker,
    lifetime: Vec<attention::Identity>,
    pending: bool,
    inspection: Option<Inspection>,
    detail: String,
    installed: bool,
}

pub(super) struct Setup {
    epoch: u64,
    generation: u64,
    cancel: Cancel,
    computers: Vec<Computer>,
    cursor: usize,
    list: ListState,
    list_area: Rect,
    scroll: u16,
    confirmation: Option<(usize, Target)>,
    installing: bool,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn lifetime(entry: &Entry) -> Vec<attention::Identity> {
    entry
        .inventory
        .as_ref()
        .map(|inventory| {
            inventory
                .sessions
                .iter()
                .map(|session| attention::Identity::new(&entry.worker, session))
                .collect()
        })
        .unwrap_or_default()
}

impl Setup {
    fn rows(&self) -> impl Iterator<Item = (usize, Option<usize>)> + '_ {
        self.computers
            .iter()
            .enumerate()
            .flat_map(|(index, computer)| {
                let count = computer
                    .inspection
                    .as_ref()
                    .map_or(0, |inspection| inspection.targets.len());
                std::iter::once((index, None))
                    .chain((0..count).map(move |target| (index, Some(target))))
            })
    }

    fn current(&self, entries: &[Entry], generation: u64, index: usize) -> bool {
        let computer = &self.computers[index];
        self.generation == generation
            && entries.iter().any(|entry| {
                let sessions = entry
                    .inventory
                    .as_ref()
                    .map_or(&[][..], |inventory| inventory.sessions.as_slice());
                entry.worker.key == computer.worker.key
                    && entry.worker.destination == computer.worker.destination
                    && sessions.len() == computer.lifetime.len()
                    && sessions
                        .iter()
                        .zip(&computer.lifetime)
                        .all(|(session, identity)| identity.matches(&entry.worker, session))
            })
    }

    fn move_cursor(&mut self, delta: isize) {
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.rows().count().saturating_sub(1));
        self.scroll = 0;
    }

    pub(super) fn mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(2),
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_add(2),
            MouseEventKind::Down(MouseButton::Left)
                if self.confirmation.is_none()
                    && self.list_area.contains((event.column, event.row).into()) =>
            {
                self.cursor = (self.list.offset() + usize::from(event.row - self.list_area.y))
                    .min(self.rows().count().saturating_sub(1));
                self.scroll = 0;
            }
            _ => {}
        }
    }

    pub(super) fn render(&mut self, frame: &mut Frame<'_>, area: Rect, tint: crate::theme::Tint) {
        let title = if self.confirmation.is_some() {
            " Confirm reporting changes "
        } else {
            " Reporting setup · read-only until confirmed "
        };
        let block = Block::bordered().title(title).border_style(tint.border());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let footer_height = 2.min(inner.height);
        let content_height = inner.height.saturating_sub(footer_height);
        let list_height = if self.confirmation.is_some() {
            0
        } else {
            (content_height / 2)
                .min(self.rows().count() as u16)
                .max(1)
                .min(content_height)
        };
        let list_area = Rect::new(inner.x, inner.y, inner.width, list_height);
        self.list_area = list_area;
        let items: Vec<ListItem<'static>> = self
            .rows()
            .map(|(computer, target)| {
                let computer = &self.computers[computer];
                let text = if let Some(index) = target {
                    let target = &computer.inspection.as_ref().unwrap().targets[index];
                    format!(
                        "  {} · {} · {} · {}",
                        display(&target.session_name, 100),
                        display(&target.agent, 40),
                        display(target.profile.as_deref().unwrap_or("default profile"), 80),
                        target.state.label()
                    )
                } else {
                    let capability = computer
                        .inspection
                        .as_ref()
                        .map(|inspection| match inspection.native {
                            Some(true) => "native-capable (per-agent reporting below)",
                            Some(false) => "adapter reporting",
                            None => "native capability unknown",
                        })
                        .unwrap_or(if computer.pending {
                            "checking…"
                        } else if computer.installed {
                            "installed · reporting not confirmed"
                        } else {
                            "unable to check"
                        });
                    format!("{} · {}", display(&computer.worker.name, 100), capability)
                };
                ListItem::new(text)
            })
            .collect();
        self.list.select(Some(self.cursor));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("> ")
                .highlight_style(tint.selected()),
            list_area,
            &mut self.list,
        );
        let mut lines = Vec::<Line<'static>>::new();
        let selection = self
            .confirmation
            .as_ref()
            .map(|(index, target)| (*index, Some(target)))
            .or_else(|| {
                self.rows().nth(self.cursor).map(|(index, target)| {
                    (
                        index,
                        target.map(|target| {
                            &self.computers[index].inspection.as_ref().unwrap().targets[target]
                        }),
                    )
                })
            });
        if let Some((index, target)) = selection {
            let computer = &self.computers[index];
            lines.push(
                format!(
                    "Computer: {} ({})",
                    display(&computer.worker.name, usize::MAX),
                    display(
                        computer.worker.destination.as_deref().unwrap_or("local"),
                        usize::MAX
                    )
                )
                .into(),
            );
            if let Some(target) = target {
                lines.push(
                    format!(
                        "Agent: {} · profile: {} · session: {} ({}) · pane: {}",
                        display(&target.agent, usize::MAX),
                        display(target.profile.as_deref().unwrap_or("default"), usize::MAX),
                        display(&target.session_name, usize::MAX),
                        display(&target.session_id, usize::MAX),
                        display(&target.pane_id, usize::MAX)
                    )
                    .into(),
                );
                lines.push(
                    format!(
                        "{}: {}",
                        target.state.label(),
                        display(&target.detail, usize::MAX)
                    )
                    .into(),
                );
                if !target.files.is_empty() {
                    lines.push(
                        if target.actionable() {
                            "Exact files to write:"
                        } else {
                            "Reporter files:"
                        }
                        .into(),
                    );
                    lines.extend(
                        target
                            .files
                            .iter()
                            .map(|file| Line::from(format!("  {}", display(file, usize::MAX)))),
                    );
                }
                if self.confirmation.is_some() {
                    lines.push(format!("{} only these reporter files? No agent restart or terminal replacement.", target.action.unwrap().label()).into());
                } else if target.actionable() {
                    lines.push("Enter: review installation/update (no changes yet).".into());
                }
            } else if let Some(inspection) = &computer.inspection {
                lines.push(
                    format!(
                        "tmux: {} · {}",
                        display(inspection.tmux_version.as_deref().unwrap_or("unknown"), 100),
                        display(&inspection.detail, usize::MAX)
                    )
                    .into(),
                );
                if inspection.targets.is_empty() {
                    lines.push("No safely identifiable agent targets found. This is not proof that reporting works.".into());
                }
            }
            if !computer.detail.is_empty() {
                lines.extend(
                    computer
                        .detail
                        .lines()
                        .map(|line| display(line, usize::MAX).into()),
                );
            }
        } else {
            lines.push(
                "No computers discovered yet. Close setup and rescan Browse, then return here."
                    .into(),
            );
        }
        if self.installing {
            lines.push("Installing once… Do not restart the agent. Closing cancels the request; its outcome may be uncertain.".into());
        }
        let detail_area = Rect::new(
            inner.x,
            inner.y + list_height,
            inner.width,
            content_height.saturating_sub(list_height),
        );
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            detail_area,
        );
        let help = if self.confirmation.is_some() {
            "y confirm · Esc cancel · PgUp/PgDn scroll exact files"
        } else if self.installing {
            "Esc close (outcome may be uncertain) · PgUp/PgDn scroll"
        } else {
            "↑/↓ choose · Enter review · r recheck · Esc close\nPgUp/PgDn scroll details · installation is not proof of reporting"
        };
        frame.render_widget(
            Paragraph::new(help).style(tint.secondary()),
            Rect::new(
                inner.x,
                inner.y + content_height,
                inner.width,
                footer_height,
            ),
        );
    }
}

impl Picker {
    pub(super) fn start_reporting_setup(&mut self) -> Command {
        self.setup_epoch += 1;
        let cancel = self.cancel.child();
        let workers: Vec<Worker> = self
            .entries
            .iter()
            .map(|entry| entry.worker.clone())
            .collect();
        let computers = self
            .entries
            .iter()
            .map(|entry| Computer {
                worker: entry.worker.clone(),
                lifetime: lifetime(entry),
                pending: true,
                inspection: None,
                detail: String::new(),
                installed: false,
            })
            .collect();
        self.reporting_setup = Some(Setup {
            epoch: self.setup_epoch,
            generation: self.generation,
            cancel: cancel.clone(),
            computers,
            cursor: 0,
            list: ListState::default(),
            list_area: Rect::default(),
            scroll: 0,
            confirmation: None,
            installing: false,
        });
        Command::ReportingSetup {
            epoch: self.setup_epoch,
            cancel,
            workers,
            token: None,
        }
    }

    pub(super) fn reporting_key(&mut self, key: KeyEvent) -> Option<Command> {
        let setup = self.reporting_setup.as_mut()?;
        if key.code == KeyCode::Esc {
            if setup.confirmation.take().is_none() {
                self.reporting_setup = None;
            }
            return None;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::PageUp => setup.scroll = setup.scroll.saturating_sub(5),
            KeyCode::PageDown => setup.scroll = setup.scroll.saturating_add(5),
            _ if setup.installing => {}
            KeyCode::Char('y') if setup.confirmation.is_some() => {
                let (index, target) = setup.confirmation.take()?;
                if !setup.current(&self.entries, self.generation, index) {
                    setup.computers[index].detail = "Computer/session lifetime changed. Press r to inspect again; nothing was installed.".into();
                    setup.computers[index].inspection = None;
                    let cursor = setup
                        .rows()
                        .position(|row| row == (index, None))
                        .unwrap_or(0);
                    setup.cursor = cursor;
                    setup.scroll = 0;
                    return None;
                }
                let current = setup.computers[index]
                    .inspection
                    .as_ref()?
                    .targets
                    .iter()
                    .find(|candidate| candidate.id == target.id)?;
                if !current.actionable()
                    || current.token != target.token
                    || current.files != target.files
                {
                    return None;
                }
                let token = target.token?;
                setup.cancel.cancel();
                setup.cancel = self.cancel.child();
                self.setup_epoch += 1;
                setup.epoch = self.setup_epoch;
                setup.installing = true;
                setup.computers[index].pending = true;
                setup.computers[index].detail = "Installing once; outcome pending.".into();
                // Consume every token for this computer immediately. A failed or
                // uncertain attempt always requires a fresh read-only inspection.
                if let Some(inspection) = &mut setup.computers[index].inspection {
                    for target in &mut inspection.targets {
                        target.token = None;
                        target.action = None;
                    }
                }
                return Some(Command::ReportingSetup {
                    epoch: setup.epoch,
                    cancel: setup.cancel.clone(),
                    workers: vec![setup.computers[index].worker.clone()],
                    token: Some(token),
                });
            }
            _ if setup.confirmation.is_some() => {}
            KeyCode::Char('r') => return Some(self.start_reporting_setup()),
            KeyCode::Up | KeyCode::Char('k') => setup.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => setup.move_cursor(1),
            KeyCode::Enter => {
                if setup.computers.iter().any(|computer| computer.pending) {
                    return None;
                }
                let (index, target_index) = setup.rows().nth(setup.cursor)?;
                if !setup.current(&self.entries, self.generation, index) {
                    setup.computers[index].detail =
                        "Computer/session lifetime changed. Press r to inspect again.".into();
                    setup.computers[index].inspection = None;
                    let cursor = setup
                        .rows()
                        .position(|row| row == (index, None))
                        .unwrap_or(0);
                    setup.cursor = cursor;
                    setup.scroll = 0;
                    return None;
                }
                let target = setup.computers[index]
                    .inspection
                    .as_ref()?
                    .targets
                    .get(target_index?)?;
                if target.actionable() {
                    setup.confirmation = Some((index, target.clone()));
                    setup.scroll = 0;
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn reporting_result(
        &mut self,
        epoch: u64,
        worker: Worker,
        result: Result<Response, String>,
    ) {
        let Some(setup) = &mut self.reporting_setup else {
            return;
        };
        if setup.epoch != epoch || setup.cancel.is_cancelled() {
            return;
        }
        let Some(index) = setup.computers.iter().position(|computer| {
            computer.worker.key == worker.key && computer.worker.destination == worker.destination
        }) else {
            return;
        };
        if !setup.computers[index].pending {
            return;
        }
        let selected = setup.rows().nth(setup.cursor);
        let current = setup.current(&self.entries, self.generation, index);
        let computer = &mut setup.computers[index];
        computer.pending = false;
        if !current {
            computer.inspection = None;
            computer.detail = if setup.installing { "Computer/session changed while installing; outcome uncertain. Recheck before another attempt." } else { "Computer/session changed during inspection. Press r to recheck." }.into();
        } else {
            match result {
                Ok(Response::Inspected(inspection)) if !setup.installing => {
                    computer.inspection = Some(inspection);
                    computer.detail.clear();
                }
                Ok(Response::Installed(installed)) if setup.installing => {
                    computer.installed = true;
                    computer.inspection = None;
                    computer.detail = format!(
                        "{}\nFiles written: {}\nRestart/resume the agent at a safe point yourself. Reporting is not yet confirmed; recheck and observe actual Attention status.",
                        installed.detail,
                        installed.files.join(", ")
                    );
                }
                Err(error) => {
                    computer.inspection = None;
                    computer.detail = if setup.installing {
                        format!(
                            "Installation outcome uncertain: {error}. Do not retry without a fresh r recheck. No automatic restart."
                        )
                    } else {
                        format!(
                            "Unable to check: {error}. This does not mean a reporter is missing. Python 3 is required for optional setup."
                        )
                    };
                }
                _ => {
                    computer.inspection = None;
                    computer.detail = "Unexpected response; outcome unknown. Press r to recheck before any installation.".into();
                }
            }
        }
        setup.installing = false;
        setup.confirmation = None;
        let cursor = selected
            .and_then(|selected| setup.rows().position(|row| row == selected))
            .or_else(|| {
                selected.and_then(|(index, _)| setup.rows().position(|row| row == (index, None)))
            })
            .unwrap_or(0);
        setup.cursor = cursor;
        setup.scroll = 0;
    }
}
