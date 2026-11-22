//! Explorer tree rows and viewport drawing.

use crate::CellGrid;
use crate::Style;
use crate::config::AmbiguousWidth;
use crate::config::Config;
use crate::geometry::{cell_text_width, sanitize_file_name, text_width};
use crate::grid::CONTROL_CHARACTER_REPLACEMENT;
use crate::grid::ZERO_WIDTH_REPLACEMENT;
use crate::windows::PANE_MINIMUM_WIDTH;
use crate::windows::WindowRect;

use crate::render::ExplorerSegmentRole;

use crate::render::{ExplorerUiRow, ExplorerUiSegment, ExplorerUiState};
use std::borrow::Cow;
use std::path::Path;
use unicode_segmentation::UnicodeSegmentation;

use crate::grid::Frame;

use crate::config::Highlight;
use crate::render::editor::cursor_style;

pub const EXPLORER_MAXIMUM_VISIBLE_ROWS: usize = 200;
pub const EXPLORER_MINIMUM_WIDTH: u16 = 12;
pub const EXPLORER_MAXIMUM_WIDTH: u16 = 60;
pub const EXPLORER_FOCUS_MARKER: &str = ">";
pub const EXPLORER_LOADING_LABEL: &str = "loading…";

/// Width of the persistent explorer dock, leaving a usable editor pane.
#[must_use]
pub fn explorer_panel_width(config: &Config, terminal_width: u16) -> Option<u16> {
    let separator = 1;
    let minimum_editor = PANE_MINIMUM_WIDTH.max(20);
    if terminal_width < 3 {
        return None;
    }
    let maximum_panel = terminal_width.saturating_sub(minimum_editor.saturating_add(separator));
    if maximum_panel < 12 {
        return Some(terminal_width.saturating_sub(separator) / 2);
    }
    Some(
        config
            .ui
            .explorer
            .width
            .clamp(EXPLORER_MINIMUM_WIDTH, EXPLORER_MAXIMUM_WIDTH)
            .min(maximum_panel),
    )
}

pub(super) fn place_workspace_frame(
    content: Frame,
    bounds: WindowRect,
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
    for y in 0..content.grid.height().min(bounds.height) {
        for x in 0..content.grid.width().min(bounds.width) {
            if let Some(cell) = content.grid.get(x, y)
                && !cell.continuation
            {
                grid.set_cell(
                    bounds.x.saturating_add(x),
                    bounds.y.saturating_add(y),
                    &cell.symbol,
                    cell.style,
                );
            }
        }
    }
    let cursor = content.cursor.and_then(|(x, y, shape)| {
        (x < bounds.width && y < bounds.height).then_some((
            bounds.x.saturating_add(x),
            bounds.y.saturating_add(y),
            shape,
        ))
    });
    Frame { grid, cursor }
}

pub(super) fn render_explorer_window(
    frame: &mut Frame,
    explorer: &ExplorerUiState,
    config: &Config,
    rect: WindowRect,
    focused: bool,
) {
    if rect.width < 2 || rect.height == 0 {
        return;
    }
    let mut local = CellGrid::new_with_policy(
        rect.width,
        rect.height,
        CONTROL_CHARACTER_REPLACEMENT,
        ZERO_WIDTH_REPLACEMENT,
        config.editor.ambiguous_width == AmbiguousWidth::Wide,
    );
    let panel_width = rect.width.saturating_sub(1);
    let body = crate::render::explorer::explorer_body_geometry(rect);
    let local_body = WindowRect {
        x: 0,
        y: body.y.saturating_sub(rect.y),
        width: body.width,
        height: body.height,
    };
    let search_cursor = render_explorer_dock(
        &mut local,
        explorer,
        config,
        panel_width,
        rect.height,
        local_body,
    );
    for y in 0..rect.height {
        for x in 0..rect.width {
            if let Some(cell) = local.get(x, y)
                && !cell.continuation
            {
                frame.grid.set_cell(
                    rect.x.saturating_add(x),
                    rect.y.saturating_add(y),
                    &cell.symbol,
                    cell.style,
                );
            }
        }
    }
    if focused && explorer.searching {
        // Typing goes to the search box.
        frame.cursor = search_cursor.map(|x| {
            (
                rect.x.saturating_add(x),
                rect.y.saturating_add(1),
                cursor_style(config.ui.cursor.insert),
            )
        });
    } else if focused {
        frame.cursor = explorer
            .rows
            .iter()
            .position(|row| row.selected)
            .and_then(|row| u16::try_from(row).ok())
            .map(|row| {
                (
                    // The margin left of the tree, so the cursor doesn't cover its lines.
                    rect.x
                        + if panel_width <= NARROW_EXPLORER_WIDTH {
                            panel_width
                        } else {
                            0
                        },
                    body.y.saturating_add(row),
                    cursor_style(config.ui.cursor.normal),
                )
            })
            .filter(|(_, y, _)| *y < body.y.saturating_add(body.height));
    }
}

pub(super) fn render_explorer_dock(
    grid: &mut CellGrid,
    explorer: &ExplorerUiState,
    config: &Config,
    panel_width: u16,
    height: u16,
    body: WindowRect,
) -> Option<u16> {
    if panel_width == 0 || height == 0 {
        return None;
    }
    let panel_height = height;
    let border = if config.ui.explorer.icon_mode == crate::config::ExplorerIconMode::Ascii
        || grid.display_width("│") != 1
    {
        "|"
    } else {
        "│"
    };
    for y in 0..panel_height {
        for x in 0..panel_width {
            grid.set_cell(x, y, " ", Style::ExplorerBackground);
        }
        grid.set_cell(panel_width, y, border, Style::ExplorerBorder);
    }

    let search_cursor = if body.y >= 3 {
        render_explorer_search_box(grid, explorer, config, panel_width)
    } else {
        None
    };

    let body_top = body.y;
    let available_rows = usize::from(body.height);
    let (content_left, content_right) = explorer_row_columns(panel_width);
    let rows = &explorer.rows[..explorer.rows.len().min(available_rows)];
    let ambiguous = config.editor.ambiguous_width;
    let width = |text: &str| cell_text_width(text, ambiguous);
    let glyphs = config.ui.explorer.active_glyphs();
    let guide_width = [
        &glyphs.tree_vertical,
        &glyphs.tree_middle,
        &glyphs.tree_last,
        &glyphs.tree_space,
    ]
    .into_iter()
    .map(|glyph| width(glyph))
    .max()
    .unwrap_or(2) as u16;
    let right_column = ExplorerRightColumn::new(rows, content_left, content_right, ambiguous);
    let selected = rows.iter().find(|row| row.selected);
    let shift = selected.map_or(0, |row| {
        let Some((name, prefix)) = row.segments.split_last() else {
            return 0;
        };
        let desired = width(&sanitize_file_name(&name.text)).min(24);
        let prefix_width: usize = prefix.iter().map(|segment| width(&segment.text)).sum();
        let room = usize::from(
            right_column
                .layout(row)
                .name_right
                .saturating_sub(content_left),
        );
        (0..row.guides.len())
            .find(|&shift| {
                let marker = if shift == 0 { 0 } else { width("…") };
                marker
                    + (row.guides.len() - shift) * usize::from(guide_width)
                    + prefix_width
                    + desired
                    <= room
            })
            .unwrap_or(row.guides.len())
    });
    let mut footer_path = None;
    for (screen_row, row) in rows.iter().enumerate() {
        let y = body_top.saturating_add(u16::try_from(screen_row).unwrap_or(0));
        if row.selected {
            for x in 0..panel_width {
                grid.set_cell(x, y, " ", Style::ExplorerSelection);
            }
        }
        let style = |role| {
            if row.selected {
                Style::ExplorerSelected(role)
            } else {
                explorer_segment_style(role)
            }
        };
        let ExplorerRowLayout {
            segments: right_segments,
            status_left: mut right_x,
            name_right,
            using_counts,
        } = right_column.layout(row);
        let mut x = content_left;
        if shift != 0 && !row.guides.is_empty() {
            x = grid.put_text_clipped(x, y, "…", style(ExplorerSegmentRole::Tree), name_right);
        }
        if let Some((name, prefix)) = row.segments.split_last() {
            let name_text = sanitize_file_name(&name.text);
            let name_width = width(&name_text);
            let minimum_name = if using_counts {
                MINIMUM_NAME_WITH_COUNTS
            } else {
                3 + width("…")
            };
            let minimum_name = name_width.min(minimum_name) as u16;
            let prefix_width: usize = prefix.iter().map(|segment| width(&segment.text)).sum();
            let prefix_width = u16::try_from(prefix_width).unwrap_or(u16::MAX);
            // Give an overflowing row's icon and name priority over any remaining guides.
            let guides_right = name_right.saturating_sub(minimum_name.saturating_add(prefix_width));
            for guide in row.guides.iter().skip(shift) {
                let next = x.saturating_add(guide_width);
                if next > guides_right {
                    break;
                }
                grid.put_text_clipped(x, y, guide, style(ExplorerSegmentRole::Tree), next);
                x = next;
            }
            // On very narrow rows the icon can use its space, but not its optional padding.
            let prefix_right = name_right.saturating_sub(minimum_name);
            for segment in prefix {
                x = grid.put_text_clipped(x, y, &segment.text, style(segment.role), prefix_right);
            }
            let name_room = usize::from(name_right.saturating_sub(x));
            let text = truncate_explorer_name(&name_text, name_room, ambiguous);
            grid.put_text_clipped(x, y, &text, style(name.role), name_right);
            if row.selected && (shift != 0 || name_width > name_room) {
                footer_path = row.relative_path.as_deref();
            }
        }
        for segment in right_segments {
            right_x = grid.put_text_clipped(
                right_x,
                y,
                &segment.text,
                style(segment.role),
                content_right,
            );
        }
    }

    if explorer.rows.is_empty() && !explorer.loading && available_rows > 0 {
        grid.put_text_clipped(
            1,
            body_top,
            if explorer.query.is_empty() {
                "Empty folder"
            } else {
                "No matches"
            },
            Style::ExplorerCount,
            panel_width.saturating_sub(1),
        );
    }
    if explorer.loading && rows.len() < available_rows {
        let y = body_top.saturating_add(u16::try_from(rows.len()).unwrap_or(0));
        grid.put_text_clipped(
            1,
            y,
            EXPLORER_LOADING_LABEL,
            Style::ExplorerCount,
            panel_width.saturating_sub(1),
        );
    }
    if height > EXPLORER_SEARCH_ROWS {
        render_explorer_footer(
            grid,
            footer_path,
            panel_width,
            height - 1,
            border == "|",
            ambiguous,
        );
    }
    search_cursor
}

const NARROW_EXPLORER_WIDTH: u16 = 12;

/// Keep the usual margins unless they would take space from the icon and file name.
fn explorer_row_columns(panel_width: u16) -> (u16, u16) {
    if panel_width <= NARROW_EXPLORER_WIDTH {
        (0, panel_width)
    } else {
        (1, panel_width - 1)
    }
}

const MINIMUM_NAME_WITH_COUNTS: usize = 8;

struct ExplorerRightColumn {
    left: u16,
    right: u16,
    ambiguous: AmbiguousWidth,
    count_width: usize,
    two_letter_column: bool,
}

struct ExplorerRowLayout<'a> {
    segments: &'a [ExplorerUiSegment],
    status_left: u16,
    name_right: u16,
    using_counts: bool,
}

fn has_two_git_letters(segments: &[ExplorerUiSegment]) -> bool {
    matches!(
        segments,
        [
            ExplorerUiSegment {
                role: ExplorerSegmentRole::GitStaged,
                ..
            },
            _
        ]
    )
}

impl ExplorerRightColumn {
    fn new(rows: &[ExplorerUiRow], left: u16, right: u16, ambiguous: AmbiguousWidth) -> Self {
        let mut column = Self {
            left,
            right,
            ambiguous,
            count_width: 0,
            // The narrow layout drops the usual margins and keeps only the worktree letter.
            two_letter_column: left != 0
                && rows
                    .iter()
                    .any(|row| has_two_git_letters(&row.right_segments)),
        };
        column.count_width = rows
            .iter()
            .filter(|row| !row.git_line_count_segments.is_empty())
            .filter_map(|row| {
                let width = column.width(&row.git_line_count_segments);
                (width <= column.count_room(row)).then_some(width)
            })
            .max()
            .unwrap_or(0);
        column
    }

    fn width(&self, segments: &[ExplorerUiSegment]) -> usize {
        segments
            .iter()
            .map(|segment| cell_text_width(&segment.text, self.ambiguous))
            .sum()
    }

    fn count_room(&self, row: &ExplorerUiRow) -> usize {
        let prefix = row
            .segments
            .split_last()
            .map_or(0, |(_, prefix)| self.width(prefix));
        // Leave space for the whole-level shift marker when the selected row needs a shift.
        let marker = if row.guides.is_empty() {
            0
        } else {
            cell_text_width("…", self.ambiguous)
        };
        usize::from(self.right.saturating_sub(self.left))
            .saturating_sub(prefix + marker + MINIMUM_NAME_WITH_COUNTS + 1)
    }

    fn layout<'a>(&self, row: &'a ExplorerUiRow) -> ExplorerRowLayout<'a> {
        let shared = self.count_width != 0 && self.count_width <= self.count_room(row);
        let using_counts = shared
            && !row.git_line_count_segments.is_empty()
            && self.width(&row.git_line_count_segments) <= self.count_width;
        let segments = if using_counts {
            &row.git_line_count_segments[..]
        } else if !self.two_letter_column && has_two_git_letters(&row.right_segments) {
            &row.right_segments[1..]
        } else {
            &row.right_segments[..]
        };
        let status_width = self.width(segments);
        let reserved = status_width
            .max(if shared { self.count_width } else { 0 })
            .max(if self.two_letter_column { 2 } else { 0 });
        let status_left = self
            .right
            .saturating_sub(u16::try_from(status_width).unwrap_or(u16::MAX))
            .max(self.left);
        let name_right = self
            .right
            .saturating_sub(u16::try_from(reserved).unwrap_or(u16::MAX))
            .saturating_sub(u16::from(reserved != 0));
        ExplorerRowLayout {
            segments,
            status_left,
            name_right,
            using_counts,
        }
    }
}

fn truncate_explorer_name(name: &str, maximum: usize, ambiguous: AmbiguousWidth) -> Cow<'_, str> {
    let width = |text: &str| cell_text_width(text, ambiguous);
    if width(name) <= maximum {
        return Cow::Borrowed(name);
    }
    let ellipsis = "…";
    let Some(available) = maximum.checked_sub(width(ellipsis)) else {
        return Cow::Borrowed("");
    };
    let first = name.graphemes(true).next().map_or(0, width);
    let extension = Path::new(name).extension().and_then(|ext| ext.to_str());
    let suffix_limit = available.saturating_sub(first);
    let extension_width = extension
        .map(|ext| width(ext) + 1)
        .filter(|&width| width <= suffix_limit)
        .unwrap_or(0);
    let suffix_room = (available * 2 / 5).max(extension_width).min(suffix_limit);
    let mut suffix_start = name.len();
    let mut suffix_width = 0;
    for (index, grapheme) in name.grapheme_indices(true).rev() {
        let next = suffix_width + width(grapheme);
        if next > suffix_room {
            break;
        }
        suffix_start = index;
        suffix_width = next;
    }
    let mut prefix_end = 0;
    let mut prefix_width = 0;
    for (index, grapheme) in name[..suffix_start].grapheme_indices(true) {
        prefix_width += width(grapheme);
        if prefix_width > available - suffix_width {
            break;
        }
        prefix_end = index + grapheme.len();
    }
    Cow::Owned(format!(
        "{0}{ellipsis}{1}",
        &name[..prefix_end],
        &name[suffix_start..]
    ))
}

fn render_explorer_footer(
    grid: &mut CellGrid,
    path: Option<&str>,
    panel_width: u16,
    y: u16,
    ascii: bool,
    ambiguous: AmbiguousWidth,
) {
    let (horizontal, left, right) = if ascii {
        ("-", "+", "+")
    } else {
        ("─", "╰", "╯")
    };
    grid.set_cell(0, y, left, Style::ExplorerBorder);
    for x in 1..panel_width {
        grid.set_cell(x, y, horizontal, Style::ExplorerBorder);
    }
    grid.set_cell(panel_width, y, right, Style::ExplorerBorder);
    if let Some(path) = path {
        let right = panel_width.saturating_sub(1);
        let path = sanitize_file_name(path);
        let title = crate::render::overlays::truncate_content_title(
            &path,
            usize::from(right.saturating_sub(2)),
            ambiguous,
        );
        if !title.is_empty() {
            grid.put_text_clipped(
                1,
                y,
                &format!(" {title} "),
                Style::ExplorerTree,
                panel_width,
            );
        }
    }
}

/// The titled box at the top of the panel: the prompt, the query and how many rows match.
fn render_explorer_search_box(
    grid: &mut CellGrid,
    explorer: &ExplorerUiState,
    config: &Config,
    panel_width: u16,
) -> Option<u16> {
    if panel_width < 4 {
        return None;
    }
    let (horizontal, vertical, top_left, top_right, bottom_left, bottom_right) =
        if grid.display_width("─") == 1 {
            crate::config::BorderStyle::Rounded.glyphs()
        } else {
            ('-', '|', '+', '+', '+', '+')
        };
    let last = panel_width - 1;
    for x in 1..last {
        grid.set_cell(x, 0, &horizontal.to_string(), Style::ExplorerBorder);
        grid.set_cell(x, 1, " ", Style::ExplorerHeader);
        grid.set_cell(x, 2, &horizontal.to_string(), Style::ExplorerBorder);
    }
    for (x, top, bottom) in [(0, top_left, bottom_left), (last, top_right, bottom_right)] {
        grid.set_cell(x, 0, &top.to_string(), Style::ExplorerBorder);
        grid.set_cell(x, 1, &vertical.to_string(), Style::ExplorerBorder);
        grid.set_cell(x, 2, &bottom.to_string(), Style::ExplorerBorder);
    }
    let title = format!(" {} ", explorer.title);
    let title_width =
        u16::try_from(text_width(&title, config.editor.ambiguous_width)).unwrap_or(last);
    grid.put_text_clipped(
        panel_width.saturating_sub(title_width) / 2,
        0,
        &title,
        Style::ExplorerHeader,
        last,
    );
    // How many entries are listed, or how many match while a search filters the tree.
    let count = if explorer.query.is_empty() {
        format!("{0}/{0}", explorer.total)
    } else {
        explorer.matches.to_string()
    };
    let count_width =
        u16::try_from(text_width(&count, config.editor.ambiguous_width)).unwrap_or(last);
    let count_x = last.saturating_sub(count_width).saturating_sub(1);
    grid.put_text_clipped(count_x, 1, &count, Style::ExplorerCount, last);
    let x = grid.put_text_clipped(2, 1, EXPLORER_FOCUS_MARKER, Style::ExplorerHeader, count_x);
    let right = count_x.saturating_sub(1);
    let start = x.saturating_add(1).min(right);
    let (query, cursor) = crate::render::prompt::prompt_viewport(
        &explorer.query,
        explorer.query.len(),
        usize::from(right.saturating_sub(start)),
        grid,
    );
    grid.put_text_clipped(start, 1, query, Style::ExplorerHeader, right);
    Some(start.saturating_add(u16::try_from(cursor).unwrap_or(0)))
}

pub fn explorer_segment_style(role: ExplorerSegmentRole) -> Style {
    match role {
        ExplorerSegmentRole::Tree => Style::ExplorerTree,
        ExplorerSegmentRole::Icon | ExplorerSegmentRole::IconGrey => Style::ExplorerIcon,
        ExplorerSegmentRole::IconAzure => Style::Accent,
        ExplorerSegmentRole::IconBlue => Style::Syntax(Highlight::FUNCTION),
        ExplorerSegmentRole::IconCyan => Style::Syntax(Highlight::PROPERTY),
        ExplorerSegmentRole::IconGreen => Style::GitSignAdded,
        ExplorerSegmentRole::IconOrange => Style::Syntax(Highlight::NUMBER),
        ExplorerSegmentRole::IconPurple => Style::ExplorerSymlink,
        ExplorerSegmentRole::IconRed => Style::GitSignDeleted,
        ExplorerSegmentRole::IconYellow => Style::GitSignChanged,
        ExplorerSegmentRole::Directory => Style::ExplorerDirectory,
        ExplorerSegmentRole::File => Style::ExplorerFile,
        ExplorerSegmentRole::Symlink => Style::ExplorerSymlink,
        ExplorerSegmentRole::Loading => Style::ExplorerDecoration,
        ExplorerSegmentRole::Git => Style::ExplorerGit,
        ExplorerSegmentRole::GitModified => Style::ExplorerGitModified,
        ExplorerSegmentRole::GitAdded => Style::ExplorerGitAdded,
        ExplorerSegmentRole::GitRenamed => Style::ExplorerGitRenamed,
        ExplorerSegmentRole::GitConflict => Style::ExplorerGitConflict,
        ExplorerSegmentRole::GitStaged => Style::ExplorerGitStaged,
        ExplorerSegmentRole::GitDim => Style::ExplorerGitDim,
        ExplorerSegmentRole::GitLineCount { added } => Style::ExplorerGitLineCount { added },
        ExplorerSegmentRole::Diagnostic => Style::ExplorerDiagnostic,
    }
}

/// The box around the explorer's search row.
const EXPLORER_SEARCH_ROWS: u16 = 3;

#[must_use]
pub fn explorer_body_geometry(rect: WindowRect) -> WindowRect {
    WindowRect {
        x: rect.x,
        y: rect
            .y
            .saturating_add(EXPLORER_SEARCH_ROWS)
            .min(rect.y.saturating_add(rect.height)),
        width: rect.width.saturating_sub(1),
        height: rect
            .height
            .saturating_sub(EXPLORER_SEARCH_ROWS + 1)
            .min(u16::try_from(EXPLORER_MAXIMUM_VISIBLE_ROWS).unwrap_or(u16::MAX)),
    }
}

#[cfg(test)]
mod tests {
    use crate::render::explorer::*;

    fn explorer(query: &str) -> ExplorerUiState {
        ExplorerUiState {
            title: "Explorer".into(),
            rows: Vec::new(),
            scroll: 0,
            loading: false,
            total: 1,
            matches: 1,
            query: query.into(),
            searching: true,
        }
    }

    fn render_search(query: &str, ambiguous: AmbiguousWidth) -> Frame {
        let mut config = Config::default();
        config.editor.ambiguous_width = ambiguous;
        config.ui.explorer.icon_mode = crate::config::ExplorerIconMode::Unicode;
        let mut frame = Frame {
            grid: CellGrid::new_with_policy(
                29,
                8,
                CONTROL_CHARACTER_REPLACEMENT,
                ZERO_WIDTH_REPLACEMENT,
                ambiguous == AmbiguousWidth::Wide,
            ),
            cursor: None,
        };
        render_explorer_window(
            &mut frame,
            &explorer(query),
            &config,
            WindowRect {
                x: 0,
                y: 0,
                width: 29,
                height: 8,
            },
            true,
        );
        frame
    }

    #[test]
    fn empty_explorer_explains_empty_folders_and_searches() {
        for (query, hint) in [("", "Empty folder"), ("missing", "No matches")] {
            let frame = render_search(query, AmbiguousWidth::Narrow);
            assert!(frame.grid.row_text(3).contains(hint));
        }
    }

    #[test]
    fn wide_explorer_separator_fits_the_final_column() {
        let frame = render_search("", AmbiguousWidth::Wide);
        for y in 0..7 {
            let cell = frame.grid.get(28, y).unwrap();
            assert_eq!(cell.symbol, "|");
            assert!(!cell.continuation);
        }
        assert_eq!(frame.grid.get(28, 7).unwrap().symbol, "+");
    }

    #[test]
    fn wide_search_box_borders_stay_within_the_panel() {
        let frame = render_search("", AmbiguousWidth::Wide);
        for y in [0, 2] {
            assert_eq!(frame.grid.get(0, y).unwrap().symbol, "+");
            assert_eq!(frame.grid.get(27, y).unwrap().symbol, "+");
        }
        assert_eq!(frame.grid.row_text(2), format!("+{}+|", "-".repeat(26)));
        assert_eq!(frame.grid.get(27, 1).unwrap().symbol, "|");
        let narrow = render_search("", AmbiguousWidth::Narrow);
        assert_eq!(narrow.grid.get(0, 0).unwrap().symbol, "╭");
        assert_eq!(narrow.grid.get(28, 1).unwrap().symbol, "│");
    }

    #[test]
    fn search_query_scrolls_with_the_cursor_before_the_count() {
        for ambiguous in [AmbiguousWidth::Narrow, AmbiguousWidth::Wide] {
            for tail in ["z", "界", "e\u{301}", "·"] {
                let query = format!("{}{tail}", "a".repeat(40));
                let frame = render_search(&query, ambiguous);
                let line = frame.grid.row_text(1);
                assert!(line.contains(tail), "{line}");
                let (x, y, _) = frame.cursor.unwrap();
                assert_eq!(y, 1);
                assert!(x < 25, "{x}: {line}");
                assert_eq!(frame.grid.get(x, y).unwrap().symbol, " ");
                assert_eq!(frame.grid.get(25, y).unwrap().symbol, "1");
                assert_eq!(
                    frame.grid.get(x - 1, y).unwrap().style,
                    Style::ExplorerHeader
                );
            }
        }
    }
}
