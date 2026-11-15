//! Pure geometry for the picker chrome and its sidebar/client panes.

use ratatui::layout::{Constraint, Layout, Rect};

pub(super) fn chrome(area: Rect, status_lines: usize) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length((status_lines as u16).min(area.height.saturating_sub(5))),
        Constraint::Length(1),
    ])
    .areas(area)
}

pub(super) struct Panes {
    pub(super) compact: bool,
    pub(super) list: Rect,
    pub(super) preview: Option<Rect>,
}

pub(super) fn panes(body: Rect, rows: usize, active: bool, focused: bool) -> Panes {
    // Keep the sidebar stable through scans, filtering, and selection changes.
    // At 80 columns it leaves 48 for the preview or terminal.
    let compact = body.width >= 80 || body.height >= 24;
    let width = 32;
    let beside = compact && body.width.saturating_sub(width) >= 48;
    let (list_area, preview_area) = if focused && (!beside || body.height < 3) && body.height < 24 {
        (Rect::default(), Some(body))
    } else if beside {
        let [list, preview] =
            Layout::horizontal([Constraint::Length(width), Constraint::Min(48)]).areas(body);
        (list, Some(preview))
    } else if body.height >= 24 {
        // Fit short lists; long lists scroll within half the available height.
        // A live client keeps its size while scans and filters change the rows.
        let half = body.height / 2;
        let list_height = if active {
            half
        } else {
            rows.max(1).saturating_add(2).min(usize::from(half)) as u16
        };
        let [list, preview] =
            Layout::vertical([Constraint::Length(list_height), Constraint::Min(10)]).areas(body);
        (list, Some(preview))
    } else {
        (body, None)
    };
    Panes {
        compact,
        list: list_area,
        preview: preview_area,
    }
}
