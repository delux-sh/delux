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

const DIRECTORY: usize = 1;

/// Shown match names are capped so a wide directory never floods the form.
const LISTED_MATCHES: usize = 24;

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
    values: [String; 3],
    field: usize,
    completion: Completion,
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
            completion: Completion::Idle,
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
            self.edited();
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
                    directory: self.values[DIRECTORY].clone(),
                    command: self.values[2].clone(),
                };
                match session.validate() {
                    Ok(()) => {
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
            KeyCode::Tab if self.field == DIRECTORY => {
                let input = self.values[DIRECTORY].clone();
                self.error.clear();
                self.completion = Completion::Pending(input.clone());
                return Some(FormAction::CompleteDirectory(input));
            }
            KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % 3,
            KeyCode::Up => self.field = (self.field + 2) % 3,
            KeyCode::Backspace => {
                self.values[self.field].pop();
                self.edited();
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
        self.edited();
    }

    fn edited(&mut self) {
        self.error.clear();
        if self.field == DIRECTORY {
            self.completion = Completion::Idle;
        }
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
                if !fill.is_empty() && fill != self.values[DIRECTORY] {
                    self.values[DIRECTORY] = fill.to_owned();
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

    /// The match names (not full paths) to list under the status row.
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

        // Preserve an editable field first, then an error/status row. Matches,
        // help and the other fields yield space on short terminals rather than
        // hiding focus.
        let status = if !self.error.is_empty() {
            Some((display(&self.error, 4096), theme::error()))
        } else if self.busy {
            Some(("Creating… Esc cancels".to_owned(), tint.secondary()))
        } else {
            self.completion_status(tint)
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
        if let Some(listed) = self.listed_matches()
            && self.error.is_empty()
            && !self.busy
            && y < bottom
        {
            let rows = (bottom - y).min(3);
            frame.render_widget(
                Paragraph::new(listed)
                    .style(tint.text())
                    .wrap(Wrap { trim: false }),
                Rect::new(inner.x, y, inner.width, rows),
            );
            y += rows;
        }
        for hint in [
            "Directory: ~, ~/path, or an absolute path on that computer.",
            "Tab completes Directory there; ↑/↓ move between fields.",
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
        assert_eq!(form.field, DIRECTORY);
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

    #[test]
    fn tab_completes_only_in_directory_and_arrows_still_move_fields() {
        let mut form = form();
        assert!(form.key(key(KeyCode::Tab)).is_none());
        assert_eq!(form.field, DIRECTORY);
        assert!(!form.directory_completion_pending());
        assert!(form.key(key(KeyCode::Down)).is_none());
        assert_eq!(form.field, 2);
        assert!(form.key(key(KeyCode::Tab)).is_none());
        assert_eq!(form.field, 0);
        assert!(form.key(key(KeyCode::Up)).is_none());
        assert_eq!(form.field, 2);
        form.key(key(KeyCode::Up));
        assert!(matches!(
            form.key(key(KeyCode::Tab)),
            Some(FormAction::CompleteDirectory(input)) if input == "~"
        ));
        assert_eq!(form.field, DIRECTORY, "Tab in Directory keeps focus there");
        assert!(matches!(
            form.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)),
            Some(FormAction::Cancel)
        ));
    }

    #[test]
    fn unique_match_fills_directory_with_trailing_slash() {
        let mut form = requested("~/pro");
        form.complete_directory("~/pro", Ok(found("~/pro", false, &["~/projects/"], false)));
        assert_eq!(form.values[DIRECTORY], "~/projects/");
        assert!(!form.directory_completion_pending());
        let (text, _) = rendered(&form, 60, 8);
        assert!(text.contains("Directory exists."), "{text}");
        assert!(!text.contains("matches"), "{text}");
    }

    #[test]
    fn bare_tilde_is_normalized_and_children_listed() {
        let mut form = requested("~");
        form.complete_directory("~", Ok(found("~/", true, &["~/Desktop/", "~/dev/"], false)));
        assert_eq!(form.values[DIRECTORY], "~/");
        let (text, _) = rendered(&form, 60, 12);
        assert!(text.contains("2 matches; type more, then Tab."), "{text}");
        assert!(text.contains("Desktop/  dev/"), "{text}");
        // An existing directory with nothing below it still takes its
        // normalized form, as a shell completes `~` to `~/`.
        let mut form = requested("~");
        form.complete_directory("~", Ok(found("~/", true, &[], false)));
        assert_eq!(form.values[DIRECTORY], "~/");
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
        assert_eq!(form.values[DIRECTORY], "~/про");
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
        assert_eq!(
            form.values[DIRECTORY], "/w",
            "partial list proves no prefix"
        );
        let (text, _) = rendered(&form, 60, 12);
        assert!(
            text.contains("Over 2 matches; type more, then Tab."),
            "{text}"
        );
        assert!(text.contains("work-a/  work-b/  …"), "{text}");
        // A single reported match on a truncated list is not unique either.
        let mut form = requested("/w");
        form.complete_directory("/w", Ok(found("/w", false, &["/work-a/"], true)));
        assert_eq!(form.values[DIRECTORY], "/w");
    }

    #[test]
    fn listed_matches_are_bounded() {
        let mut form = requested("/");
        let many: Vec<String> = (0..40).map(|i| format!("/dir{i:02}/")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        form.complete_directory("/", Ok(found("/", true, &refs, false)));
        assert_eq!(
            form.values[DIRECTORY], "/dir",
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
        assert_eq!(form.values[DIRECTORY], "/nope");
        let (text, _) = rendered(&form, 60, 8);
        assert!(text.contains("No matching directory."), "{text}");

        let mut form = requested("/nope");
        form.complete_directory("/nope", Err("ssh: connection closed".into()));
        assert_eq!(form.values[DIRECTORY], "/nope");
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
        assert_eq!(form.values[DIRECTORY], "~/pro");
        assert!(!form.directory_completion_pending());
        form.complete_directory("~/pro", Ok(found("~/pro", false, &["~/projects/"], false)));
        assert_eq!(
            form.values[DIRECTORY], "~/pro",
            "stale reply must not apply"
        );
        assert_eq!(form.completion, Completion::Idle);

        let mut form = requested("~/pro");
        form.complete_directory("~/pr", Ok(found("~/pr", false, &["~/private/"], false)));
        assert_eq!(
            form.values[DIRECTORY], "~/pro",
            "reply for other text ignored"
        );
        assert!(
            form.directory_completion_pending(),
            "the real request is still awaited"
        );
        form.complete_directory("~/pro", Err("late".into()));
        assert!(!form.directory_completion_pending());
        form.complete_directory("~/pro", Ok(found("~/pro", false, &["~/projects/"], false)));
        assert_eq!(
            form.values[DIRECTORY], "~/pro",
            "a settled request accepts no second reply"
        );
    }

    #[test]
    fn reply_targets_directory_even_when_focus_moved_or_form_submitted() {
        let mut form = requested("~/pro");
        form.key(key(KeyCode::Down));
        form.paste("omp");
        assert!(
            form.directory_completion_pending(),
            "other fields do not cancel it"
        );
        form.complete_directory("~/pro", Ok(found("~/pro", false, &["~/projects/"], false)));
        assert_eq!(form.values, ["", "~/projects/", "omp"]);
        assert_eq!(form.field, 2);

        let mut form = requested("~/pro");
        form.key(key(KeyCode::Up));
        form.paste("work");
        assert!(matches!(
            form.key(key(KeyCode::Enter)),
            Some(FormAction::Submit(request)) if request.directory == "~/pro"
        ));
        assert!(!form.directory_completion_pending());
        form.complete_directory("~/pro", Ok(found("~/pro", false, &["~/projects/"], false)));
        assert_eq!(
            form.values[DIRECTORY], "~/pro",
            "submitted text stays as sent"
        );
        let (text, _) = rendered(&form, 60, 8);
        assert!(text.contains("Creating… Esc cancels"), "{text}");
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
}
