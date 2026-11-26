use std::borrow::Cow;

use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Block, BorderType, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    directory_completion::DirectoryCompletion,
    display::display,
    launcher::{InstalledLaunchers, LaunchMode, Launcher},
    theme::{self, Tint},
    tmux::NewSession,
    workers::Worker,
};

pub enum FormAction {
    Submit(NewSession),
    Cancel,
    /// Look up the Directory input on the form's computer; the caller answers
    /// through `SessionForm::complete_directory` with the same string.
    CompleteDirectory(String),
}

/// Shown match names are capped so a wide directory never floods the form.
const LISTED_MATCHES: usize = 24;

/// Stable inline option order. Named tools appear when discovery finds them.
const MODES: [LaunchMode; 6] = [
    LaunchMode::Shell,
    LaunchMode::Omp,
    LaunchMode::ResumeOmp,
    LaunchMode::Claude,
    LaunchMode::Codex,
    LaunchMode::Custom,
];

fn mode_label(mode: LaunchMode) -> &'static str {
    match mode {
        LaunchMode::Shell => "Shell",
        LaunchMode::Omp => "OMP",
        LaunchMode::ResumeOmp => "Resume OMP…",
        LaunchMode::Claude => "Claude",
        LaunchMode::Codex => "Codex",
        LaunchMode::Custom => "Custom command",
    }
}

/// The tool a Start choice depends on, if it depends on one being installed.
fn tool_name(mode: LaunchMode) -> Option<&'static str> {
    match mode {
        LaunchMode::Shell | LaunchMode::Custom => None,
        LaunchMode::Omp | LaunchMode::ResumeOmp => Some("OMP"),
        LaunchMode::Claude => Some("Claude"),
        LaunchMode::Codex => Some("Codex"),
    }
}

/// What is known about the tools installed on the form's computer.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Availability {
    Checking,
    Failed(String),
    Known(InstalledLaunchers),
}

/// The controls of the form. Which ones are shown, and in what order, follows
/// the Start choice; see `SessionForm::fields`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Name,
    Directory,
    Start,
    Profile,
    Command,
    SaveDefault,
    Create,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Directory => "Directory",
            Self::Start => "Start",
            Self::Profile => "OMP profile (optional)",
            Self::Command => "Custom command",
            Self::SaveDefault | Self::Create => "",
        }
    }

    /// The checkbox carries its label; Create uses its label row as a spacer.
    /// Compact layouts collapse both labels and the spacer to preserve focus.
    fn rows(self) -> u16 {
        if self == Self::SaveDefault { 1 } else { 2 }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Name => "Name: the tmux session name; Enter moves to Directory.",
            Self::Directory => {
                "Directory: ~, ~/path, or an absolute path on that computer; Tab completes."
            }
            Self::Start => "Start: ↑/↓ moves; Space selects; Enter selects and advances.",
            Self::Profile => {
                "Profile: a named OMP profile; blank uses OMP's default profile or environment."
            }
            Self::Command => {
                "Command: runs exactly as typed in the shell there; when it ends, the session may end."
            }
            Self::SaveDefault => {
                "Space toggles. Saved once this session is created; later new sessions start this way."
            }
            Self::Create => {
                "Enter or Space creates and attaches. ↑ returns to settings; Esc cancels."
            }
        }
    }
}

const NAVIGATION_HINT: &str =
    "↑/↓ move between fields · Enter advances / activates · Create session when ready.";

/// The lifecycle of one Tab completion. Any edit of the Directory input
/// discards it, so a reply only ever applies to the exact text it was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Completion {
    Idle,
    Pending(String),
    Failed(String),
    Done(DirectoryCompletion),
}

pub struct SessionForm {
    pub worker: Worker,
    pub busy: bool,
    pub error: String,
    name: String,
    directory: String,
    /// Profile and command drafts survive changing Start. Submission uses only
    /// fields for the selected mode; saving discards the rest.
    launcher: Launcher,
    save_default: bool,
    field: Field,
    /// Focused option, independent of the selected launcher.
    start_choice: LaunchMode,
    completion: Completion,
    /// What the caller learned about the tools installed there; the form
    /// opens before the answer, so Start fills in as it arrives.
    available: Availability,
}

impl SessionForm {
    /// `launcher` seeds the Start choice (the saved default, or Shell); the
    /// Use-as-default checkbox always starts unchecked.
    pub fn new(worker: Worker, directory: String, launcher: Launcher) -> Self {
        Self {
            worker,
            busy: false,
            error: String::new(),
            name: String::new(),
            directory: if directory.is_empty() {
                "~".into()
            } else {
                directory
            },
            start_choice: launcher.mode,
            launcher,
            save_default: false,
            field: Field::Name,
            completion: Completion::Idle,
            available: Availability::Checking,
        }
    }

    /// The controls in traversal order for the current Start choice. Start can
    /// only change while it is focused, so the focused field is always listed.
    fn fields(&self) -> &'static [Field] {
        match self.launcher.mode {
            LaunchMode::Shell | LaunchMode::Claude | LaunchMode::Codex => &[
                Field::Name,
                Field::Directory,
                Field::Start,
                Field::SaveDefault,
                Field::Create,
            ],
            LaunchMode::Omp | LaunchMode::ResumeOmp => &[
                Field::Name,
                Field::Directory,
                Field::Start,
                Field::Profile,
                Field::SaveDefault,
                Field::Create,
            ],
            LaunchMode::Custom => &[
                Field::Name,
                Field::Directory,
                Field::Start,
                Field::Command,
                Field::SaveDefault,
                Field::Create,
            ],
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
            if let Some((value, _)) = self.text_mut() {
                value.clear();
                self.edited();
            }
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
            KeyCode::Enter | KeyCode::Char(' ') if self.field == Field::Start => {
                if self.note_for(self.start_choice).is_none() {
                    self.launcher.mode = self.start_choice;
                    self.error.clear();
                    if key.code == KeyCode::Enter {
                        self.step(true);
                    }
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.field == Field::Create => {
                if let Some(refusal) = self.unavailable() {
                    self.error = refusal;
                    return None;
                }
                let session = self.launcher.command_line().map(|command| NewSession {
                    name: self.name.clone(),
                    directory: self.directory.clone(),
                    command,
                });
                match session.and_then(|session| session.validate().map(|()| session)) {
                    Ok(session) => {
                        self.error.clear();
                        self.busy = true;
                        // A reply landing after submission must not rewrite
                        // the directory the request already captured.
                        self.completion = Completion::Idle;
                        return Some(FormAction::Submit(session));
                    }
                    Err(error) => self.error = error,
                }
            }
            KeyCode::Tab if self.field == Field::Directory => {
                let input = self.directory.clone();
                self.error.clear();
                self.completion = Completion::Pending(input.clone());
                return Some(FormAction::CompleteDirectory(input));
            }
            KeyCode::Down if self.field == Field::Start => self.move_choice(true),
            KeyCode::Up if self.field == Field::Start => self.move_choice(false),
            KeyCode::Enter | KeyCode::Tab | KeyCode::Down => self.step(true),
            KeyCode::Up => self.step(false),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.field == Field::SaveDefault =>
            {
                self.save_default = !self.save_default;
            }
            KeyCode::Backspace => {
                if let Some((value, _)) = self.text_mut() {
                    value.pop();
                    self.edited();
                }
            }
            KeyCode::Char(character) => self.paste(character.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        None
    }

    /// Move focus to the next or previous control, wrapping at either end.
    fn step(&mut self, forward: bool) {
        let fields = self.fields();
        let index = fields
            .iter()
            .position(|field| *field == self.field)
            .expect("the focused field is always listed");
        let next = if forward {
            (index + 1) % fields.len()
        } else {
            (index + fields.len() - 1) % fields.len()
        };
        self.field = fields[next];
        if self.field == Field::Start {
            self.start_choice = self.launcher.mode;
        }
    }

    fn move_choice(&mut self, forward: bool) {
        let current = self.start_choice;
        let index = MODES
            .iter()
            .position(|mode| *mode == current)
            .expect("every launcher has a selector entry");
        let next = if forward {
            MODES[index + 1..].iter().find(|mode| self.offered(**mode))
        } else {
            MODES[..index]
                .iter()
                .rev()
                .find(|mode| self.offered(**mode))
        };
        if let Some(mode) = next {
            self.start_choice = *mode;
        } else {
            self.step(forward);
        }
    }

    /// Text typed or pasted goes into the focused input; the Start choice and
    /// the checkbox never react to it.
    pub fn paste(&mut self, text: &str) {
        if self.busy {
            return;
        }
        let Some((value, limit)) = self.text_mut() else {
            return;
        };
        let remaining = limit.saturating_sub(value.chars().count());
        value.extend(text.chars().filter(|c| !c.is_control()).take(remaining));
        self.edited();
    }

    /// The focused text input and its length limit, if a text input is focused.
    fn text_mut(&mut self) -> Option<(&mut String, usize)> {
        Some(match self.field {
            Field::Name => (&mut self.name, 64),
            Field::Directory => (&mut self.directory, 4096),
            Field::Profile => (&mut self.launcher.profile, 64),
            Field::Command => (&mut self.launcher.command, 4096),
            Field::Start | Field::SaveDefault | Field::Create => return None,
        })
    }

    fn text(&self, field: Field) -> &str {
        match field {
            Field::Name => &self.name,
            Field::Directory => &self.directory,
            Field::Profile => &self.launcher.profile,
            Field::Command => &self.launcher.command,
            Field::Start | Field::SaveDefault | Field::Create => "",
        }
    }

    fn edited(&mut self) {
        self.error.clear();
        if self.field == Field::Directory {
            self.completion = Completion::Idle;
        }
    }

    /// The Start choice to store as the default for later sessions: only when
    /// the user checked Use as default, and only if it is valid. The caller
    /// consults this once the session has actually been created.
    pub fn launcher_to_save(&self) -> Option<Launcher> {
        let launcher = self.save_default.then(|| self.launcher.normalized())?;
        launcher.validate().is_ok().then_some(launcher)
    }

    /// Whether a Tab lookup is still awaited; the caller may cancel it once
    /// this turns false.
    pub fn directory_completion_pending(&self) -> bool {
        matches!(self.completion, Completion::Pending(_))
    }

    /// Apply a lookup reply. It is ignored unless `requested` is exactly the
    /// text still awaiting an answer, so stale or superseded replies never
    /// touch an input the user has edited since.
    pub fn complete_directory(
        &mut self,
        requested: &str,
        result: Result<DirectoryCompletion, String>,
    ) {
        if !matches!(&self.completion, Completion::Pending(input) if input == requested) {
            return;
        }
        self.completion = match result {
            Err(error) => Completion::Failed(error),
            Ok(completion) => {
                // A partial list cannot prove a common prefix, so only a full
                // one may extend the input; a lone full match is complete, and
                // an existing directory with nothing below it takes its
                // normalized form (~ becomes ~/).
                let fill = if completion.truncated {
                    ""
                } else if completion.matches.is_empty() {
                    if completion.exists {
                        completion.path.as_str()
                    } else {
                        ""
                    }
                } else {
                    common_prefix(&completion.matches)
                };
                if !fill.is_empty() && fill != self.directory {
                    self.directory = fill.to_owned();
                }
                Completion::Done(completion)
            }
        };
    }

    fn completion_status(&self, tint: Tint) -> Option<(String, Style)> {
        Some(match &self.completion {
            Completion::Idle => return None,
            Completion::Pending(_) => ("Checking directory…".to_owned(), tint.secondary()),
            Completion::Failed(error) => (
                format!("Directory check failed: {}", display(error, 4096)),
                theme::error(),
            ),
            Completion::Done(completion) => {
                match (
                    completion.exists,
                    completion.matches.len(),
                    completion.truncated,
                ) {
                    (_, 1, false) => ("Directory exists.".to_owned(), theme::success()),
                    (true, 0, _) => (
                        "Directory exists; nothing more to complete.".to_owned(),
                        theme::success(),
                    ),
                    (false, 0, _) => ("No matching directory.".to_owned(), theme::warning()),
                    (_, count, false) => (
                        format!("{count} matches; type more, then Tab."),
                        tint.text(),
                    ),
                    (_, count, true) => (
                        format!("Over {count} matches; type more, then Tab."),
                        tint.text(),
                    ),
                }
            }
        })
    }

    /// The match names (not full paths) to list immediately below the input.
    fn listed_matches(&self) -> Option<String> {
        let Completion::Done(completion) = &self.completion else {
            return None;
        };
        if completion.matches.len() < 2 && !completion.truncated {
            return None;
        }
        let mut listed = String::new();
        for path in completion.matches.iter().take(LISTED_MATCHES) {
            if !listed.is_empty() {
                listed.push_str("  ");
            }
            let name = path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default();
            listed.push_str(&display(name, 80));
            listed.push('/');
        }
        if completion.truncated || completion.matches.len() > LISTED_MATCHES {
            listed.push_str("  …");
        }
        Some(listed)
    }

    /// Whether `mode` is offered in Start: Shell and Custom command always,
    /// a named tool once it is known to be installed there, and the chosen
    /// one regardless, so a saved default is never silently replaced.
    fn offered(&self, mode: LaunchMode) -> bool {
        mode == self.launcher.mode
            || (self.field == Field::Start && self.start_choice == mode)
            || tool_name(mode).is_none()
            || matches!(&self.available, Availability::Known(installed) if installed.supports(mode))
    }

    /// Availability note for a selected or highlighted launcher.
    fn note_for(&self, mode: LaunchMode) -> Option<&'static str> {
        tool_name(mode)?;
        match &self.available {
            Availability::Checking => Some("checking…"),
            Availability::Failed(_) => Some("check failed"),
            Availability::Known(installed) if !installed.supports(mode) => Some("not installed"),
            Availability::Known(_) => None,
        }
    }

    /// Why the chosen Start cannot be submitted: it names a tool whose
    /// presence there is still unknown, could not be checked, or is ruled out.
    fn unavailable(&self) -> Option<String> {
        let tool = tool_name(self.launcher.mode)?;
        Some(match &self.available {
            Availability::Checking => format!(
                "Still checking whether {tool} is installed there; wait a moment, or choose Shell or Custom command."
            ),
            Availability::Failed(error) => format!(
                "Could not check whether {tool} is installed there: {}",
                display(error, 4096)
            ),
            Availability::Known(installed) if !installed.supports(self.launcher.mode) => {
                format!("{tool} is not installed there; choose another launcher.")
            }
            Availability::Known(_) => return None,
        })
    }

    pub fn needs_launcher_lookup(&self) -> bool {
        !self.busy && matches!(self.available, Availability::Checking)
    }

    /// Apply the one availability reply for this form. A form already
    /// submitted keeps what it showed; the caller has cancelled the lookup.
    /// A refusal from Enter pressed meanwhile is dropped once outdated, while
    /// name or directory errors stay.
    pub fn set_available_launchers(&mut self, result: Result<InstalledLaunchers, String>) {
        if self.busy {
            return;
        }
        let outdated = self
            .unavailable()
            .is_some_and(|refusal| refusal == self.error);
        self.available = match result {
            Ok(installed) => Availability::Known(installed),
            Err(error) => Availability::Failed(error),
        };
        if outdated {
            self.error.clear();
        }
    }

    /// The lowest-ranked status row: what the tool check is doing, or why
    /// the chosen Start is not startable.
    fn availability_status(&self, tint: Tint) -> Option<(String, Style)> {
        Some(match &self.available {
            Availability::Checking => (
                "Checking which tools are installed there; OMP, Claude and Codex join Start once found."
                    .to_owned(),
                tint.secondary(),
            ),
            Availability::Failed(error) => (
                format!(
                    "Tool check failed: {}. Only Shell and Custom command can start.",
                    display(error, 4096)
                ),
                theme::error(),
            ),
            Availability::Known(_) => (self.unavailable()?, theme::warning()),
        })
    }

    /// What the chosen Start actually runs in the new terminal.
    fn starts_hint(&self) -> Cow<'static, str> {
        let profile = &self.launcher.profile;
        match self.launcher.mode {
            LaunchMode::Shell => "Starts: the normal shell on that computer.".into(),
            LaunchMode::Omp if profile.is_empty() => {
                "Starts: omp with OMP's default profile or environment.".into()
            }
            LaunchMode::Omp => format!("Starts: omp --profile={}", display(profile, 64)).into(),
            LaunchMode::ResumeOmp => {
                let mut text = String::from("Starts: omp --resume");
                if !profile.is_empty() {
                    text.push_str(" --profile=");
                    text.push_str(&display(profile, 64));
                }
                text.push_str("; OMP's session picker opens inside the new terminal there.");
                text.into()
            }
            LaunchMode::Claude => {
                "Starts: claude; Claude Code opens in the new terminal there.".into()
            }
            LaunchMode::Codex => "Starts: codex; Codex opens in the new terminal there.".into(),
            LaunchMode::Custom => {
                "Starts: the custom command in the shell there; when it ends, the session may end."
                    .into()
            }
        }
    }

    fn render_choices(&self, frame: &mut Frame, area: Rect, tint: Tint) {
        let focused = self.field == Field::Start;
        let highlighted = if focused {
            self.start_choice
        } else {
            self.launcher.mode
        };
        let mut y = area.y;
        let mut rows = area.height;
        if rows > 1 {
            frame.render_widget(
                Paragraph::new("Start").style(if focused {
                    tint.heading()
                } else {
                    tint.secondary()
                }),
                Rect::new(area.x, y, area.width, 1),
            );
            y += 1;
            rows -= 1;
        }
        let selected = MODES
            .iter()
            .filter(|mode| self.offered(**mode))
            .position(|mode| *mode == highlighted)
            .expect("focused choice remains offered");
        let first = selected.saturating_sub(usize::from(rows).saturating_sub(1));
        for (row, mode) in MODES
            .iter()
            .copied()
            .filter(|mode| self.offered(*mode))
            .skip(first)
            .take(usize::from(rows))
            .enumerate()
        {
            let pointer = if focused && mode == highlighted {
                "›"
            } else {
                " "
            };
            let mark = if mode == self.launcher.mode {
                "•"
            } else {
                " "
            };
            let note = self.note_for(mode);
            let text = match note {
                Some(note) => format!("{pointer} ({mark}) {} ({note})", mode_label(mode)),
                None => format!("{pointer} ({mark}) {}", mode_label(mode)),
            };
            let style = if focused && mode == highlighted {
                tint.selected()
            } else if note.is_some() {
                tint.secondary()
            } else {
                tint.text()
            };
            frame.render_widget(
                Paragraph::new(text).style(style),
                Rect::new(area.x, y + row as u16, area.width, 1),
            );
        }
    }

    /// One control: its label when `area` has two rows, then its row.
    fn render_field(&self, frame: &mut Frame, field: Field, area: Rect, tint: Tint) {
        if field == Field::Start {
            self.render_choices(frame, area, tint);
            return;
        }
        let focused = field == self.field;
        let mut y = area.y;
        if area.height > 1 {
            frame.render_widget(
                Paragraph::new(field.label()).style(if focused {
                    tint.heading()
                } else {
                    tint.secondary()
                }),
                Rect::new(area.x, y, area.width, 1),
            );
            y += 1;
        }
        let row = Rect::new(area.x, y, area.width, 1);
        let style = if focused {
            tint.selected()
        } else {
            tint.text()
        };
        match field {
            Field::Start => unreachable!("Start renders its inline choices above"),
            Field::SaveDefault => frame.render_widget(
                Paragraph::new(if self.save_default {
                    "[x] Use as default for new sessions"
                } else {
                    "[ ] Use as default for new sessions"
                })
                .style(style),
                row,
            ),
            Field::Create => frame.render_widget(
                Paragraph::new(if self.busy {
                    "[ Creating… ]"
                } else {
                    "[ Create session ]"
                })
                .style(style),
                row,
            ),
            Field::Name | Field::Directory | Field::Profile | Field::Command => {
                let rendered = display(self.text(field), 4096);
                let visible = if focused {
                    suffix(&rendered, usize::from(area.width.saturating_sub(1)))
                } else {
                    &rendered
                };
                frame.render_widget(Paragraph::new(Line::raw(visible)).style(style), row);
                if focused && !self.busy {
                    let offset =
                        UnicodeWidthStr::width(visible).min(usize::from(area.width - 1)) as u16;
                    frame.set_cursor_position((area.x + offset, y));
                }
            }
        }
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

        // Directory feedback belongs to its input, never below other settings.
        // Preserve focus and global errors first on short terminals.
        let directory_status = if self.error.is_empty() && !self.busy {
            self.completion_status(tint)
        } else {
            None
        }
        .map(|(text, style)| Paragraph::new(text).style(style).wrap(Wrap { trim: false }));
        let matches = directory_status
            .as_ref()
            .and_then(|_| self.listed_matches())
            .map(|text| {
                Paragraph::new(text)
                    .style(tint.text())
                    .wrap(Wrap { trim: false })
            });
        let status = if !self.error.is_empty() {
            Some((display(&self.error, 4096), theme::error()))
        } else if self.busy {
            Some(("Creating… Esc cancels".to_owned(), tint.secondary()))
        } else if directory_status.is_none() {
            self.availability_status(tint)
        } else {
            None
        }
        .map(|(text, style)| Paragraph::new(text).style(style).wrap(Wrap { trim: false }));
        let status_rows = status
            .as_ref()
            .map_or(0, |paragraph| {
                paragraph.line_count(inner.width).clamp(1, 3) as u16
            })
            .min(inner.height.saturating_sub(1));
        let available = inner.height - status_rows;
        let directory_status_rows = directory_status
            .as_ref()
            .map_or(0, |paragraph| {
                paragraph.line_count(inner.width).clamp(1, 3) as u16
            })
            .min(available.saturating_sub(1));
        let compact = available < 2
            || (self.field == Field::Directory && available - directory_status_rows < 2);
        let start_rows = 1 + MODES.iter().filter(|mode| self.offered(**mode)).count() as u16;
        let control_height = |field: Field| {
            if field == Field::Start {
                start_rows.min(available)
            } else if compact {
                1
            } else {
                field.rows()
            }
        };
        let match_lines = matches
            .as_ref()
            .map_or(0, |paragraph| paragraph.line_count(inner.width));
        let match_rows = (match_lines.min(3) as u16).min(
            available.saturating_sub(control_height(Field::Directory) + directory_status_rows),
        );
        let height_of = |field: Field| {
            control_height(field)
                + if field == Field::Directory {
                    match_rows + directory_status_rows
                } else {
                    0
                }
        };
        let fields = self.fields();
        let focused = fields
            .iter()
            .position(|field| *field == self.field)
            .unwrap_or(0);
        let mut first = focused;
        let mut used = height_of(fields[focused]);
        let mut visible_start_rows = control_height(Field::Start);
        while first > 0 {
            let previous = fields[first - 1];
            if used + height_of(previous) <= available {
                first -= 1;
                used += height_of(previous);
            } else {
                // Keep part of the option list above later settings instead
                // of dropping the entire group when only a few rows are short.
                if previous == Field::Start && available - used >= 2 {
                    first -= 1;
                    visible_start_rows = available - used;
                }
                break;
            }
        }
        let mut y = inner.y;
        for &field in &fields[first..] {
            let height = if field == Field::Start {
                visible_start_rows.min(inner.y + available - y)
            } else {
                height_of(field)
            };
            if height == 0 || y + height > inner.y + available {
                break;
            }
            let control_rows = if field == Field::Start {
                height
            } else {
                control_height(field)
            };
            self.render_field(
                frame,
                field,
                Rect::new(inner.x, y, inner.width, control_rows),
                tint,
            );
            y += control_rows;
            if field == Field::Directory {
                if let Some(listed) = &matches
                    && match_rows != 0
                {
                    // Count with the same word wrapper used to render, then
                    // reserve the last allotted row when any results are hidden.
                    let clipped = match_lines > usize::from(match_rows);
                    let listed_rows = match_rows - u16::from(clipped);
                    if listed_rows != 0 {
                        frame
                            .render_widget(listed, Rect::new(inner.x, y, inner.width, listed_rows));
                    }
                    if clipped {
                        frame.render_widget(
                            Paragraph::new("… more results").style(tint.secondary()),
                            Rect::new(inner.x, y + listed_rows, inner.width, 1),
                        );
                    }
                    y += match_rows;
                }
                if let Some(paragraph) = &directory_status
                    && directory_status_rows != 0
                {
                    frame.render_widget(
                        paragraph,
                        Rect::new(inner.x, y, inner.width, directory_status_rows),
                    );
                    y += directory_status_rows;
                }
            }
        }
        if let Some(paragraph) = status
            && status_rows != 0
        {
            frame.render_widget(paragraph, Rect::new(inner.x, y, inner.width, status_rows));
            y += status_rows;
        }
        let bottom = inner.y + inner.height;
        if y < bottom {
            let starts = self.starts_hint();
            for hint in [self.field.hint(), &*starts, NAVIGATION_HINT] {
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
}

/// The longest prefix shared by every match, cut on character boundaries.
fn common_prefix(matches: &[String]) -> &str {
    let Some(first) = matches.first() else {
        return "";
    };
    let mut end = first.len();
    for other in &matches[1..] {
        end = first[..end]
            .char_indices()
            .zip(other.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map_or(0, |((index, a), _)| index + a.len_utf8());
    }
    &first[..end]
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
    use ratatui::{Terminal, backend::TestBackend, layout::Position};

    use super::*;

    fn worker() -> Worker {
        Worker {
            key: "local".into(),
            name: "Local".into(),
            destination: None,
            online: Some(true),
        }
    }

    fn installed(omp: bool, claude: bool, codex: bool) -> InstalledLaunchers {
        InstalledLaunchers { omp, claude, codex }
    }

    /// A form whose computer's tools are already known.
    fn form_with(installed: InstalledLaunchers, launcher: Launcher) -> SessionForm {
        let mut form = SessionForm::new(worker(), "~".into(), launcher);
        form.set_available_launchers(Ok(installed));
        form
    }

    /// A form on a computer with only OMP installed, where Start offers what
    /// it did before Claude and Codex joined.
    fn form() -> SessionForm {
        form_with(installed(true, false, false), Launcher::default())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Move focus down until `field` is focused, failing if it is not listed.
    fn focus(form: &mut SessionForm, field: Field) {
        for _ in 0..MODES.len() + 6 {
            if form.field == field {
                return;
            }
            form.key(key(KeyCode::Down));
        }
        panic!("{field:?} is not reachable among {:?}", form.fields());
    }

    fn choose(form: &mut SessionForm, mode: LaunchMode) {
        focus(form, Field::Start);
        for _ in 0..MODES.len() {
            if form.start_choice == LaunchMode::Shell {
                break;
            }
            form.key(key(KeyCode::Up));
        }
        for _ in 0..MODES.len() {
            if form.start_choice == mode {
                break;
            }
            form.key(key(KeyCode::Down));
        }
        assert_eq!(form.start_choice, mode, "requested choice must be offered");
        assert!(form.key(key(KeyCode::Char(' '))).is_none());
        assert_eq!(form.launcher.mode, mode);
    }

    fn submit(form: &mut SessionForm) -> Result<NewSession, String> {
        focus(form, Field::Create);
        match form.key(key(KeyCode::Enter)) {
            Some(FormAction::Submit(session)) => Ok(session),
            None => Err(form.error.clone()),
            Some(_) => panic!("Enter never cancels or completes"),
        }
    }

    /// A form focused on Directory holding `directory`, with Tab pressed.
    fn requested(directory: &str) -> SessionForm {
        let mut form = form();
        form.key(key(KeyCode::Down));
        form.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        form.paste(directory);
        match form.key(key(KeyCode::Tab)) {
            Some(FormAction::CompleteDirectory(input)) => assert_eq!(input, directory),
            _ => panic!("Tab in Directory must request completion"),
        }
        assert!(form.directory_completion_pending());
        assert_eq!(form.field, Field::Directory);
        form
    }

    fn found(path: &str, exists: bool, matches: &[&str], truncated: bool) -> DirectoryCompletion {
        DirectoryCompletion {
            path: path.to_owned(),
            exists,
            matches: matches.iter().map(|m| (*m).to_owned()).collect(),
            truncated,
        }
    }

    fn rendered(form: &SessionForm, width: u16, height: u16) -> (String, Position) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| form.render(frame, frame.area(), Tint::Cyan))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, cursor)
    }

    #[test]
    fn shell_default_submits_an_empty_command_and_saves_nothing() {
        let mut form = form();
        form.paste("work");
        let session = submit(&mut form).unwrap();
        assert_eq!(session.name, "work");
        assert_eq!(session.directory, "~");
        assert_eq!(session.command, "");
        assert_eq!(form.launcher_to_save(), None);
    }

    #[test]
    fn inline_choices_require_selection_and_enter_only_advances() {
        let mut form = form();
        form.paste("work");
        focus(&mut form, Field::Start);
        form.key(key(KeyCode::Down));
        assert_eq!(form.start_choice, LaunchMode::Omp);
        assert_eq!(form.launcher.mode, LaunchMode::Shell);
        form.paste("ignored");
        assert_eq!(form.name, "work");
        assert!(form.key(key(KeyCode::Char(' '))).is_none());
        assert_eq!(form.launcher.mode, LaunchMode::Omp);
        assert!(!form.busy);
        assert_eq!(form.field, Field::Start);
        assert!(form.fields().contains(&Field::Profile));
        assert!(form.key(key(KeyCode::Enter)).is_none());
        assert_eq!(form.field, Field::Profile);
        form.paste("work");
        assert_eq!(submit(&mut form).unwrap().command, "omp --profile=work");
    }

    #[test]
    fn tab_leaves_inline_choices_without_selecting_and_escape_cancels() {
        let mut form = form();
        focus(&mut form, Field::Start);
        form.key(key(KeyCode::Down));
        form.key(key(KeyCode::Tab));
        assert_eq!(form.launcher.mode, LaunchMode::Shell);
        assert_eq!(form.field, Field::SaveDefault);
        assert!(matches!(
            form.key(key(KeyCode::Esc)),
            Some(FormAction::Cancel)
        ));
    }

    #[test]
    fn arriving_tools_do_not_retarget_the_focused_inline_choice() {
        let mut form = SessionForm::new(worker(), "~".into(), Launcher::default());
        focus(&mut form, Field::Start);
        form.key(key(KeyCode::Down));
        assert_eq!(form.start_choice, LaunchMode::Custom);
        form.set_available_launchers(Ok(installed(true, true, true)));
        assert_eq!(form.start_choice, LaunchMode::Custom);
        assert!(form.key(key(KeyCode::Char(' '))).is_none());
        assert_eq!(form.launcher.mode, LaunchMode::Custom);
        assert!(!form.busy);
        assert!(form.fields().contains(&Field::Command));
    }

    #[test]
    fn selected_start_becomes_the_session_command() {
        let mut form = form();
        form.paste("work");
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Omp);
        assert_eq!(submit(&mut form).unwrap().command, "omp");
        assert!(form.busy);

        let mut form = self::form();
        form.paste("work");
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Omp);
        focus(&mut form, Field::Profile);
        form.paste("team-a.v2");
        assert_eq!(
            submit(&mut form).unwrap().command,
            "omp --profile=team-a.v2"
        );

        let mut form = self::form();
        form.paste("work");
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::ResumeOmp);
        assert_eq!(form.launcher.mode, LaunchMode::ResumeOmp);
        focus(&mut form, Field::Profile);
        form.paste("work");
        assert_eq!(
            submit(&mut form).unwrap().command,
            "omp --resume --profile=work"
        );

        let mut form = self::form();
        form.paste("work");
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Custom);
        assert_eq!(form.launcher.mode, LaunchMode::Custom);
        assert!(submit(&mut form).is_err(), "a custom command is required");
        assert!(!form.busy);
        focus(&mut form, Field::Command);
        form.paste("claude --resume 'my session'");
        assert_eq!(
            submit(&mut form).unwrap().command,
            "claude --resume 'my session'"
        );
    }

    #[test]
    fn unsafe_profiles_are_refused_until_corrected_or_irrelevant() {
        let mut form = form();
        form.paste("work");
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Omp);
        focus(&mut form, Field::Profile);
        for profile in ["../etc", "a b", "$(id)", "x;y", "..", "ünï"] {
            focus(&mut form, Field::Profile);
            form.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
            form.paste(profile);
            assert!(submit(&mut form).is_err(), "accepted {profile:?}");
            assert!(!form.busy);
            assert!(!form.error.is_empty());
        }
        focus(&mut form, Field::Profile);
        form.paste("x");
        assert!(form.error.is_empty(), "editing clears the error");
        // The bad text is kept, but Shell ignores it and saves without it.
        form.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        form.paste("a b");
        focus(&mut form, Field::SaveDefault);
        form.key(key(KeyCode::Char(' ')));
        assert_eq!(form.launcher_to_save(), None, "invalid choice never saved");
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Shell);
        assert_eq!(form.launcher.mode, LaunchMode::Shell);
        assert_eq!(
            form.launcher.profile, "a b",
            "changing launchers preserves drafts"
        );
        assert_eq!(
            form.launcher_to_save(),
            Some(Launcher::default()),
            "Shell saves neither profile nor command"
        );
        assert_eq!(submit(&mut form).unwrap().command, "");
    }

    #[test]
    fn default_is_saved_only_when_explicitly_checked() {
        let seeded = Launcher {
            mode: LaunchMode::Omp,
            profile: "work".into(),
            command: "stale".into(),
        };
        let mut form = form_with(installed(true, false, false), seeded);
        assert_eq!(
            form.launcher.mode,
            LaunchMode::Omp,
            "the default seeds Start"
        );
        assert!(form.fields().contains(&Field::Profile));
        assert!(!form.save_default, "the checkbox starts unchecked");
        assert_eq!(form.launcher_to_save(), None);
        form.paste("work");
        assert_eq!(submit(&mut form).unwrap().command, "omp --profile=work");
        assert_eq!(form.launcher_to_save(), None, "unchecked: nothing to save");

        let mut form = self::form();
        form.paste("work");
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Omp);
        focus(&mut form, Field::Profile);
        form.paste("work");
        focus(&mut form, Field::SaveDefault);
        assert!(form.key(key(KeyCode::Char(' '))).is_none());
        assert!(form.save_default);
        form.key(key(KeyCode::Left));
        assert!(!form.save_default, "←/→ toggle as well");
        form.key(key(KeyCode::Right));
        assert!(form.save_default);
        assert_eq!(
            form.launcher_to_save(),
            Some(Launcher {
                mode: LaunchMode::Omp,
                profile: "work".into(),
                command: String::new(),
            })
        );
        assert_eq!(submit(&mut form).unwrap().command, "omp --profile=work");
        assert_eq!(
            form.launcher_to_save().map(|launcher| launcher.mode),
            Some(LaunchMode::Omp),
            "still available once the caller confirms creation"
        );
    }

    #[test]
    fn every_launcher_requires_the_final_create_action() {
        for mode in MODES {
            for activation in [KeyCode::Enter, KeyCode::Char(' ')] {
                let launcher = Launcher {
                    mode,
                    profile: "work".into(),
                    command: "printf ready".into(),
                };
                let expected_command = launcher.command_line().unwrap();
                let mut form = form_with(installed(true, true, true), launcher);
                form.paste("work");
                for &field in form.fields() {
                    if field == Field::Create {
                        break;
                    }
                    focus(&mut form, field);
                    assert!(
                        form.key(key(KeyCode::Enter)).is_none(),
                        "{mode:?} {field:?}"
                    );
                    assert!(!form.busy);
                    assert!(form.error.is_empty());
                }
                assert_eq!(form.field, Field::Create);
                assert!(!form.save_default);
                let (text, _) = rendered(&form, 24, 3);
                assert!(text.contains("Create session"), "{text}");
                form.paste("ignored");
                let Some(FormAction::Submit(request)) = form.key(key(activation)) else {
                    panic!("the final action must submit {mode:?}");
                };
                assert_eq!(request.name, "work");
                assert_eq!(request.command, expected_command);
                assert!(
                    form.key(key(activation)).is_none(),
                    "no duplicate submission"
                );
            }
        }
    }

    #[test]
    fn submitted_form_refuses_repeats_and_edits_but_allows_cancel() {
        let mut form = form();
        form.paste("work");
        focus(&mut form, Field::SaveDefault);
        form.key(key(KeyCode::Char(' ')));
        focus(&mut form, Field::Create);
        assert!(matches!(
            form.key(key(KeyCode::Enter)),
            Some(FormAction::Submit(_))
        ));
        assert!(form.key(key(KeyCode::Enter)).is_none());
        for frozen in [
            key(KeyCode::Char(' ')),
            key(KeyCode::Left),
            key(KeyCode::Right),
            key(KeyCode::Up),
            key(KeyCode::Down),
            key(KeyCode::Tab),
            key(KeyCode::Backspace),
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        ] {
            assert!(form.key(frozen).is_none());
        }
        form.paste("ignored");
        assert!(form.save_default, "busy: the checkbox stays as submitted");
        assert_eq!(form.field, Field::Create);
        assert_eq!(form.name, "work");
        assert_eq!(form.launcher, Launcher::default());
        assert!(form.launcher_to_save().is_some());
        for cancel in [
            key(KeyCode::Esc),
            key(KeyCode::BackTab),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            assert!(matches!(form.key(cancel), Some(FormAction::Cancel)));
        }
        // A failed attempt hands the form back with nothing lost.
        form.busy = false;
        form.error = "Computer is no longer ready.".into();
        assert!(form.save_default);
        focus(&mut form, Field::SaveDefault);
        form.key(key(KeyCode::Char(' ')));
        assert!(!form.save_default);
    }

    #[test]
    fn paste_is_bounded_text_not_submission_or_shortcuts() {
        let mut form = form();
        form.paste("work\r\n\t\u{1b}\u{3}\u{15}");
        assert_eq!(form.name, "work");
        assert!(!form.busy);
        assert_eq!(form.field, Field::Name);
        form.paste(&"界".repeat(100));
        assert_eq!(form.name.chars().count(), 64);
        form.key(key(KeyCode::Tab));
        form.paste(&"界".repeat(5000));
        assert_eq!(form.directory.chars().count(), 4096);
        assert!(!form.busy);
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Omp);
        focus(&mut form, Field::Profile);
        form.paste(&"a".repeat(100));
        assert_eq!(form.launcher.profile.len(), 64);
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Custom);
        focus(&mut form, Field::Command);
        form.paste(&"界".repeat(5000));
        assert_eq!(form.launcher.command.chars().count(), 4096);
    }

    #[test]
    fn inherited_directory_is_not_silently_rewritten() {
        let mut form = SessionForm::new(worker(), "/work\nother".into(), Launcher::default());
        form.paste("work");
        assert!(submit(&mut form).is_err());
        assert!(!form.busy);
    }

    #[test]
    fn invalid_submission_keeps_form_editable_and_release_does_not_submit() {
        let mut form = form();
        form.paste("bad.name");
        assert!(submit(&mut form).is_err());
        assert!(!form.error.is_empty());
        assert!(!form.busy);
        focus(&mut form, Field::Name);
        form.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        form.paste("valid");
        focus(&mut form, Field::Create);
        let mut release = key(KeyCode::Enter);
        release.kind = KeyEventKind::Release;
        assert!(form.key(release).is_none());
        assert!(!form.busy);
        assert!(matches!(
            form.key(key(KeyCode::Enter)),
            Some(FormAction::Submit(_))
        ));
    }

    #[test]
    fn tab_completes_only_in_directory_and_arrows_still_move_fields() {
        let mut form = form();
        assert!(form.key(key(KeyCode::Tab)).is_none());
        assert_eq!(form.field, Field::Directory);
        assert!(!form.directory_completion_pending());
        assert!(form.key(key(KeyCode::Down)).is_none());
        assert_eq!(form.field, Field::Start);
        assert!(form.key(key(KeyCode::Tab)).is_none());
        assert_eq!(form.field, Field::SaveDefault);
        assert!(form.key(key(KeyCode::Tab)).is_none());
        assert_eq!(form.field, Field::Create);
        assert!(form.key(key(KeyCode::Tab)).is_none());
        assert_eq!(form.field, Field::Name);
        assert!(form.key(key(KeyCode::Up)).is_none());
        assert_eq!(form.field, Field::Create);
        form.key(key(KeyCode::Up));
        assert_eq!(form.field, Field::SaveDefault);
        form.key(key(KeyCode::Up));
        form.key(key(KeyCode::Up));
        assert!(matches!(
            form.key(key(KeyCode::Tab)),
            Some(FormAction::CompleteDirectory(input)) if input == "~"
        ));
        assert_eq!(
            form.field,
            Field::Directory,
            "Tab in Directory keeps focus there"
        );
        assert!(matches!(
            form.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)),
            Some(FormAction::Cancel)
        ));
    }

    #[test]
    fn unique_match_fills_directory_with_trailing_slash() {
        let mut form = requested("~/pro");
        form.complete_directory("~/pro", Ok(found("~/pro", false, &["~/projects/"], false)));
        assert_eq!(form.directory, "~/projects/");
        assert!(!form.directory_completion_pending());
        let (text, _) = rendered(&form, 60, 8);
        assert!(text.contains("Directory exists."), "{text}");
        assert!(!text.contains("matches"), "{text}");
    }

    #[test]
    fn bare_tilde_is_normalized_and_children_listed() {
        let mut form = requested("~");
        form.complete_directory("~", Ok(found("~/", true, &["~/Desktop/", "~/dev/"], false)));
        assert_eq!(form.directory, "~/");
        let (text, _) = rendered(&form, 60, 12);
        assert!(text.contains("2 matches; type more, then Tab."), "{text}");
        assert!(text.contains("Desktop/  dev/"), "{text}");
        // An existing directory with nothing below it still takes its
        // normalized form, as a shell completes `~` to `~/`.
        let mut form = requested("~");
        form.complete_directory("~", Ok(found("~/", true, &[], false)));
        assert_eq!(form.directory, "~/");
        assert!(!form.directory_completion_pending());
    }

    #[test]
    fn ambiguous_matches_extend_unicode_common_prefix_and_list_names() {
        let mut form = requested("~/п");
        form.complete_directory(
            "~/п",
            Ok(found(
                "~/п",
                false,
                &["~/проекты/", "~/проба/", "~/прочее/"],
                false,
            )),
        );
        assert_eq!(form.directory, "~/про");
        assert!(!form.directory_completion_pending());
        let (text, _) = rendered(&form, 60, 12);
        assert!(text.contains("3 matches; type more, then Tab."), "{text}");
        assert!(text.contains("проекты/  проба/  прочее/"), "{text}");
        // Narrowing and pressing Tab again asks afresh; nothing is cached.
        form.paste("е");
        assert!(matches!(
            form.key(key(KeyCode::Tab)),
            Some(FormAction::CompleteDirectory(input)) if input == "~/прое"
        ));
    }

    #[test]
    fn truncated_results_never_autocomplete_but_still_show_partial_list() {
        let mut form = requested("/w");
        form.complete_directory(
            "/w",
            Ok(found("/w", false, &["/work-a/", "/work-b/"], true)),
        );
        assert_eq!(form.directory, "/w", "partial list proves no prefix");
        let (text, _) = rendered(&form, 60, 12);
        assert!(
            text.contains("Over 2 matches; type more, then Tab."),
            "{text}"
        );
        assert!(text.contains("work-a/  work-b/  …"), "{text}");
        // A single reported match on a truncated list is not unique either.
        let mut form = requested("/w");
        form.complete_directory("/w", Ok(found("/w", false, &["/work-a/"], true)));
        assert_eq!(form.directory, "/w");
    }

    #[test]
    fn listed_matches_are_bounded() {
        let mut form = requested("/");
        let many: Vec<String> = (0..40).map(|i| format!("/dir{i:02}/")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        form.complete_directory("/", Ok(found("/", true, &refs, false)));
        assert_eq!(
            form.directory, "/dir",
            "a full list still proves its prefix"
        );
        let listed = form.listed_matches().unwrap();
        assert!(listed.contains("dir23/  …"), "{listed}");
        assert!(!listed.contains("dir24/"), "{listed}");
    }

    #[test]
    fn missing_directory_and_lookup_errors_stay_distinct() {
        let mut form = requested("/nope");
        form.complete_directory("/nope", Ok(found("/nope", false, &[], false)));
        assert_eq!(form.directory, "/nope");
        let (text, _) = rendered(&form, 60, 8);
        assert!(text.contains("No matching directory."), "{text}");

        let mut form = requested("/nope");
        form.complete_directory("/nope", Err("ssh: connection closed".into()));
        assert_eq!(form.directory, "/nope");
        assert!(!form.directory_completion_pending());
        assert!(
            form.error.is_empty(),
            "lookup failure is not a validation error"
        );
        let (text, _) = rendered(&form, 60, 8);
        assert!(
            text.contains("Directory check failed: ssh: connection closed"),
            "{text}"
        );
        assert!(!text.contains("No matching"), "{text}");

        let mut form = requested("/srv/");
        form.complete_directory("/srv/", Ok(found("/srv/", true, &[], false)));
        let (text, _) = rendered(&form, 60, 8);
        assert!(
            text.contains("Directory exists; nothing more to complete."),
            "{text}"
        );
        // Typing again clears the verdict; it belongs to the old text.
        form.paste("x");
        assert_eq!(form.completion, Completion::Idle);
    }

    #[test]
    fn edits_invalidate_pending_lookup_even_when_typed_back() {
        let mut form = requested("~/pro");
        assert!(form.key(key(KeyCode::Backspace)).is_none());
        assert!(!form.directory_completion_pending());
        form.paste("o");
        assert_eq!(form.directory, "~/pro");
        assert!(!form.directory_completion_pending());
        form.complete_directory("~/pro", Ok(found("~/pro", false, &["~/projects/"], false)));
        assert_eq!(form.directory, "~/pro", "stale reply must not apply");
        assert_eq!(form.completion, Completion::Idle);

        let mut form = requested("~/pro");
        form.complete_directory("~/pr", Ok(found("~/pr", false, &["~/private/"], false)));
        assert_eq!(form.directory, "~/pro", "reply for other text ignored");
        assert!(
            form.directory_completion_pending(),
            "the real request is still awaited"
        );
        form.complete_directory("~/pro", Err("late".into()));
        assert!(!form.directory_completion_pending());
        form.complete_directory("~/pro", Ok(found("~/pro", false, &["~/projects/"], false)));
        assert_eq!(
            form.directory, "~/pro",
            "a settled request accepts no second reply"
        );
    }

    #[test]
    fn reply_targets_directory_even_when_focus_moved_or_form_submitted() {
        let mut form = requested("~/pro");
        form.key(key(KeyCode::Down));
        form.paste("omp");
        choose(&mut form, LaunchMode::Omp);
        assert!(
            form.directory_completion_pending(),
            "other controls do not cancel it"
        );
        form.complete_directory("~/pro", Ok(found("~/pro", false, &["~/projects/"], false)));
        assert_eq!(form.directory, "~/projects/");
        assert_eq!(form.field, Field::Start);
        assert_eq!(form.launcher.mode, LaunchMode::Omp);
        assert_eq!(form.launcher.profile, "", "pasted text never selects");

        let mut form = requested("~/pro");
        form.key(key(KeyCode::Up));
        form.paste("work");
        focus(&mut form, Field::Create);
        assert!(matches!(
            form.key(key(KeyCode::Enter)),
            Some(FormAction::Submit(request)) if request.directory == "~/pro"
        ));
        assert!(!form.directory_completion_pending());
        form.complete_directory("~/pro", Ok(found("~/pro", false, &["~/projects/"], false)));
        assert_eq!(form.directory, "~/pro", "submitted text stays as sent");
        let (text, _) = rendered(&form, 60, 8);
        assert!(text.contains("Creating… Esc cancels"), "{text}");
    }

    #[test]
    fn directory_matches_stay_beneath_the_path_before_other_settings() {
        let mut form = requested("~/pro");
        form.complete_directory(
            "~/pro",
            Ok(found("~/pro", false, &["~/projects/", "~/proto/"], false)),
        );
        for field in [Field::Directory, Field::Start, Field::Create] {
            focus(&mut form, field);
            let (text, _) = rendered(&form, 70, 20);
            let lines: Vec<_> = text.lines().collect();
            let path = lines
                .iter()
                .position(|line| line.contains("~/pro"))
                .unwrap();
            assert!(lines[path + 1].contains("projects/  proto/"), "{text}");
            assert!(lines[path + 2].contains("2 matches"), "{text}");
            assert!(lines[path + 3].contains("Start"), "{text}");
        }
        // Feedback cannot displace the focused final action on a short terminal.
        let (text, _) = rendered(&form, 24, 3);
        assert!(text.contains("Create session"), "{text}");
        assert!(!text.contains("projects/"), "{text}");
        focus(&mut form, Field::Directory);
        let (text, _) = rendered(&form, 40, 6);
        let lines: Vec<_> = text.lines().collect();
        let path = lines
            .iter()
            .position(|line| line.contains("~/pro"))
            .unwrap();
        assert!(lines[path + 1].contains("projects/  proto/"), "{text}");
    }

    #[test]
    fn three_long_directory_matches_each_get_their_wrapped_row() {
        let mut form = requested("~/project-");
        form.complete_directory(
            "~/project-",
            Ok(found(
                "~/project-",
                false,
                &[
                    "~/project-alpha-long-name/",
                    "~/project-beta-long-name/",
                    "~/project-delta-long-name/",
                ],
                false,
            )),
        );
        for width in [44, 46] {
            let (text, _) = rendered(&form, width, 24);
            let lines: Vec<_> = text.lines().collect();
            let path = lines
                .iter()
                .position(|line| line.contains("~/project-"))
                .unwrap();
            assert!(
                lines[path + 1].contains("project-alpha-long-name/"),
                "{text}"
            );
            assert!(
                lines[path + 2].contains("project-beta-long-name/"),
                "{text}"
            );
            assert!(
                lines[path + 3].contains("project-delta-long-name/"),
                "{text}"
            );
            assert!(lines[path + 4].contains("3 matches"), "{text}");
            assert!(lines[path + 5].contains("Start"), "{text}");
            assert!(!text.contains("… more results"), "{text}");
        }
    }

    #[test]
    fn clipped_directory_matches_show_overflow_at_row_cap_and_short_heights() {
        let mut form = requested("~/project-");
        form.complete_directory(
            "~/project-",
            Ok(found(
                "~/project-",
                false,
                &[
                    "~/project-alpha-long-name/",
                    "~/project-beta-long-name/",
                    "~/project-delta-long-name/",
                    "~/project-gamma-long-name/",
                ],
                false,
            )),
        );
        for height in [6, 7, 8, 24] {
            let (text, cursor) = rendered(&form, 44, height);
            let lines: Vec<_> = text.lines().collect();
            let path = lines
                .iter()
                .position(|line| line.contains("~/project-"))
                .unwrap();
            let overflow = lines
                .iter()
                .position(|line| line.contains("… more results"))
                .unwrap_or_else(|| panic!("hidden matches need an indicator: {text}"));
            assert!(overflow > path, "{text}");
            assert!(lines[overflow + 1].contains("4 matches"), "{text}");
            assert_eq!(cursor, Position::new(11, path as u16));
            assert!(!text.contains("project-delta-long-name/"), "{text}");
            assert!(!text.contains("project-gamma-long-name/"), "{text}");
            if height >= 8 {
                assert!(
                    lines[path + 1].contains("project-alpha-long-name/"),
                    "{text}"
                );
                assert!(
                    lines[path + 2].contains("project-beta-long-name/"),
                    "{text}"
                );
                assert_eq!(overflow, path + 3, "{text}");
            } else if height == 6 {
                assert_eq!(overflow, path + 1, "{text}");
                assert!(!text.contains("project-alpha-long-name/"), "{text}");
            }
        }
    }

    #[test]
    fn word_wrapped_directory_and_global_errors_keep_their_last_line_and_cursor() {
        let mut form = requested("~/pro");
        form.complete_directory(
            "~/pro",
            Err("authentication-failed destination-unreachable".into()),
        );
        let (text, cursor) = rendered(&form, 44, 6);
        assert!(text.contains("Directory check failed:"), "{text}");
        assert!(text.contains("authentication-failed"), "{text}");
        assert!(text.contains("destination-unreachable"), "{text}");
        assert_eq!(cursor, Position::new(6, 1));

        form.error = "connection-unavailable authentication-failed destination-unreachable".into();
        let (text, cursor) = rendered(&form, 44, 6);
        assert!(text.contains("~/pro"), "{text}");
        assert!(text.contains("connection-unavailable"), "{text}");
        assert!(text.contains("authentication-failed"), "{text}");
        assert!(text.contains("destination-unreachable"), "{text}");
        assert!(!text.contains("Directory check failed:"), "{text}");
        assert_eq!(cursor, Position::new(6, 1));
    }

    #[test]
    fn tiny_layouts_keep_directory_input_cursor_and_status() {
        let form = requested("~/pro");
        // One inner row: only the focused input survives, cursor after it.
        let (text, cursor) = rendered(&form, 20, 3);
        assert_eq!(
            text.lines().nth(1).unwrap().trim_matches('│').trim(),
            "~/pro"
        );
        assert_eq!(cursor, Position::new(6, 1));
        // Two rows: the input keeps its cursor and the status shows.
        let (text, cursor) = rendered(&form, 30, 4);
        assert!(text.contains("~/pro"), "{text}");
        assert!(text.contains("Checking directory…"), "{text}");
        assert_eq!(cursor, Position::new(6, 1));

        let mut form = form;
        form.complete_directory(
            "~/pro",
            Ok(found("~/pro", false, &["~/projects/", "~/proto/"], false)),
        );
        form.error = "Computer is no longer ready.".into();
        let (text, cursor) = rendered(&form, 40, 5);
        assert!(text.contains("Directory"), "{text}");
        assert!(text.contains("~/pro"), "{text}");
        assert!(text.contains("Computer is no longer ready."), "{text}");
        assert!(
            !text.contains("projects/"),
            "error row outranks matches: {text}"
        );
        assert_eq!(cursor, Position::new(6, 2));
        form.error.clear();
        let (text, _) = rendered(&form, 40, 5);
        assert!(text.contains("2 matches; type more, then Tab."), "{text}");
        assert!(!text.contains("projects/"), "no room for the list: {text}");
        let (text, _) = rendered(&form, 40, 6);
        assert!(text.contains("projects/  proto/"), "{text}");
    }

    #[test]
    fn inline_choices_are_visible_without_opening_and_keep_focus_in_short_layouts() {
        let mut form = form_with(installed(true, true, true), Launcher::default());
        let (initial, _) = rendered(&form, 70, 20);
        for mode in MODES {
            assert!(initial.contains(mode_label(mode)), "{initial}");
        }
        focus(&mut form, Field::Start);
        for _ in 0..4 {
            form.key(key(KeyCode::Down));
        }
        for (width, height) in [(28, 3), (40, 6), (70, 14)] {
            let (text, _) = rendered(&form, width, height);
            assert!(
                text.lines()
                    .any(|line| line.contains("Codex") && line.contains('›')),
                "{text}"
            );
        }
        let (text, _) = rendered(&form, 70, 14);
        let option_rows: Vec<_> = MODES
            .iter()
            .map(|mode| {
                text.lines()
                    .position(|line| line.contains(mode_label(*mode)))
                    .unwrap()
            })
            .collect();
        assert!(
            option_rows.windows(2).all(|rows| rows[0] < rows[1]),
            "{text}"
        );
        focus(&mut form, Field::SaveDefault);
        form.key(key(KeyCode::Char(' ')));
        let (text, _) = rendered(&form, 30, 3);
        assert!(text.contains("[x] Use as default"), "{text}");
    }

    #[test]
    fn claude_and_codex_start_their_tools_without_profile_or_command() {
        let mut form = form_with(installed(true, true, true), Launcher::default());
        form.paste("work");
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Omp);
        focus(&mut form, Field::Profile);
        form.paste("team");
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Claude);
        assert_eq!(form.launcher.mode, LaunchMode::Claude);
        assert_eq!(
            form.fields(),
            [
                Field::Name,
                Field::Directory,
                Field::Start,
                Field::SaveDefault,
                Field::Create
            ]
        );
        let (text, _) = rendered(&form, 100, 14);
        assert!(!text.contains("team"), "OMP's profile draft leaks: {text}");
        assert!(!text.contains("profile"), "{text}");
        focus(&mut form, Field::SaveDefault);
        form.key(key(KeyCode::Char(' ')));
        assert_eq!(
            form.launcher_to_save(),
            Some(Launcher {
                mode: LaunchMode::Claude,
                profile: String::new(),
                command: String::new(),
            }),
            "the profile draft is not saved with Claude"
        );
        assert_eq!(submit(&mut form).unwrap().command, "claude");

        let mut form = form_with(installed(false, false, true), Launcher::default());
        form.paste("work");
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Codex);
        assert_eq!(
            form.launcher.mode,
            LaunchMode::Codex,
            "absent tools are hidden"
        );
        let (text, _) = rendered(&form, 100, 14);
        assert!(!text.contains("OMP") && !text.contains("Claude"), "{text}");
        assert_eq!(submit(&mut form).unwrap().command, "codex");
    }

    #[test]
    fn missing_saved_tool_stays_selected_with_a_warning_until_left_deliberately() {
        let seeded = Launcher {
            mode: LaunchMode::Claude,
            ..Launcher::default()
        };
        let mut form = form_with(installed(true, false, false), seeded);
        form.paste("work");
        assert_eq!(
            form.launcher.mode,
            LaunchMode::Claude,
            "the default is never silently replaced"
        );
        assert!(submit(&mut form).is_err());
        assert!(!form.busy);
        focus(&mut form, Field::Start);
        // Narrow and short: the chosen one keeps its note beside the error.
        let (text, _) = rendered(&form, 30, 6);
        assert!(text.contains("Claude (not installed)"), "{text}");
        form.key(key(KeyCode::Char(' ')));
        assert!(form.key(key(KeyCode::Enter)).is_none());
        assert_eq!(form.launcher.mode, LaunchMode::Claude);
        assert_eq!(
            form.field,
            Field::Start,
            "unavailable choice cannot advance"
        );
        form.key(key(KeyCode::Down));
        form.key(key(KeyCode::Enter));
        assert_eq!(
            form.launcher.mode,
            LaunchMode::Custom,
            "Codex is hidden too"
        );
        assert!(form.error.is_empty());
        choose(&mut form, LaunchMode::ResumeOmp);
        assert_eq!(
            form.launcher.mode,
            LaunchMode::ResumeOmp,
            "once left, the missing tool is gone"
        );
        let (text, _) = rendered(&form, 100, 14);
        assert!(!text.contains("Claude"), "{text}");
        assert!(!text.contains("not installed"), "{text}");
        choose(&mut form, LaunchMode::Shell);
        assert_eq!(form.launcher.mode, LaunchMode::Shell);
        assert_eq!(submit(&mut form).unwrap().command, "");
    }

    #[test]
    fn pending_and_failed_checks_refuse_tools_without_claiming_absence() {
        let seeded = Launcher {
            mode: LaunchMode::Omp,
            ..Launcher::default()
        };
        let mut form = SessionForm::new(worker(), "~".into(), seeded);
        form.paste("work");
        let (text, _) = rendered(&form, 100, 14);
        assert!(
            text.contains("Checking which tools are installed there"),
            "{text}"
        );
        let error = submit(&mut form).unwrap_err();
        assert!(
            error.starts_with("Still checking whether OMP is installed there"),
            "{error}"
        );
        assert!(!error.contains("not installed"), "{error}");
        assert!(!form.busy);
        // The answer drops that refusal; OMP and Claude join, Codex does not.
        form.set_available_launchers(Ok(installed(true, true, false)));
        assert!(form.error.is_empty(), "{}", form.error);
        let (text, _) = rendered(&form, 100, 14);
        assert!(!text.contains("Checking which tools"), "{text}");
        assert_eq!(submit(&mut form).unwrap().command, "omp");

        let mut form = SessionForm::new(worker(), "~".into(), Launcher::default());
        form.paste("bad.name");
        assert!(submit(&mut form).is_err());
        let unrelated = form.error.clone();
        form.set_available_launchers(Ok(installed(true, false, false)));
        assert_eq!(form.error, unrelated, "a name error survives the answer");

        // Shell starts while the check is pending; the answer arriving after
        // submission is ignored, as the caller has cancelled the lookup.
        let mut form = SessionForm::new(worker(), "~".into(), Launcher::default());
        form.paste("work");
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Custom);
        assert_eq!(
            form.launcher.mode,
            LaunchMode::Custom,
            "no tool is offered yet"
        );
        choose(&mut form, LaunchMode::Shell);
        assert_eq!(submit(&mut form).unwrap().command, "");
        form.set_available_launchers(Ok(installed(true, true, true)));
        assert_eq!(form.available, Availability::Checking);

        // A failed check is reported as such, never as absence.
        let seeded = Launcher {
            mode: LaunchMode::Codex,
            ..Launcher::default()
        };
        let mut form = SessionForm::new(worker(), "~".into(), seeded);
        form.paste("work");
        form.set_available_launchers(Err("ssh: connection closed".into()));
        let (text, _) = rendered(&form, 100, 14);
        assert!(
            text.contains("Tool check failed: ssh: connection closed."),
            "{text}"
        );
        assert_eq!(
            submit(&mut form).unwrap_err(),
            "Could not check whether Codex is installed there: ssh: connection closed"
        );
        focus(&mut form, Field::Start);
        choose(&mut form, LaunchMode::Shell);
        assert_eq!(form.launcher.mode, LaunchMode::Shell);
        assert!(form.error.is_empty());
        assert_eq!(submit(&mut form).unwrap().command, "");
    }

    #[test]
    fn tool_check_status_yields_to_errors_and_directory_feedback() {
        let mut form = SessionForm::new(worker(), "~".into(), Launcher::default());
        form.key(key(KeyCode::Down));
        form.key(key(KeyCode::Tab));
        let (text, _) = rendered(&form, 40, 5);
        assert!(text.contains("Checking directory…"), "{text}");
        assert!(!text.contains("Checking which tools"), "{text}");
        form.complete_directory("~", Ok(found("~/", true, &[], false)));
        let (text, _) = rendered(&form, 60, 8);
        assert!(
            text.contains("Directory exists; nothing more to complete."),
            "{text}"
        );
        assert!(!text.contains("Checking which tools"), "{text}");
        form.error = "Computer is no longer ready.".into();
        let (text, _) = rendered(&form, 60, 8);
        assert!(text.contains("Computer is no longer ready."), "{text}");
        assert!(!text.contains("Checking which tools"), "{text}");
        form.error.clear();
        form.paste("x");
        let (text, _) = rendered(&form, 60, 8);
        assert!(text.contains("Checking which tools"), "{text}");
    }

    #[test]
    fn common_prefix_is_character_safe() {
        assert_eq!(common_prefix(&[]), "");
        assert_eq!(common_prefix(&["~/東京/".into()]), "~/東京/");
        assert_eq!(
            common_prefix(&["~/東京/".into(), "~/東北/".into(), "~/東海/".into()]),
            "~/東"
        );
        assert_eq!(common_prefix(&["a/".into(), "b/".into()]), "");
        assert_eq!(common_prefix(&["~/pro/".into(), "~/pro/".into()]), "~/pro/");
    }

    #[test]
    fn compact_forms_show_partial_choices_without_hiding_focused_controls() {
        let mut form = form_with(installed(true, true, true), Launcher::default());
        let (text, _) = rendered(&form, 40, 12);
        assert!(
            text.contains("Name") && text.contains("Start") && text.contains("Shell"),
            "{text}"
        );
        choose(&mut form, LaunchMode::Custom);
        focus(&mut form, Field::Create);
        let (text, _) = rendered(&form, 40, 12);
        assert!(text.contains("Start") && text.contains("Codex"), "{text}");
        assert!(
            text.contains("(•) Custom command") && text.contains("Create session"),
            "{text}"
        );
    }
}
