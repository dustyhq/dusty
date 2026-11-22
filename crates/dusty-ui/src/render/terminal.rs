//! Terminal grid, scrollback and selection drawing.

use crate::grid::CONTROL_CHARACTER_REPLACEMENT;
use crate::grid::ZERO_WIDTH_REPLACEMENT;

use crate::config::AmbiguousWidth;

use crate::config::Config;

use crate::editor::EditorState;

use crate::CellGrid;

use crate::Style;

use crate::geometry::text_width;

use crate::windows::CompositorLayer;

use crate::windows::ProjectedCompositorWindow;

use crate::windows::WindowRect;

use crate::render::CompositorLayoutState;

use crate::grid::Frame;

use crate::render::KeyHintUiState;

use crate::render::TerminalPanelGeometry;

use crate::render::TerminalPanelMode;

use crate::render::TerminalPanelState;

use crate::render::TerminalPhase;

use crate::render::TerminalTabState;

use crate::render::UiState;

pub const KEY_HINT_MAX_ROWS: u16 = 8;
pub const KEY_HINT_MAX_COLUMNS: u16 = 3;
pub const KEY_HINT_MAX_WIDTH: u16 = 96;
pub const KEY_HINT_PADDING: u16 = 1;
pub const KEY_HINT_COLUMN_GAP: u16 = 2;
pub const KEY_HINT_FALLBACK_LABEL: &str = "Action";
pub const TERMINAL_TAB_SEPARATOR: &str = " │ ";
pub const TERMINAL_NORMAL_LABEL: &str = "NORMAL";
pub const TERMINAL_VISUAL_LABEL: &str = "VISUAL";

pub(super) fn active_compositor<'a>(
    state: &EditorState,
    ui: &'a UiState,
    width: u16,
    height: u16,
) -> Option<&'a CompositorLayoutState> {
    let revision = state.window_revision()?;
    ui.compositor_layout.as_ref().filter(|compositor| {
        compositor.source_revision == revision
            && Some(compositor.graph_revision) == ui.window_graph_revision
            && compositor.projection.bounds.width == width
            && compositor.projection.bounds.height == height
    })
}

/// Where a terminal window draws. A docked terminal with something on its left, such as an
/// agent on the right, keeps its first column for a border.
pub fn terminal_window_geometry(
    window: ProjectedCompositorWindow,
) -> Option<TerminalPanelGeometry> {
    let rect = window.rect;
    let left_border = window.layer == CompositorLayer::Docked && rect.x > 0;
    let border_width = u16::from(left_border);
    (rect.width > border_width && rect.height >= 3).then_some(TerminalPanelGeometry {
        x: rect.x.saturating_add(border_width),
        width: rect.width.saturating_sub(border_width),
        left_border,
        top: rect.y,
        height: rect.height,
        body_top: rect.y.saturating_add(1),
        body_height: rect.height.saturating_sub(1),
    })
}

/// A tab's name: its profile, numbered when an earlier tab has the same one.
pub(super) fn terminal_tab_label(tabs: &[TerminalTabState], tab: &TerminalTabState) -> String {
    let earlier = tabs
        .iter()
        .take_while(|other| other.id != tab.id)
        .filter(|other| other.profile == tab.profile)
        .count();
    let mut title = if earlier == 0 {
        tab.profile.clone()
    } else {
        format!("{} {}", tab.profile, earlier + 1)
    };
    if let Some(status) = match tab.phase {
        TerminalPhase::Preflight => Some(" [preflight]"),
        TerminalPhase::Starting => Some(" [starting]"),
        TerminalPhase::Stopping => Some(" [stopping]"),
        TerminalPhase::Failed => Some(" [failed]"),
        TerminalPhase::Unknown => Some(" [unknown]"),
        _ => None,
    } {
        title.push_str(status);
    }
    if tab.phase == TerminalPhase::Exited {
        title.push_str(
            &tab.exit_code
                .map_or_else(|| " [signal]".into(), |code| format!(" [exit {code}]")),
        );
    }
    title
}

fn fitted_terminal_tab_label(
    terminal: &TerminalPanelState,
    tab: &TerminalTabState,
    config: &Config,
    width: u16,
) -> String {
    let ambiguous = config.editor.ambiguous_width;
    let count = terminal.tabs.len().clamp(1, 3);
    let separators = text_width(TERMINAL_TAB_SEPARATOR, ambiguous) * (count - 1);
    let available = usize::from(width.saturating_sub(1)).saturating_sub(separators) / count;
    let label = terminal_tab_label(&terminal.tabs, tab);
    let suffix = &label[tab.profile.len()..];
    let room = available.saturating_sub(text_width(suffix, ambiguous));
    let title = if tab.profile.contains('/') || tab.profile.contains('\\') {
        crate::render::overlays::truncate_content_title(&tab.profile, room, ambiguous)
    } else {
        crate::geometry::ellipsized(&tab.profile, room, ambiguous)
    };
    format!("{title}{suffix}")
}

// Drop leading tabs until the active title fits, keeping earlier tabs when there is room.
fn terminal_tab_start(terminal: &TerminalPanelState, config: &Config, width: u16) -> usize {
    let Some(active) = terminal.tabs.iter().position(|tab| tab.id == terminal.id) else {
        return 0;
    };
    let separator = text_width(TERMINAL_TAB_SEPARATOR, config.editor.ambiguous_width);
    let widths: Vec<_> = terminal.tabs[..=active]
        .iter()
        .map(|tab| {
            text_width(
                &fitted_terminal_tab_label(terminal, tab, config, width),
                config.editor.ambiguous_width,
            )
        })
        .collect();
    let mut used = widths.iter().sum::<usize>() + separator * active;
    let mut first = 0;
    while first < active && used > usize::from(width.saturating_sub(1)) {
        used = used.saturating_sub(widths[first] + separator);
        first += 1;
    }
    first
}

fn terminal_input_hint(terminal: &TerminalPanelState) -> Option<&str> {
    terminal
        .input_hint
        .as_deref()
        .filter(|_| terminal.focused && terminal.mode == TerminalPanelMode::Input)
}

fn terminal_title_width(terminal: &TerminalPanelState, config: &Config, width: u16) -> u16 {
    // An exit notice can only use space left by the tabs. Drawing and hit testing fit the
    // same titles whether the exit notice ends up in the header or in the footer.
    if terminal.exit_notice.is_some() {
        return width;
    }
    let hint = terminal_input_hint(terminal);
    let mode = match terminal.mode {
        TerminalPanelMode::Input => None,
        TerminalPanelMode::Normal => Some(TERMINAL_NORMAL_LABEL),
        TerminalPanelMode::Visual => Some(TERMINAL_VISUAL_LABEL),
    };
    let reserved = [hint, mode, terminal.truncated.then_some("output truncated")]
        .into_iter()
        .flatten()
        .map(|text| text_width(text, config.editor.ambiguous_width) + 2)
        .sum::<usize>();
    if reserved == 0 {
        return width;
    }
    let reserved = u16::try_from(reserved)
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    width
        .checked_sub(reserved)
        .filter(|&room| room > 8)
        .unwrap_or(width)
}

#[must_use]
pub fn terminal_tab_at_column(
    terminal: &TerminalPanelState,
    config: &Config,
    geometry: TerminalPanelGeometry,
    column: u16,
) -> Option<u64> {
    let title_width = terminal_title_width(terminal, config, geometry.width);
    let right = geometry.x.saturating_add(title_width);
    let mut x = geometry.x.saturating_add(1);
    let first = terminal_tab_start(terminal, config, title_width);
    for (index, tab) in terminal.tabs.iter().skip(first).enumerate() {
        if index > 0 {
            x = x.saturating_add(
                u16::try_from(text_width(
                    TERMINAL_TAB_SEPARATOR,
                    config.editor.ambiguous_width,
                ))
                .unwrap_or(u16::MAX),
            );
        }
        let label = fitted_terminal_tab_label(terminal, tab, config, title_width);
        let end = x
            .saturating_add(
                u16::try_from(text_width(&label, config.editor.ambiguous_width))
                    .unwrap_or(u16::MAX),
            )
            .min(right);
        if column >= x && column < end {
            return Some(tab.id);
        }
        x = end;
        if x >= right {
            break;
        }
    }
    None
}

pub(super) fn dock_terminal_panel(
    workspace: Frame,
    terminal: &TerminalPanelState,
    geometry: TerminalPanelGeometry,
    config: &Config,
    width: u16,
    height: u16,
) -> Frame {
    let mut grid = CellGrid::new_with_policy(
        width,
        height,
        CONTROL_CHARACTER_REPLACEMENT,
        ZERO_WIDTH_REPLACEMENT,
        config.editor.ambiguous_width == AmbiguousWidth::Wide,
    );
    for y in 0..workspace.grid.height().min(height) {
        for x in 0..workspace.grid.width().min(width) {
            let Some(cell) = workspace.grid.get(x, y) else {
                continue;
            };
            if !cell.continuation {
                let cell_width = if workspace
                    .grid
                    .get(x + 1, y)
                    .is_some_and(|next| next.continuation)
                {
                    2
                } else {
                    1
                };
                grid.set_cell_with_width(x, y, &cell.symbol, cell.style, cell_width);
            }
        }
    }
    let right = geometry.x.saturating_add(geometry.width).min(width);
    let bottom = geometry.top.saturating_add(geometry.height).min(height);
    for y in geometry.top..bottom {
        for x in geometry.x..right {
            grid.set_cell(x, y, " ", Style::TerminalBackground);
        }
    }
    let (horizontal, vertical, top_left, _, _, _) =
        grid.border_glyphs(crate::config::BorderStyle::Single);
    for x in geometry.x..right {
        grid.set_cell(
            x,
            geometry.top,
            &horizontal.to_string(),
            Style::TerminalBorder,
        );
    }
    if geometry.left_border {
        let x = geometry.x.saturating_sub(1);
        for y in geometry.top..bottom {
            let symbol = if y == geometry.top {
                top_left
            } else {
                vertical
            };
            grid.set_cell(x, y, &symbol.to_string(), Style::TerminalBorder);
        }
    }
    let title_width = terminal_title_width(terminal, config, geometry.width);
    let title_right = geometry.x.saturating_add(title_width).min(right);
    let mut title_x = geometry.x.saturating_add(1);
    let first = terminal_tab_start(terminal, config, title_width);
    for (index, tab) in terminal.tabs.iter().skip(first).enumerate() {
        if title_x >= title_right {
            break;
        }
        if index > 0 {
            title_x = grid.put_text_clipped(
                title_x,
                geometry.top,
                TERMINAL_TAB_SEPARATOR,
                Style::TerminalBorder,
                title_right,
            );
        }
        let title = fitted_terminal_tab_label(terminal, tab, config, title_width);
        title_x = grid.put_text_clipped(
            title_x,
            geometry.top,
            &title,
            if tab.id == terminal.id {
                Style::TerminalTitle
            } else {
                Style::Muted
            },
            title_right,
        );
    }
    // The status line names the mode; only the other modes are repeated at the end of the tab
    // row, unless the terminal covers the status line, as fullscreen does.
    let mode = match terminal.mode {
        TerminalPanelMode::Input => {
            (bottom >= height && terminal.exit_notice.is_none()).then_some("TERMINAL")
        }
        TerminalPanelMode::Normal => Some(TERMINAL_NORMAL_LABEL),
        TerminalPanelMode::Visual => Some(TERMINAL_VISUAL_LABEL),
    };
    let warning = terminal.truncated.then_some("output truncated");
    let mut put_header_notice = |hint: Option<&str>, mode: Option<&str>, warning: Option<&str>| {
        let notice = [hint, mode, warning]
            .into_iter()
            .flatten()
            .map(|text| format!(" {text} "))
            .collect::<String>();
        let notice_width =
            u16::try_from(text_width(&notice, config.editor.ambiguous_width)).unwrap_or(u16::MAX);
        let notice_x = right.saturating_sub(notice_width.saturating_add(1));
        if notice.is_empty() || notice_x <= title_x {
            return false;
        }
        grid.put_text_clipped(notice_x, geometry.top, &notice, Style::TerminalTitle, right);
        true
    };
    let mut footer = Vec::new();
    if let Some(exit_notice) = terminal.exit_notice.as_deref() {
        if !put_header_notice(Some(exit_notice), mode, warning) {
            let compact = match terminal.exit_code {
                None | Some(0) => "Exited · q closes".to_owned(),
                Some(code) => format!("Exited {code} · q closes"),
            };
            if !put_header_notice(Some(&compact), mode, warning) {
                let header_fits = put_header_notice(None, mode, warning)
                    || put_header_notice(None, None, warning);
                let body_rows = bottom.saturating_sub(geometry.body_top);
                // Leave a PTY row for the cursor even in a clipped dock.
                if body_rows > 1 {
                    if let Some(warning) = warning.filter(|_| !header_fits) {
                        let combined = format!("{compact} · {warning}");
                        if text_width(&combined, config.editor.ambiguous_width)
                            <= usize::from(right.saturating_sub(geometry.x))
                        {
                            footer.push(combined);
                        } else {
                            footer.push(warning.to_owned());
                            if body_rows > 2 {
                                footer.push(compact);
                            }
                        }
                    } else {
                        footer.push(compact);
                    }
                }
            }
        }
    } else {
        put_header_notice(terminal_input_hint(terminal), mode, warning);
    }

    let body_bottom = bottom - footer.len() as u16;
    let visible_rows = usize::from(
        geometry
            .body_height
            .min(body_bottom.saturating_sub(geometry.body_top)),
    );
    // Reuse empty trailing PTY rows before shifting the viewport. If it must shift, keep
    // the end of the output in view and still let navigation reach the first projected row.
    let skip = if footer.is_empty() {
        0
    } else {
        let blank_rows = terminal
            .cells
            .iter()
            .rev()
            .take(footer.len())
            .take_while(|row| {
                row.iter().all(|cell| {
                    cell.text == " " && !cell.continuation && cell.style == Default::default()
                })
            })
            .count();
        let tail = terminal
            .cells
            .len()
            .saturating_sub(visible_rows + blank_rows);
        match terminal.mode {
            TerminalPanelMode::Input => tail,
            TerminalPanelMode::Normal | TerminalPanelMode::Visual => {
                terminal.cursor.map_or(0, |(_, row)| {
                    let cursor = row.saturating_sub(terminal.cells_start);
                    // A blank row is only spare space if the navigation cursor can still
                    // be drawn. Scroll just enough to move the cursor above the footer.
                    let first_visible = cursor.saturating_sub(visible_rows.saturating_sub(1));
                    tail.clamp(first_visible, cursor)
                })
            }
        }
    };
    let source_start = terminal.cells_start.saturating_add(skip);
    for (screen_row, row) in terminal
        .cells
        .iter()
        .skip(skip)
        .take(visible_rows)
        .enumerate()
    {
        let source_row = source_start.saturating_add(screen_row);
        let y = geometry
            .body_top
            .saturating_add(u16::try_from(screen_row).unwrap_or(u16::MAX));
        for (column, cell) in row
            .iter()
            .take(usize::from(geometry.width.min(terminal.columns)))
            .enumerate()
        {
            if cell.continuation {
                continue;
            }
            let cell_width = if row.get(column + 1).is_some_and(|next| next.continuation) {
                2
            } else {
                1
            };
            let x = geometry
                .x
                .saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
            if x.saturating_add(cell_width as u16) > right {
                break;
            }
            let selected = terminal.selection.is_some_and(|(first, second, linewise)| {
                let (start, end) = if first <= second {
                    (first, second)
                } else {
                    (second, first)
                };
                if linewise {
                    (start.0..=end.0).contains(&source_row)
                } else {
                    (source_row, column) >= start && (source_row, column) <= end
                }
            });
            grid.set_cell_with_width(
                x,
                y,
                &cell.text,
                if selected {
                    Style::Selection
                } else {
                    Style::Terminal(cell.style)
                },
                cell_width,
            );
        }
    }

    for (row, notice) in (body_bottom..bottom).zip(footer) {
        let notice_width =
            u16::try_from(text_width(&notice, config.editor.ambiguous_width)).unwrap_or(u16::MAX);
        let x = right
            .saturating_sub(notice_width.saturating_add(1))
            .max(geometry.x);
        grid.put_text_clipped(x, row, &notice, Style::TerminalTitle, right);
    }

    let cursor = if terminal.focused {
        terminal.cursor.and_then(|(column, source_row)| {
            let projected_row = source_row.checked_sub(source_start)?;
            let y = geometry
                .body_top
                .saturating_add(u16::try_from(projected_row).ok()?);
            (column < usize::from(geometry.width) && y < body_bottom).then_some((
                geometry.x.saturating_add(u16::try_from(column).ok()?),
                y,
                crossterm::cursor::SetCursorStyle::SteadyBlock,
            ))
        })
    } else {
        workspace.cursor
    };
    Frame { grid, cursor }
}

pub fn render_key_hints(
    frame: &mut Frame,
    hints: &KeyHintUiState,
    bounds: WindowRect,
    ambiguous: AmbiguousWidth,
) {
    let editor_left = bounds.x;
    let editor_width = bounds.width;
    let available_height = bounds.height.saturating_sub(1);
    if editor_width < 12 || available_height < 4 || hints.rows.is_empty() {
        return;
    }
    let max_columns = usize::from(KEY_HINT_MAX_COLUMNS);
    let rows_with_header = KEY_HINT_MAX_ROWS.min(available_height.saturating_sub(3));
    // In a short workspace, put the title in the top border to make room for another row.
    let rows_before_compacting = hints
        .rows
        .len()
        .min(usize::from(rows_with_header) * max_columns);
    let compact_header =
        hints.rows.len() > rows_before_compacting && rows_with_header < KEY_HINT_MAX_ROWS;
    let title_rows = u16::from(!compact_header);
    let chrome_rows = 2 + title_rows;
    let maximum_rows =
        usize::from(KEY_HINT_MAX_ROWS.min(available_height.saturating_sub(chrome_rows)));
    let row_count = hints.rows.len().min(maximum_rows * max_columns);
    let columns = max_columns.min(row_count.div_ceil(maximum_rows)).max(1);
    let rows_per_column = row_count.div_ceil(columns);
    let desired_height = usize::from(chrome_rows).saturating_add(rows_per_column);
    let box_height = u16::try_from(desired_height)
        .unwrap_or(available_height)
        .min(available_height);
    if box_height < 4 {
        return;
    }
    let entry_width = |grid: &crate::CellGrid, row: &super::KeyHintUiRow| {
        grid.display_width(&row.next_key)
            + grid.display_width(&row.label)
            + row
                .icon
                .as_deref()
                .map_or(0, |icon| grid.display_width(icon) + 1)
            + 3
    };
    let widest = hints
        .rows
        .iter()
        // Extra entries can use the saved row, not increase the box's original width.
        .take(rows_before_compacting)
        .map(|row| entry_width(&frame.grid, row))
        .max()
        .unwrap_or(1);
    let title = format!("Keys: {}", hints.prefix);
    let horizontal_chrome = usize::from(KEY_HINT_PADDING) * 2 + 2;
    let desired_width = widest
        .saturating_mul(columns)
        .saturating_add(usize::from(KEY_HINT_COLUMN_GAP).saturating_mul(columns.saturating_sub(1)))
        .saturating_add(horizontal_chrome)
        .max(frame.grid.display_width(&title) + horizontal_chrome);
    let box_width = u16::try_from(desired_width)
        .unwrap_or(KEY_HINT_MAX_WIDTH)
        .min(KEY_HINT_MAX_WIDTH)
        .min(editor_width);
    if box_width < 12 {
        return;
    }
    let left = editor_left.saturating_add(editor_width.saturating_sub(box_width));
    let top = bounds
        .y
        .saturating_add(available_height.saturating_sub(box_height));
    let right = left.saturating_add(box_width);
    let bottom = top.saturating_add(box_height).saturating_sub(1);
    for y in top..=bottom {
        for x in left..right {
            frame.grid.set_cell(x, y, " ", Style::KeyHintBackground);
        }
    }
    let (horizontal, vertical, top_left, top_right, bottom_left, bottom_right) = frame
        .grid
        .border_glyphs(crate::config::BorderStyle::Rounded);
    for x in left.saturating_add(1)..right.saturating_sub(1) {
        frame
            .grid
            .set_cell(x, top, &horizontal.to_string(), Style::KeyHintBorder);
        frame
            .grid
            .set_cell(x, bottom, &horizontal.to_string(), Style::KeyHintBorder);
    }
    for y in top.saturating_add(1)..bottom {
        frame
            .grid
            .set_cell(left, y, &vertical.to_string(), Style::KeyHintBorder);
        frame.grid.set_cell(
            right.saturating_sub(1),
            y,
            &vertical.to_string(),
            Style::KeyHintBorder,
        );
    }
    frame
        .grid
        .set_cell(left, top, &top_left.to_string(), Style::KeyHintBorder);
    frame.grid.set_cell(
        right.saturating_sub(1),
        top,
        &top_right.to_string(),
        Style::KeyHintBorder,
    );
    frame
        .grid
        .set_cell(left, bottom, &bottom_left.to_string(), Style::KeyHintBorder);
    frame.grid.set_cell(
        right.saturating_sub(1),
        bottom,
        &bottom_right.to_string(),
        Style::KeyHintBorder,
    );
    let inner_left = left.saturating_add(1).saturating_add(KEY_HINT_PADDING);
    let inner_right = right.saturating_sub(1 + KEY_HINT_PADDING);
    if inner_left >= inner_right {
        return;
    }
    frame.grid.put_text_clipped(
        inner_left,
        top.saturating_add(title_rows),
        &title,
        Style::KeyHintPrefix,
        inner_right,
    );
    let body_top = top.saturating_add(1 + title_rows);
    let inner_width = usize::from(inner_right - inner_left);
    let mut gap = usize::from(KEY_HINT_COLUMN_GAP);
    let gap_count = columns - 1;
    let column_width = inner_width.saturating_sub(gap * gap_count) / columns;
    let mut widths = [column_width; KEY_HINT_MAX_COLUMNS as usize];
    if compact_header && gap_count > 0 {
        let mut preferred = [0; KEY_HINT_MAX_COLUMNS as usize];
        for (width, rows) in preferred
            .iter_mut()
            .zip(hints.rows[..row_count].chunks(rows_per_column))
        {
            *width = rows
                .iter()
                .map(|row| entry_width(&frame.grid, row))
                .max()
                .unwrap_or(0);
        }
        let total = preferred.iter().sum::<usize>();
        // Fit wider entries from the extra row by sharing the existing space.
        if preferred.iter().any(|&width| width > column_width) && total + gap_count <= inner_width {
            gap = gap.min((inner_width - total) / gap_count);
            widths = preferred;
        }
    }
    if widths[0] < 3 {
        return;
    }
    for (index, row) in hints.rows.iter().take(row_count).enumerate() {
        let column = index / rows_per_column;
        let row_index = index % rows_per_column;
        let offset = widths[..column].iter().sum::<usize>() + column * gap;
        let x = inner_left.saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
        let y = body_top.saturating_add(u16::try_from(row_index).unwrap_or(u16::MAX));
        if y >= bottom {
            continue;
        }
        let column_right = x
            .saturating_add(u16::try_from(widths[column]).unwrap_or(u16::MAX))
            .min(inner_right);
        let mut cursor = frame.grid.put_text_clipped(
            x,
            y,
            &format!("[{}]", row.next_key),
            Style::KeyHintKey,
            column_right,
        );
        if let Some(icon) = &row.icon {
            cursor = frame.grid.put_text_clipped(
                cursor.saturating_add(1),
                y,
                icon,
                Style::KeyHintGroup,
                column_right,
            );
        }
        // A label too long for its column ends in an ellipsis instead of stopping mid-word.
        let room = usize::from(column_right.saturating_sub(cursor.saturating_add(1)));
        let label = crate::geometry::ellipsized(&row.label, room, ambiguous);
        frame.grid.put_text_clipped(
            cursor.saturating_add(1),
            y,
            &label,
            if row.group {
                Style::KeyHintGroup
            } else {
                Style::KeyHintCommand
            },
            column_right,
        );
    }
}
