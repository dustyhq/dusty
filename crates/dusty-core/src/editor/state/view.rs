//! Cursor, selection and viewport state.

use crate::editor::state::id::BufferId;
use crate::editor::state::id::ViewId;
use crate::editor::text::Affinity;
use crate::editor::text::ByteOffset;
use crate::editor::text::ByteRange;
use crate::editor::text::EditDelta;
use crate::editor::text::TextBuffer;
use crate::editor::text::TextVersion;
use crate::editor::text::previous_grapheme_boundary;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum EditorMode {
    #[default]
    Normal,
    Insert,
    Replace,
    VisualChar,
    VisualLine,
    VisualBlock,
    /// Several cursors marked, waiting for what to do at them.
    Multi,
    OperatorPending,
    CommandLine,
    Prompt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionKind {
    Character,
    Line,
    Block,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub anchor: ByteOffset,
    pub head: ByteOffset,
    pub kind: SelectionKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaceRecord {
    pub start: ByteOffset,
    pub original: String,
    pub inserted: String,
}

#[derive(Clone, Debug)]
pub struct InsertRepeat {
    pub start: ByteOffset,
    pub count: usize,
    pub new_line: bool,
}

/// The cell a screen-row motion aims for, even across shorter rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScreenColumn {
    Cell(usize),
    EndOfRow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScreenDirection {
    Up,
    Down,
}

impl ScreenDirection {
    #[must_use]
    pub fn from_motion(motion: &str) -> Option<Self> {
        match motion {
            "screen_up" => Some(Self::Up),
            "screen_down" => Some(Self::Down),
            _ => None,
        }
    }

    #[must_use]
    pub const fn file_motion(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// The app projects a motion through the target pane's current layout.
#[derive(Clone, Copy, Debug)]
pub struct ScreenMotionProjection {
    pub direction: ScreenDirection,
    pub count: usize,
    pub target: ByteOffset,
    pub column: ScreenColumn,
}

/// A view's collapsed source lines, shared by motions and visual-row layout.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FoldedLines(Vec<(usize, usize)>);

impl FoldedLines {
    #[must_use]
    pub fn new(ranges: impl Iterator<Item = (usize, usize)>, last: usize) -> Self {
        let mut ranges: Vec<_> = ranges
            .map(|(start, end)| (start.min(last), end.min(last)))
            .filter(|&(start, end)| start < end)
            .collect();
        ranges.sort_unstable_by_key(|&(start, end)| (start, std::cmp::Reverse(end)));
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            if let Some(previous) = merged.last_mut()
                && start <= previous.1
            {
                previous.1 = previous.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        Self(merged)
    }

    #[must_use]
    pub fn containing(&self, line: usize) -> Option<(usize, usize)> {
        let index = self.0.partition_point(|&(start, _)| start < line);
        self.0[..index]
            .last()
            .copied()
            .filter(|&(_, end)| line <= end)
    }

    #[must_use]
    pub fn header(&self, line: usize) -> usize {
        self.containing(line).map_or(line, |(start, _)| start)
    }

    #[must_use]
    pub fn end(&self, line: usize) -> usize {
        self.0
            .get(self.0.partition_point(|&(start, _)| start < line))
            .filter(|&&(start, _)| start == line)
            .map_or(line, |&(_, end)| end)
    }

    #[must_use]
    pub fn forward(&self, line: usize, count: usize, last: usize) -> usize {
        let line = self.header(line.min(last));
        let last = self.header(last);
        let mut target = line.saturating_add(count).min(last);
        for &(start, end) in &self.0[self.0.partition_point(|&(_, end)| end < line)..] {
            if start >= target {
                break;
            }
            target = target.saturating_add(end - start).min(last);
        }
        target
    }

    /// How many visible lines apart two visible lines are, as `3j` counts them.
    #[must_use]
    pub fn distance(&self, from: usize, to: usize) -> usize {
        let (low, high) = (from.min(to), from.max(to));
        let hidden: usize = self
            .0
            .iter()
            .filter(|&&(start, end)| start >= low && end < high)
            .map(|&(start, end)| end - start)
            .sum();
        (high - low).saturating_sub(hidden)
    }

    #[must_use]
    pub fn backward(&self, line: usize, count: usize) -> usize {
        let line = self.header(line);
        let mut target = line.saturating_sub(count);
        let before = self.0.partition_point(|&(start, _)| start < line);
        for &(start, end) in self.0[..before].iter().rev() {
            if end < target {
                break;
            }
            target = target.saturating_sub(end - start);
        }
        target
    }
}

/// Block append text to copy on Escape after live synchronized editing has ended.
#[derive(Clone, Debug)]
pub(crate) struct DeferredBlockAppend {
    pub positions: Vec<ByteOffset>,
    pub column: usize,
    pub start: ByteOffset,
}

#[derive(Clone, Debug)]
struct MultiInsertSession {
    view: ViewId,
    buffer: BufferId,
    source: TextVersion,
}

#[derive(Clone, Debug)]
pub struct View {
    pub buffer: BufferId,
    pub cursor: ByteOffset,
    pub selection: Option<Selection>,
    /// Previous cursor and selection for incremental syntax selection, newest last.
    pub syntax_selections: Vec<(ByteOffset, Option<Selection>)>,
    pub scroll_line: usize,
    /// Wrapped visual row within `scroll_line`; runtime viewport state only.
    pub scroll_wrap_row: usize,
    pub scroll_column: usize,
    pub preferred_column: Option<usize>,
    pub preferred_screen_column: Option<ScreenColumn>,
    /// Available only while the app dispatches the command.
    pub screen_motion: Option<ScreenMotionProjection>,
    /// Runtime motion projection supplied by the app before command dispatch.
    pub folded_lines: FoldedLines,
    pub mode: EditorMode,
    pub replace_history: Vec<ReplaceRecord>,
    pub insert_repeat: Option<InsertRepeat>,
    pub block_insert: Vec<ByteOffset>,
    /// Virtual column to reach on the first text insertion for Visual Block A.
    pub(crate) block_append_column: Option<usize>,
    /// Physical insertion sites before any append text or generated padding.
    pub(crate) block_append_origins: Vec<ByteOffset>,
    pub(crate) deferred_block_append: Option<DeferredBlockAppend>,
    multi_insert_session: Option<MultiInsertSession>,
    /// The ranges marked in `EditorMode::Multi`, in order. Empty ranges are plain cursors.
    pub multi_marks: Vec<crate::editor::text::ByteRange>,
    pub multi_whole_word: bool,
    /// Wanted display columns, in mark order; empty after nonvertical movement.
    pub multi_preferred_columns: Vec<usize>,
}

impl View {
    pub(crate) fn projected_screen_motion(
        &self,
        direction: ScreenDirection,
        count: usize,
    ) -> Option<ScreenMotionProjection> {
        self.screen_motion
            .filter(|projection| projection.direction == direction && projection.count == count)
    }

    pub(crate) fn begin_multi_insert(&mut self, view: ViewId, source: TextVersion) {
        self.multi_insert_session = Some(MultiInsertSession {
            view,
            buffer: self.buffer,
            source,
        });
    }

    pub(crate) fn end_multi_insert(&mut self) {
        self.multi_insert_session = None;
    }

    /// Rebind the guard in a view cloned to continue this Insert session.
    pub(crate) fn rebind_multi_insert(&mut self, from: ViewId, to: ViewId, source: &TextVersion) {
        if self.is_multi_insert(from, source) {
            self.multi_insert_session
                .as_mut()
                .expect("checked session")
                .view = to;
        } else {
            self.end_multi_insert();
        }
    }

    /// The cursor left by Escape when no final text is inserted.
    pub(crate) fn insert_exit_cursor(&self, text: &TextBuffer) -> ByteOffset {
        if !matches!(self.mode, EditorMode::Insert | EditorMode::Replace) || self.cursor.get() == 0
        {
            return self.cursor;
        }
        let previous = previous_grapheme_boundary(text, self.cursor);
        let range = ByteRange {
            start: previous,
            end: self.cursor,
        };
        if text.slice(range).is_ok_and(|grapheme| grapheme != "\n") {
            previous
        } else {
            self.cursor
        }
    }

    /// Move a Normal cursor on the hidden line after a final newline to the
    /// first nonblank on the last visible line, or its start if blank.
    pub fn keep_cursor_off_final_newline(&mut self, text: &TextBuffer) {
        if self.mode != EditorMode::Normal {
            return;
        }
        let last = text.visible_lines().saturating_sub(1);
        if text.byte_to_line(self.cursor).unwrap_or(0) <= last {
            return;
        }
        let Ok(start) = text.line_to_byte(last) else {
            return;
        };
        let indent = text.line(last).map_or(0, |line| {
            line.find(|character: char| !character.is_whitespace())
                .unwrap_or(0)
        });
        self.cursor = ByteOffset(start.get() + indent);
    }

    /// Leave Insert/Replace without finalizing the session's edits or marks.
    pub(crate) fn leave_insert_without_commit(&mut self, text: &TextBuffer) -> bool {
        if !matches!(self.mode, EditorMode::Insert | EditorMode::Replace) {
            return false;
        }
        self.cursor = self.insert_exit_cursor(text);
        self.mode = EditorMode::Normal;
        self.selection = None;
        self.clear_editing_state();
        true
    }

    /// True only for the owning view and text instance, including edits during Insert.
    #[must_use]
    pub fn is_multi_insert(&self, view: ViewId, source: &TextVersion) -> bool {
        self.mode == EditorMode::Insert
            && self.multi_insert_session.as_ref().is_some_and(|session| {
                session.view == view
                    && session.buffer == self.buffer
                    && session.source.same_instance(source)
            })
    }

    pub(crate) fn clear_block_insert(&mut self) {
        self.block_insert.clear();
        self.block_append_column = None;
        self.block_append_origins.clear();
        self.deferred_block_append = None;
    }

    pub(crate) fn block_append_pending_column(&self) -> Option<usize> {
        self.block_append_column
            .filter(|_| self.block_insert == self.block_append_origins)
    }

    pub(crate) fn defer_block_append(&mut self, position: ByteOffset) -> bool {
        if let Some(append) = &mut self.deferred_block_append {
            if append.start != self.cursor {
                return false;
            }
            append.start = position;
            return true;
        }
        let Some(column) = self.block_append_pending_column() else {
            return false;
        };
        self.block_append_column = None;
        self.block_append_origins.clear();
        self.deferred_block_append = Some(DeferredBlockAppend {
            positions: std::mem::take(&mut self.block_insert),
            column,
            start: position,
        });
        true
    }

    pub(crate) fn map_block_insert(&mut self, delta: &EditDelta) {
        fn map_positions(positions: &mut Vec<ByteOffset>, delta: &EditDelta) {
            for position in positions.iter_mut() {
                *position = delta.map_offset(*position, Affinity::After);
            }
            positions.sort();
            positions.dedup();
        }

        map_positions(&mut self.block_insert, delta);
        for origin in &mut self.block_append_origins {
            *origin = delta.map_offset(*origin, Affinity::Before);
        }
        self.block_append_origins.dedup();
        if let Some(append) = &mut self.deferred_block_append {
            map_positions(&mut append.positions, delta);
            append.start = delta.map_offset(append.start, Affinity::Before);
        }
    }

    pub fn clear_editing_state(&mut self) {
        self.syntax_selections.clear();
        self.replace_history.clear();
        self.insert_repeat = None;
        self.clear_block_insert();
        self.end_multi_insert();
        self.multi_marks.clear();
        self.multi_whole_word = true;
        self.multi_preferred_columns.clear();
        self.preferred_column = None;
        self.preferred_screen_column = None;
    }

    #[must_use]
    pub fn new(buffer: BufferId) -> Self {
        Self {
            buffer,
            cursor: ByteOffset(0),
            selection: None,
            syntax_selections: Vec::new(),
            scroll_line: 0,
            scroll_wrap_row: 0,
            scroll_column: 0,
            preferred_column: None,
            preferred_screen_column: None,
            screen_motion: None,
            folded_lines: FoldedLines::default(),
            mode: EditorMode::Normal,
            replace_history: Vec::new(),
            insert_repeat: None,
            block_insert: Vec::new(),
            block_append_column: None,
            block_append_origins: Vec::new(),
            deferred_block_append: None,
            multi_insert_session: None,
            multi_marks: Vec::new(),
            multi_whole_word: true,
            multi_preferred_columns: Vec::new(),
        }
    }
}
