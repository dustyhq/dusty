//! The grouped Problems list drawn in its native dock.

use crate::config::Config;
use crate::windows::WindowRect;
use crate::workspace::DiagnosticSeverity;
use crate::{Frame, Style};
use std::sync::Arc;

/// One visual row; group headings are not selectable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProblemsRow {
    File {
        path: String,
        count: usize,
    },
    Problem {
        severity: DiagnosticSeverity,
        message: String,
        source: String,
        location: String,
    },
}

/// Immutable rows and the current viewport and selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProblemsPanelUi {
    pub rows: Arc<[ProblemsRow]>,
    pub scroll: usize,
    pub selected_row: Option<usize>,
}

impl ProblemsPanelUi {
    /// Rows available beneath the dock's title.
    pub fn capacity(rect: WindowRect) -> usize {
        usize::from(rect.height.saturating_sub(1))
    }
}

fn single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

pub(super) fn render(
    frame: &mut Frame,
    panel: &ProblemsPanelUi,
    config: &Config,
    rect: WindowRect,
    focused: bool,
) {
    let grid = &mut frame.grid;
    let right = rect.x.saturating_add(rect.width);
    let bottom = rect.y.saturating_add(rect.height);
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    for y in rect.y..bottom {
        for x in rect.x..right {
            grid.set_cell(x, y, " ", Style::Default);
        }
    }
    let (horizontal, ..) = grid.border_glyphs(crate::config::BorderStyle::Single);
    let horizontal = horizontal.to_string();
    for x in rect.x..right {
        grid.set_cell(x, rect.y, &horizontal, Style::TerminalBorder);
    }
    grid.put_text_clipped(
        rect.x.saturating_add(1),
        rect.y,
        " Problems ",
        if focused {
            Style::Accent
        } else {
            Style::TerminalTitle
        },
        right,
    );
    let left = rect.x.saturating_add(2);
    if panel.rows.is_empty() && rect.height > 1 {
        grid.put_text_clipped(left, rect.y + 1, "No problems", Style::Muted, right);
    }
    for (offset, (row, y)) in panel
        .rows
        .iter()
        .skip(panel.scroll)
        .zip(rect.y + 1..bottom)
        .enumerate()
    {
        let selected = panel.selected_row == Some(panel.scroll + offset);
        match row {
            ProblemsRow::File { path, count } => {
                let glyph = &config.ui.explorer.active_glyphs().file;
                let line = format!("{glyph} {} ({count})", single_line(path));
                grid.put_text_clipped(left, y, &line, Style::ExplorerHeader, right);
            }
            ProblemsRow::Problem {
                severity,
                message,
                source,
                location,
            } => {
                let (icon, style) = match severity {
                    DiagnosticSeverity::Error => ("✗", Style::Error),
                    DiagnosticSeverity::Warning => ("!", Style::Warning),
                    DiagnosticSeverity::Information => ("i", Style::Accent),
                    DiagnosticSeverity::Hint => ("·", Style::Muted),
                };
                let mark = if selected { "›" } else { " " };
                grid.put_text_clipped(rect.x, y, &format!("{mark} {icon}"), style, right);
                let start = rect.x.saturating_add(4).min(right);
                let location_width =
                    u16::try_from(grid.display_width(location)).unwrap_or(u16::MAX);
                let location_start = right.saturating_sub(location_width.saturating_add(1));
                let content_end = if !location.is_empty() && location_start > start {
                    grid.put_text_clipped(location_start, y, location, Style::Muted, right - 1);
                    location_start.saturating_sub(2).max(start)
                } else {
                    right
                };
                let room = content_end.saturating_sub(start);
                let source = single_line(source);
                let source_room = u16::try_from(grid.display_width(&source))
                    .unwrap_or(u16::MAX)
                    .min(room / 3);
                let message_room = if source_room == 0 {
                    room
                } else {
                    room - source_room - 1
                };
                let written = grid.put_text_clipped(
                    start,
                    y,
                    &single_line(message),
                    if selected {
                        Style::Selection
                    } else {
                        Style::Default
                    },
                    start + message_room,
                );
                if source_room > 0 {
                    grid.put_text_clipped(
                        written + 1,
                        y,
                        &source,
                        Style::Muted,
                        (written + 1 + source_room).min(content_end),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems_groups_and_empty_state_at_100_by_30() {
        let config = Config::default();
        let rect = WindowRect {
            x: 0,
            y: 19,
            width: 100,
            height: 10,
        };
        let mut frame = Frame {
            grid: crate::CellGrid::new(100, 30),
            cursor: None,
        };
        let problem = |severity, message: &str| ProblemsRow::Problem {
            severity,
            message: message.into(),
            source: "check E001".into(),
            location: "5:2".into(),
        };
        let mut panel = ProblemsPanelUi {
            rows: vec![
                ProblemsRow::File {
                    path: "src/a.rs".into(),
                    count: 2,
                },
                problem(DiagnosticSeverity::Error, "first error"),
                problem(DiagnosticSeverity::Warning, "second warning"),
                ProblemsRow::File {
                    path: "src/b.rs".into(),
                    count: 1,
                },
                problem(DiagnosticSeverity::Error, "another error"),
            ]
            .into(),
            scroll: 0,
            selected_row: Some(1),
        };
        render(&mut frame, &panel, &config, rect, true);
        assert!(frame.grid.row_text(20).contains("src/a.rs (2)"));
        assert!(frame.grid.row_text(21).contains("first error"));
        assert!(frame.grid.row_text(21).trim_end().ends_with("5:2"));
        assert_eq!(frame.grid.get(96, 21).unwrap().symbol, "5");
        assert!(frame.grid.row_text(22).contains("second warning"));
        assert!(frame.grid.row_text(23).contains("src/b.rs (1)"));
        assert!(frame.grid.row_text(24).contains("another error"));
        panel.rows = Arc::from([]);
        render(&mut frame, &panel, &config, rect, false);
        assert_eq!(frame.grid.row_text(20).trim(), "No problems");
        assert_eq!(frame.grid.get(2, 20).unwrap().style, Style::Muted);
        assert!(frame.grid.row_text(21).trim().is_empty());
    }
}
