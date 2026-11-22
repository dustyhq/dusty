//! Execution of built-in editing commands.

use unicode_segmentation::UnicodeSegmentation;

use crate::editor::state::EditorMode;

use crate::editor::state::EditorState;

use crate::editor::state::RegisterShape;

use crate::editor::state::ReplaceRecord;
use crate::editor::state::{ScreenColumn, ScreenDirection};

use crate::editor::text::ByteOffset;

use crate::editor::text::ByteRange;

use crate::editor::text::DisplayWidthPolicy;

use crate::editor::text::TextEdit;

use crate::editor::text::TextEditSet;

use crate::editor::text::display_column_at;

use crate::editor::text::grapheme_cell_width;

use crate::editor::text::insertion_at_display_column;

use crate::editor::text::next_grapheme_boundary;

use crate::editor::text::previous_grapheme_boundary;

use crate::editor::transaction::EditorOperation;

use crate::editor::transaction::UndoPolicy;

use crate::editor::command::CommandError;

use crate::editor::command::CommandId;

use crate::editor::{CommandInvocation, CommandValue};

use crate::editor::command::CommandOutcome;

use crate::editor::command::builtins::comment::build_comment_toggle;

use crate::editor::command::builtins::motion::cursor_display_column;

use crate::editor::command::builtins::motion::line_content_end;

use crate::editor::command::builtins::motion::motion_target;

use crate::editor::command::builtins::motion::on_last_character;

use crate::editor::command::builtins::motion::operator_range;

use crate::editor::command::builtins::motion::repeat_visual_ranges;

use crate::editor::command::builtins::motion::visual_ranges;

use crate::editor::command::builtins::multi;

use crate::editor::command::builtins::pairs::AutoPair;

use crate::editor::command::builtins::pairs::auto_pair;

use crate::editor::command::builtins::pairs::auto_pair_over;

use crate::editor::command::builtins::pairs::build_surround_add;

use crate::editor::command::builtins::pairs::build_surround_change;

use crate::editor::command::builtins::pairs::empty_pair_end;

use crate::editor::command::builtins::text_object::{
    is_linewise_inner_block, shaped_text_object, visual_text_object,
};

use crate::editor::command::builtins::transaction::bool_argument;

use crate::editor::command::builtins::transaction::build_indent;

use crate::editor::command::builtins::transaction::build_insert_transition;

use crate::editor::command::builtins::transaction::build_join_lines;

use crate::editor::command::builtins::transaction::build_toggle_case;

use crate::editor::command::builtins::transaction::failed;

use crate::editor::command::builtins::transaction::string_argument;

use crate::editor::command::builtins::transaction::transaction_for;

fn checked_offset(invocation: &CommandInvocation, name: &str) -> Result<ByteOffset, CommandError> {
    let value = crate::editor::command::builtins::transaction::integer_argument(invocation, name)?;
    usize::try_from(value)
        .map(ByteOffset)
        .map_err(|_| CommandError::InvalidArguments(format!("invalid {name}")))
}

fn checked_range(
    invocation: &CommandInvocation,
    text: &crate::editor::text::TextBuffer,
) -> Result<ByteRange, CommandError> {
    if checked_offset(invocation, "revision")?.0 as u64 != text.revision().0 {
        return Err(CommandError::Unavailable("edit revision changed".into()));
    }
    let range = ByteRange::new(
        checked_offset(invocation, "start")?,
        checked_offset(invocation, "end")?,
    )
    .map_err(failed)?;
    text.slice(range).map_err(failed)?;
    Ok(range)
}

/// A remembered position on a character boundary inside the text, after edits the
/// position did not follow (an undo, a reload).
fn clamp_offset(text: &crate::editor::text::TextBuffer, position: ByteOffset) -> ByteOffset {
    let mut position = ByteOffset(position.get().min(text.len_bytes()));
    while position.get() > 0 && text.validate_offset(position).is_err() {
        position = ByteOffset(position.get() - 1);
    }
    position
}

pub(super) fn execute_builtin<E>(
    state: &EditorState,
    invocation: &CommandInvocation,
) -> Result<CommandOutcome<E>, CommandError> {
    let view_id = invocation
        .target
        .view
        .or(state.active_view())
        .ok_or_else(|| CommandError::Unavailable("no active view".into()))?;
    let view = state
        .views
        .get(view_id)
        .ok_or_else(|| CommandError::Unavailable("target view is stale".into()))?;
    let buffer_id = invocation.target.buffer.unwrap_or(view.buffer);
    if buffer_id != view.buffer {
        return Err(CommandError::Unavailable(
            "target view displays a different buffer".into(),
        ));
    }
    let buffer = state
        .buffers
        .get(buffer_id)
        .ok_or_else(|| CommandError::Unavailable("target buffer is stale".into()))?;
    let count = invocation.count.map_or(1, |value| value.get());
    let count = usize::try_from(count).map_err(|_| CommandError::CountTooLarge)?;
    let mut transaction = transaction_for(view_id, buffer_id, buffer.text.revision());

    match invocation.id.as_str() {
        id if id.starts_with("syntax.") => {
            crate::editor::command::builtins::structural::apply(
                &mut transaction,
                buffer,
                view_id,
                view,
                invocation,
                count,
            )?;
        }
        "edit.apply" => {
            let range = checked_range(invocation, &buffer.text)?;
            let position = checked_offset(invocation, "cursor")?;
            transaction.undo_policy = if view.mode == EditorMode::Insert {
                UndoPolicy::Merge
            } else {
                UndoPolicy::Record
            };
            let replacement = string_argument(invocation, "text")?;
            if !range.is_empty() || !replacement.is_empty() {
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(
                        buffer.text.revision(),
                        vec![TextEdit::new(range, replacement)],
                    ),
                });
            }
            transaction.operations.push(EditorOperation::SetCursor {
                view: view_id,
                position,
            });
            if view.mode == EditorMode::OperatorPending {
                transaction.operations.push(EditorOperation::SetMode {
                    view: view_id,
                    mode: EditorMode::Normal,
                });
            }
        }
        "view.cursor.move" => {
            let motion = counted_motion(string_argument(invocation, "motion")?, invocation);
            transaction.undo_policy =
                if matches!(view.mode, EditorMode::Insert | EditorMode::Replace) {
                    UndoPolicy::Boundary
                } else {
                    UndoPolicy::Ignore
                };
            // In Visual mode a text object selects its range.
            if let Some(mut selection) = view.selection
                && let Some((range, object_shape)) = visual_text_object(
                    &buffer.text,
                    buffer.current_syntax_objects(),
                    view.cursor,
                    selection,
                    motion,
                    count,
                )?
            {
                if range.is_empty() && motion == "inner_paragraph" {
                    // A zero-byte inner paragraph still selects the logical empty line.
                    transaction.operations.push(EditorOperation::SetMode {
                        view: view_id,
                        mode: EditorMode::VisualLine,
                    });
                    return Ok(CommandOutcome {
                        transaction: Some(transaction),
                        ..CommandOutcome::empty()
                    });
                }
                if range.is_empty()
                    && matches!(
                        motion,
                        "inner_paren" | "inner_bracket" | "inner_brace" | "inner_angle"
                    )
                {
                    // An empty bracket object must not replace the existing Visual selection.
                    return Ok(CommandOutcome::empty());
                }
                let head = if range.is_empty() {
                    range.start
                } else {
                    previous_grapheme_boundary(&buffer.text, range.end)
                };
                if object_shape == RegisterShape::Line
                    || matches!(
                        invocation.arguments.get("linewise"),
                        Some(CommandValue::Bool(true))
                    )
                {
                    selection.kind = crate::editor::SelectionKind::Line;
                    transaction.operations.push(EditorOperation::SetMode {
                        view: view_id,
                        mode: EditorMode::VisualLine,
                    });
                }
                selection.anchor = range.start;
                selection.head = head;
                transaction.operations.push(EditorOperation::SetSelection {
                    view: view_id,
                    selection: Some(selection),
                });
                transaction.operations.push(EditorOperation::SetCursor {
                    view: view_id,
                    position: head,
                });
                return Ok(CommandOutcome {
                    transaction: Some(transaction),
                    ..CommandOutcome::empty()
                });
            }
            let character = character_argument(invocation, "character")?;
            let direction = ScreenDirection::from_motion(motion);
            let screen_motion =
                direction.and_then(|direction| view.projected_screen_motion(direction, count));
            let motion = match (direction, screen_motion) {
                (Some(direction), None) => direction.file_motion(),
                _ => motion,
            };
            // Like Vim, file-line motions keep the column they started from across shorter lines.
            let wanted_column = match (motion, view.preferred_column) {
                ("up" | "down", Some(column)) => Some(column),
                ("up" | "down", None) => Some(cursor_display_column(
                    &buffer.text,
                    view.cursor,
                    state.policy,
                )?),
                _ => None,
            };
            let target = if let Some(projection) = screen_motion {
                projection.target
            } else if let Some(target) = crate::editor::command::builtins::structural::motion_target(
                buffer.current_syntax_objects(),
                view.cursor,
                motion,
                count,
            ) {
                target?
            } else {
                motion_target(
                    buffer,
                    view.cursor,
                    motion,
                    count,
                    character,
                    state.policy,
                    wanted_column,
                    &view.folded_lines,
                )?
            };
            let target = if matches!(view.mode, EditorMode::Normal | EditorMode::OperatorPending)
                || ((wanted_column.is_some() || screen_motion.is_some())
                    && !matches!(view.mode, EditorMode::Insert | EditorMode::Replace))
            {
                on_last_character(&buffer.text, target)?
            } else {
                target
            };
            transaction.operations.push(EditorOperation::SetCursor {
                view: view_id,
                position: target,
            });
            if let Some(column) = wanted_column {
                transaction
                    .operations
                    .push(EditorOperation::SetPreferredColumn {
                        view: view_id,
                        column,
                    });
            }
            let screen_column = if motion == "line_end" {
                Some(ScreenColumn::EndOfRow)
            } else {
                screen_motion.map(|projection| projection.column)
            };
            if let Some(column) = screen_column {
                transaction
                    .operations
                    .push(EditorOperation::SetPreferredScreenColumn {
                        view: view_id,
                        column,
                    });
            }
        }
        "mode.normal" => {
            transaction.undo_policy = UndoPolicy::Boundary;
            let append = multi::finish_block_append(view, &buffer.text, state.policy)?;
            if !append.is_empty() {
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(buffer.text.revision(), append),
                });
            }
            let repeated = if view.mode == EditorMode::Insert {
                view.insert_repeat
                    .as_ref()
                    .filter(|repeat| repeat.count > 1 && repeat.start <= view.cursor)
                    .map(|repeat| {
                        let text = buffer
                            .text
                            .slice(ByteRange::new(repeat.start, view.cursor).map_err(failed)?)
                            .map_err(failed)?;
                        let text = if repeat.new_line {
                            format!("\n{text}")
                        } else {
                            text
                        };
                        Ok::<_, CommandError>(text.repeat(repeat.count - 1))
                    })
                    .transpose()?
            } else {
                None
            };
            // `gi` reopens Insert mode where it stopped, before Esc steps the cursor back.
            let stopped = ByteOffset(view.cursor.get() + repeated.as_ref().map_or(0, String::len));
            if let Some(text) = repeated.filter(|text| !text.is_empty()) {
                let last = text
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(offset, _)| offset);
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(
                        buffer.text.revision(),
                        vec![TextEdit::new(ByteRange::empty(view.cursor), &text)],
                    ),
                });
                transaction.operations.push(EditorOperation::SetCursor {
                    view: view_id,
                    position: ByteOffset(view.cursor.get() + last),
                });
            } else {
                let position = view.insert_exit_cursor(&buffer.text);
                if position != view.cursor {
                    transaction.operations.push(EditorOperation::SetCursor {
                        view: view_id,
                        position,
                    });
                }
            }
            transaction.operations.push(EditorOperation::SetMode {
                view: view_id,
                mode: EditorMode::Normal,
            });
            if view.mode == EditorMode::Insert {
                transaction.operations.push(EditorOperation::SetLastInsert {
                    buffer: buffer_id,
                    position: stopped,
                });
            }
        }
        "mode.visual_reselect" => {
            transaction.undo_policy = UndoPolicy::Ignore;
            let last = buffer
                .last_visual
                .ok_or_else(|| CommandError::Unavailable("no previous Visual selection".into()))?;
            let anchor = clamp_offset(&buffer.text, last.anchor);
            let head = clamp_offset(&buffer.text, last.head);
            let mode = match last.kind {
                crate::editor::SelectionKind::Line => EditorMode::VisualLine,
                crate::editor::SelectionKind::Block => EditorMode::VisualBlock,
                crate::editor::SelectionKind::Character => EditorMode::VisualChar,
            };
            // In Visual mode the current and the previous selection swap, as in Vim.
            let current = matches!(
                view.mode,
                EditorMode::VisualChar | EditorMode::VisualLine | EditorMode::VisualBlock
            )
            .then_some(view.selection)
            .flatten();
            transaction.operations.push(EditorOperation::SetMode {
                view: view_id,
                mode,
            });
            transaction.operations.push(EditorOperation::SetSelection {
                view: view_id,
                selection: Some(crate::editor::Selection {
                    anchor,
                    head,
                    kind: last.kind,
                }),
            });
            transaction.operations.push(EditorOperation::SetCursor {
                view: view_id,
                position: head,
            });
            if current.is_some() {
                transaction.operations.push(EditorOperation::SetLastVisual {
                    buffer: buffer_id,
                    selection: current,
                });
            }
        }
        "mode.replace" => {
            transaction.undo_policy = UndoPolicy::Merge;
            transaction.operations.push(EditorOperation::SetMode {
                view: view_id,
                mode: EditorMode::Replace,
            });
        }
        "multi.add_next" => multi::add_next(&mut transaction, view_id, view, &buffer.text)?,
        "multi.add_below" | "multi.add_above" => multi::add_line(
            &mut transaction,
            view_id,
            view,
            &buffer.text,
            invocation.id.as_str() == "multi.add_below",
            state.policy,
        )?,
        "multi.insert" => {
            // Visual block `I` and `A` type at every row of the block.
            let block = if view.mode == EditorMode::VisualBlock {
                visual_ranges(&buffer.text, view, state.policy)?.0
            } else {
                view.multi_marks.clone()
            };
            multi::begin_insert(
                &mut transaction,
                view_id,
                view,
                &buffer.text,
                string_argument(invocation, "placement")?,
                &block,
                state.policy,
            )?;
        }
        "multi.delete" => multi::delete(&mut transaction, view_id, buffer_id, view, &buffer.text),
        "multi.add_all" => multi::add_all(&mut transaction, view_id, view, &buffer.text)?,
        "multi.next" | "multi.previous" => {
            multi::cycle(
                &mut transaction,
                view_id,
                view,
                invocation.id.as_str() == "multi.next",
            );
        }
        "multi.skip" => multi::skip(&mut transaction, view_id, view, &buffer.text)?,
        "multi.remove" => multi::remove(&mut transaction, view_id, view),
        "multi.move" => multi::move_all(
            &mut transaction,
            view_id,
            view,
            buffer,
            string_argument(invocation, "motion")?,
            count,
            state.policy,
        )?,
        "multi.delete_char" => {
            multi::delete_char(&mut transaction, view_id, buffer_id, view, &buffer.text)?;
        }
        "mode.visual_char" | "mode.visual_line" | "mode.visual_block" => {
            transaction.undo_policy = UndoPolicy::Ignore;
            transaction.operations.push(EditorOperation::SetMode {
                view: view_id,
                mode: match invocation.id.as_str() {
                    "mode.visual_char" => EditorMode::VisualChar,
                    "mode.visual_line" => EditorMode::VisualLine,
                    _ => EditorMode::VisualBlock,
                },
            });
        }
        "mode.insert" => {
            transaction.undo_policy = UndoPolicy::Merge;
            let placement = string_argument(invocation, "placement")?;
            // `gi`: where Insert mode last stopped in this buffer, else at the cursor.
            let (cursor, placement) = if placement == "last_insert" {
                (
                    buffer
                        .last_insert
                        .map_or(view.cursor, |position| clamp_offset(&buffer.text, position)),
                    "before",
                )
            } else {
                (view.cursor, placement)
            };
            build_insert_transition(
                &mut transaction,
                &buffer.text,
                view_id,
                buffer_id,
                cursor,
                placement,
            )?;
            if count > 1 {
                transaction
                    .operations
                    .push(EditorOperation::SetInsertRepeat {
                        view: view_id,
                        count,
                        new_line: matches!(placement, "open_above" | "open_below"),
                    });
            }
        }
        "edit.insert_text" => {
            transaction.undo_policy = if state.policy.undo_break_on_text_commit {
                UndoPolicy::Record
            } else {
                UndoPolicy::Merge
            };
            let normalized = TextEdit::new(
                ByteRange::empty(view.cursor),
                string_argument(invocation, "text")?,
            );
            let text = normalized.replacement();
            if let Some(range) = insert_selection(view) {
                let pair = state
                    .policy
                    .autopairs
                    .then(|| auto_pair_over(&buffer.text, range, text))
                    .flatten();
                let replacement = match pair {
                    Some(AutoPair::Open(close)) => format!("{text}{close}"),
                    _ => text.to_owned(),
                };
                push_selection_replacement(
                    &mut transaction,
                    view_id,
                    buffer_id,
                    buffer.text.revision(),
                    range,
                    &replacement,
                    text.len(),
                );
                return Ok(CommandOutcome {
                    transaction: Some(transaction),
                    ..CommandOutcome::empty()
                });
            }
            let synchronized = view.block_insert.as_slice();
            let pair = (state.policy.autopairs && synchronized.is_empty())
                .then(|| auto_pair(&buffer.text, view.cursor, text))
                .flatten();
            if let Some(pair) = pair {
                let (inserted, step) = match pair {
                    AutoPair::Open(close) => (format!("{text}{close}"), text.len()),
                    AutoPair::Skip => (String::new(), text.len()),
                };
                if !inserted.is_empty() {
                    transaction.operations.push(EditorOperation::ApplyEdits {
                        buffer: buffer_id,
                        edits: TextEditSet::new(
                            buffer.text.revision(),
                            vec![TextEdit::new(ByteRange::empty(view.cursor), inserted)],
                        ),
                    });
                }
                transaction.operations.push(EditorOperation::SetCursor {
                    view: view_id,
                    position: ByteOffset(view.cursor.get() + step),
                });
                return Ok(CommandOutcome {
                    transaction: Some(transaction),
                    ..CommandOutcome::empty()
                });
            }
            if !text.is_empty() {
                let positions = if synchronized.is_empty() {
                    std::slice::from_ref(&view.cursor)
                } else {
                    synchronized
                };
                let append_column = view.block_append_pending_column();
                let edits = positions
                    .iter()
                    .map(|&position| {
                        multi::insert_at(&buffer.text, position, text, append_column, state.policy)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let cursor = ByteOffset(positions[0].get() + edits[0].replacement().len());
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(buffer.text.revision(), edits),
                });
                transaction.operations.push(EditorOperation::SetCursor {
                    view: view_id,
                    position: cursor,
                });
            }
        }
        "edit.replace_text" => {
            transaction.undo_policy = if state.policy.undo_break_on_text_commit {
                UndoPolicy::Record
            } else {
                UndoPolicy::Merge
            };
            let normalized = TextEdit::new(
                ByteRange::empty(view.cursor),
                string_argument(invocation, "text")?,
            );
            let replacement = normalized.replacement();
            let mut end = view.cursor;
            for grapheme in replacement.graphemes(true) {
                if grapheme == "\n" {
                    break;
                }
                let next = next_grapheme_boundary(&buffer.text, end);
                if next == end
                    || buffer
                        .text
                        .slice(ByteRange::new(end, next).map_err(failed)?)
                        .map_err(failed)?
                        == "\n"
                {
                    break;
                }
                end = next;
            }
            let range = ByteRange::new(view.cursor, end).map_err(failed)?;
            let original = buffer.text.slice(range).map_err(failed)?;
            transaction.operations.push(EditorOperation::ApplyEdits {
                buffer: buffer_id,
                edits: TextEditSet::new(
                    buffer.text.revision(),
                    vec![TextEdit::new(range, replacement)],
                ),
            });
            transaction.operations.push(EditorOperation::SetCursor {
                view: view_id,
                position: ByteOffset(view.cursor.get().saturating_add(replacement.len())),
            });
            transaction
                .operations
                .push(EditorOperation::PushReplaceRecord {
                    view: view_id,
                    record: ReplaceRecord {
                        start: view.cursor,
                        original,
                        inserted: replacement.to_owned(),
                    },
                });
        }
        "edit.replace_backspace" => {
            transaction.undo_policy = if state.policy.undo_break_on_text_commit {
                UndoPolicy::Record
            } else {
                UndoPolicy::Merge
            };
            let Some(record) = view.replace_history.last().filter(|record| {
                let end = ByteOffset(record.start.get().saturating_add(record.inserted.len()));
                end == view.cursor
                    && buffer
                        .text
                        .slice(ByteRange::new(record.start, end).expect("ordered range"))
                        .is_ok_and(|text| text == record.inserted)
            }) else {
                return Ok(CommandOutcome {
                    transaction: None,
                    ..CommandOutcome::empty()
                });
            };
            let end = ByteOffset(record.start.get().saturating_add(record.inserted.len()));
            transaction
                .operations
                .push(EditorOperation::PopReplaceRecord { view: view_id });
            transaction.operations.push(EditorOperation::ApplyEdits {
                buffer: buffer_id,
                edits: TextEditSet::new(
                    buffer.text.revision(),
                    vec![TextEdit::new(
                        ByteRange::new(record.start, end).map_err(failed)?,
                        &record.original,
                    )],
                ),
            });
            transaction.operations.push(EditorOperation::SetCursor {
                view: view_id,
                position: record.start,
            });
        }
        "edit.backspace" => {
            transaction.undo_policy = if state.policy.undo_break_on_text_commit {
                UndoPolicy::Record
            } else {
                UndoPolicy::Merge
            };
            if let Some(range) = insert_selection(view) {
                push_selection_replacement(
                    &mut transaction,
                    view_id,
                    buffer_id,
                    buffer.text.revision(),
                    range,
                    "",
                    0,
                );
                return Ok(CommandOutcome {
                    transaction: Some(transaction),
                    ..CommandOutcome::empty()
                });
            }
            // Backspacing before the first append ends the block. On a short
            // primary row it moves through virtual space, not into real text.
            let synchronized = if let Some(column) = view.block_append_pending_column() {
                let cursor_column = cursor_display_column(&buffer.text, view.cursor, state.policy)?;
                let in_virtual_space = column > cursor_column;
                transaction
                    .operations
                    .push(EditorOperation::SetBlockInsert {
                        view: view_id,
                        positions: if in_virtual_space {
                            vec![view.cursor]
                        } else {
                            Vec::new()
                        },
                        append_column: in_virtual_space.then(|| column - 1),
                    });
                if in_virtual_space {
                    return Ok(CommandOutcome {
                        transaction: Some(transaction),
                        ..CommandOutcome::empty()
                    });
                }
                &[]
            } else {
                view.block_insert.as_slice()
            };
            let positions = if synchronized.is_empty() {
                vec![view.cursor]
            } else {
                synchronized.to_vec()
            };
            let mut starts = positions
                .iter()
                .map(|position| previous_grapheme_boundary(&buffer.text, *position))
                .collect::<Vec<_>>();
            let live_append = !synchronized.is_empty() && view.block_append_column.is_some();
            if let Some(column) = view.block_append_column.filter(|_| live_append)
                && let Some(&origin) = view.block_append_origins.first()
            {
                let origin_column = cursor_display_column(&buffer.text, origin, state.policy)?;
                let padding = column.saturating_sub(origin_column);
                // When Backspace erases the whole appended body, remove each
                // row's generated padding in that same edit.
                if starts[0].get() == origin.get() + padding
                    && starts.len() == view.block_append_origins.len()
                {
                    starts.copy_from_slice(&view.block_append_origins);
                }
            }
            let pair_end = (state.policy.autopairs && view.block_insert.is_empty())
                .then(|| empty_pair_end(&buffer.text, view.cursor))
                .flatten();
            let ends = pair_end.map_or_else(|| positions.clone(), |end| vec![end]);
            let edits = starts
                .iter()
                .zip(&ends)
                .filter(|(start, end)| start != end)
                .map(|(start, end)| {
                    ByteRange::new(*start, *end)
                        .map(|range| TextEdit::new(range, ""))
                        .map_err(failed)
                })
                .collect::<Result<Vec<_>, _>>()?;
            if !edits.is_empty() {
                if !synchronized.is_empty() && !live_append {
                    transaction
                        .operations
                        .push(EditorOperation::SetBlockInsert {
                            view: view_id,
                            positions: starts.clone(),
                            append_column: None,
                        });
                }
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(buffer.text.revision(), edits),
                });
                transaction.operations.push(EditorOperation::SetCursor {
                    view: view_id,
                    position: starts[0],
                });
            }
        }
        "edit.delete_char" | "edit.substitute" => {
            let mut end = view.cursor;
            for _ in 0..count {
                let next = next_grapheme_boundary(&buffer.text, end);
                if next == end
                    || buffer
                        .text
                        .slice(ByteRange::new(end, next).map_err(failed)?)
                        .map_err(failed)?
                        == "\n"
                {
                    break;
                }
                end = next;
            }
            if invocation.id.as_str() == "edit.substitute" {
                transaction.undo_policy = UndoPolicy::Merge;
                transaction.operations.push(EditorOperation::SetMode {
                    view: view_id,
                    mode: EditorMode::Insert,
                });
            }
            if end != view.cursor {
                let text = buffer
                    .text
                    .slice(ByteRange::new(view.cursor, end).map_err(failed)?)
                    .map_err(failed)?;
                transaction.operations.push(EditorOperation::SetRegister {
                    name: invocation.register.unwrap_or('"'),
                    text,
                    shape: RegisterShape::Character,
                });
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(
                        buffer.text.revision(),
                        vec![TextEdit::new(
                            ByteRange::new(view.cursor, end).map_err(failed)?,
                            "",
                        )],
                    ),
                });
            }
        }
        "edit.delete_backward" => {
            let mut start = view.cursor;
            for _ in 0..count {
                let previous = previous_grapheme_boundary(&buffer.text, start);
                if previous == start
                    || buffer
                        .text
                        .slice(ByteRange::new(previous, start).map_err(failed)?)
                        .map_err(failed)?
                        == "\n"
                {
                    break;
                }
                start = previous;
            }
            if start != view.cursor {
                let range = ByteRange::new(start, view.cursor).map_err(failed)?;
                let text = buffer.text.slice(range).map_err(failed)?;
                transaction.operations.push(EditorOperation::SetRegister {
                    name: invocation.register.unwrap_or('"'),
                    text,
                    shape: RegisterShape::Character,
                });
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(buffer.text.revision(), vec![TextEdit::new(range, "")]),
                });
                transaction.operations.push(EditorOperation::SetCursor {
                    view: view_id,
                    position: start,
                });
            }
        }
        "edit.replace" => {
            let normalized = TextEdit::new(
                ByteRange::empty(view.cursor),
                string_argument(invocation, "text")?,
            );
            let replacement = normalized.replacement();
            let mut end = view.cursor;
            let mut replaced = 0;
            for _ in 0..count {
                let next = next_grapheme_boundary(&buffer.text, end);
                if next == end
                    || buffer
                        .text
                        .slice(ByteRange::new(end, next).map_err(failed)?)
                        .map_err(failed)?
                        == "\n"
                {
                    break;
                }
                end = next;
                replaced += 1;
            }
            if replaced == count {
                let replacement = replacement.repeat(count);
                let last = replacement
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(offset, _)| offset);
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(
                        buffer.text.revision(),
                        vec![TextEdit::new(
                            ByteRange::new(view.cursor, end).map_err(failed)?,
                            replacement,
                        )],
                    ),
                });
                transaction.operations.push(EditorOperation::SetCursor {
                    view: view_id,
                    position: ByteOffset(view.cursor.get() + last),
                });
            }
        }
        "edit.join_lines" => {
            // `J` and `2J` join two lines, `3J` three, as in Vim.
            build_join_lines(
                &mut transaction,
                &buffer.text,
                view_id,
                buffer_id,
                view.cursor,
                count.saturating_sub(1).max(1),
            )?;
        }
        "view.selection.swap" => {
            let Some(selection) = view.selection else {
                return Err(CommandError::Unavailable("nothing is selected".into()));
            };
            transaction.operations.push(EditorOperation::SetSelection {
                view: view_id,
                selection: Some(crate::editor::Selection {
                    anchor: selection.head,
                    head: selection.anchor,
                    kind: selection.kind,
                }),
            });
            transaction.operations.push(EditorOperation::SetCursor {
                view: view_id,
                position: selection.anchor,
            });
            transaction.undo_policy = UndoPolicy::Ignore;
        }
        "edit.toggle_case" => {
            build_toggle_case(
                &mut transaction,
                &buffer.text,
                view_id,
                buffer_id,
                view.cursor,
                count,
            )?;
        }
        "edit.operator" | "edit.repeat_visual" => {
            let operator = string_argument(invocation, "operator")?;
            let mut keep_line_indent = true;
            let mut yank_to_range_start = false;
            let (ranges, shape) = if invocation.arguments.contains_key("start") {
                (
                    vec![checked_range(invocation, &buffer.text)?],
                    if bool_argument(invocation, "linewise")? {
                        RegisterShape::Line
                    } else {
                        RegisterShape::Character
                    },
                )
            } else if invocation.id.as_str() == "edit.repeat_visual" {
                repeat_visual_ranges(&buffer.text, view, invocation, state.policy)?
            } else {
                let motion = counted_motion(string_argument(invocation, "motion")?, invocation);
                let requested_linewise =
                    bool_argument(invocation, "linewise")? || motion == "percentage";
                if motion == "visual" {
                    visual_ranges(&buffer.text, view, state.policy)?
                } else {
                    let character = character_argument(
                        invocation,
                        if operator == "surround_add" {
                            "find_character"
                        } else {
                            "character"
                        },
                    )?;
                    let resolved_range = if let Some((range, shape)) = shaped_text_object(
                        &buffer.text,
                        buffer.current_syntax_objects(),
                        view.cursor,
                        motion,
                        count,
                    )? {
                        yank_to_range_start = true;
                        // Whole-line block objects keep a newline, not body indentation.
                        keep_line_indent = !is_linewise_inner_block(&buffer.text, motion, range)?;
                        let shape = if requested_linewise {
                            RegisterShape::Line
                        } else {
                            shape
                        };
                        Some((range, shape))
                    } else {
                        operator_range(
                            buffer,
                            view,
                            motion,
                            count,
                            operator,
                            requested_linewise,
                            character,
                            state.policy,
                        )?
                    };
                    let Some((range, shape)) = resolved_range else {
                        if view.mode != EditorMode::OperatorPending {
                            return Ok(CommandOutcome::empty());
                        }
                        transaction.undo_policy = UndoPolicy::Ignore;
                        transaction.operations.push(EditorOperation::SetMode {
                            view: view_id,
                            mode: EditorMode::Normal,
                        });
                        return Ok(CommandOutcome {
                            transaction: Some(transaction),
                            ..CommandOutcome::empty()
                        });
                    };
                    (vec![range], shape)
                }
            };
            let first_range = *ranges
                .first()
                .ok_or_else(|| CommandError::Unavailable("visual block is empty".into()))?;
            let whole_range = ByteRange::new(
                first_range.start,
                ranges.last().map_or(first_range.end, |range| range.end),
            )
            .map_err(failed)?;
            if matches!(operator, "comment" | "surround_add") {
                if operator == "comment" {
                    let token = invocation
                        .arguments
                        .get("comment")
                        .map(|value| value.as_str())
                        .transpose()?
                        .ok_or_else(|| {
                            CommandError::Unavailable(
                                "no comment string for this file type; set editor.comments".into(),
                            )
                        })?;
                    build_comment_toggle(
                        &mut transaction,
                        &buffer.text,
                        view_id,
                        buffer_id,
                        whole_range,
                        view.cursor.min(whole_range.end).max(whole_range.start),
                        token,
                    )?;
                } else {
                    let character =
                        character_argument(invocation, "character")?.ok_or_else(|| {
                            CommandError::InvalidArguments("surround needs a character".into())
                        })?;
                    build_surround_add(
                        &mut transaction,
                        &buffer.text,
                        view_id,
                        buffer_id,
                        whole_range,
                        character,
                    )?;
                }
                return Ok(CommandOutcome {
                    transaction: Some(transaction),
                    ..CommandOutcome::empty()
                });
            }
            if matches!(operator, "indent" | "unindent") {
                build_indent(
                    &mut transaction,
                    &buffer.text,
                    view_id,
                    buffer_id,
                    whole_range,
                    operator == "indent",
                    // In Visual mode the count is how many levels; otherwise it counts lines.
                    if invocation.id.as_str() == "edit.repeat_visual"
                        || string_argument(invocation, "motion").ok() == Some("visual")
                    {
                        count
                    } else {
                        1
                    },
                    state.policy,
                )?;
                transaction.operations.push(EditorOperation::SetMode {
                    view: view_id,
                    mode: EditorMode::Normal,
                });
                return Ok(CommandOutcome {
                    transaction: Some(transaction),
                    ..CommandOutcome::empty()
                });
            }
            if matches!(operator, "lowercase" | "uppercase" | "toggle_case") {
                let edits = ranges
                    .iter()
                    .map(|range| {
                        let text = buffer.text.slice(*range).map_err(failed)?;
                        let changed = match operator {
                            "lowercase" => text.to_lowercase(),
                            "uppercase" => text.to_uppercase(),
                            _ => text
                                .chars()
                                .map(|character| {
                                    if character.is_uppercase() {
                                        character.to_lowercase().collect::<String>()
                                    } else {
                                        character.to_uppercase().collect()
                                    }
                                })
                                .collect(),
                        };
                        Ok(TextEdit::new(*range, changed))
                    })
                    .collect::<Result<Vec<_>, CommandError>>()?;
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(buffer.text.revision(), edits),
                });
                transaction.operations.push(EditorOperation::SetCursor {
                    view: view_id,
                    position: first_range.start,
                });
                transaction.operations.push(EditorOperation::SetMode {
                    view: view_id,
                    mode: EditorMode::Normal,
                });
                return Ok(CommandOutcome {
                    transaction: Some(transaction),
                    ..CommandOutcome::empty()
                });
            }
            if operator == "join" {
                // Visual `J` joins every selected line, and at least two.
                let first = buffer
                    .text
                    .byte_to_line(whole_range.start)
                    .map_err(failed)?;
                let last = buffer
                    .text
                    .byte_to_line(if whole_range.is_empty() {
                        whole_range.start
                    } else {
                        previous_grapheme_boundary(&buffer.text, whole_range.end)
                    })
                    .map_err(failed)?;
                build_join_lines(
                    &mut transaction,
                    &buffer.text,
                    view_id,
                    buffer_id,
                    whole_range.start,
                    last.saturating_sub(first).max(1),
                )?;
                transaction.operations.push(EditorOperation::SetMode {
                    view: view_id,
                    mode: EditorMode::Normal,
                });
                return Ok(CommandOutcome {
                    transaction: Some(transaction),
                    ..CommandOutcome::empty()
                });
            }
            if operator == "replace" {
                // Visual `p`: the register replaces the selection, and the selection goes into
                // the unnamed register, as in Vim.
                let register = state
                    .registers
                    .get(invocation.register.unwrap_or('"'))
                    .ok_or_else(|| CommandError::Failed("register is empty".into()))?;
                if shape == RegisterShape::Block || register.shape == RegisterShape::Block {
                    return Err(CommandError::Unavailable(
                        "pasting over a block selection is not supported".into(),
                    ));
                }
                let text =
                    if register.shape == RegisterShape::Line && !register.text.ends_with('\n') {
                        format!("{}\n", register.text).repeat(count)
                    } else {
                        register.text.repeat(count)
                    };
                let put = match (shape, register.shape) {
                    (RegisterShape::Line, RegisterShape::Character) => format!("{text}\n"),
                    (RegisterShape::Character, RegisterShape::Line) => format!("\n{text}"),
                    _ => text,
                };
                let replaced =
                    register_text(buffer.text.slice(whole_range).map_err(failed)?, shape);
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(
                        buffer.text.revision(),
                        vec![TextEdit::new(whole_range, put)],
                    ),
                });
                transaction.operations.push(EditorOperation::SetRegister {
                    name: '"',
                    text: replaced,
                    shape,
                });
                transaction.operations.push(EditorOperation::SetCursor {
                    view: view_id,
                    position: whole_range.start,
                });
                transaction.operations.push(EditorOperation::SetMode {
                    view: view_id,
                    mode: EditorMode::Normal,
                });
                return Ok(CommandOutcome {
                    transaction: Some(transaction),
                    ..CommandOutcome::empty()
                });
            }
            if !matches!(operator, "delete" | "change" | "yank") {
                return Err(CommandError::InvalidArguments(format!(
                    "unknown operator `{operator}`"
                )));
            }
            // A zero-width characterwise delete or change (such as an adjacent `T`)
            // removes nothing. An empty yank, unlike a delete or change, still records it.
            if operator == "yank" || shape != RegisterShape::Character || !whole_range.is_empty() {
                let text = ranges
                    .iter()
                    .map(|range| buffer.text.slice(*range).map_err(failed))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(if shape == RegisterShape::Block {
                        "\n"
                    } else {
                        ""
                    });
                transaction.operations.push(EditorOperation::SetRegister {
                    name: invocation.register.unwrap_or('"'),
                    text: register_text(text, shape),
                    shape,
                });
            }
            // A linewise change keeps one line and, for motions like `cc`, its indent.
            let (ranges, first_range) = if operator == "change" && shape == RegisterShape::Line {
                let range = linewise_change_range(&buffer.text, whole_range, keep_line_indent)
                    .map_err(failed)?;
                (vec![range], range)
            } else {
                (ranges, first_range)
            };
            if operator != "yank" {
                if operator == "change" {
                    transaction.undo_policy = UndoPolicy::Merge;
                }
                let mut cursor = first_range.start;
                if operator == "change" && shape == RegisterShape::Block {
                    // Keep the register's row alignment, but insert only where the
                    // row reaches the left edge, including a caret right at EOL.
                    let column = if invocation.id.as_str() == "edit.repeat_visual" {
                        cursor_display_column(&buffer.text, view.cursor, state.policy)?
                    } else if let Some(selection) = view.selection {
                        multi::block_columns(
                            &buffer.text,
                            selection.anchor,
                            selection.head,
                            state.policy,
                        )?
                        .0
                    } else {
                        cursor_display_column(&buffer.text, view.cursor, state.policy)?
                    };
                    let mut positions = Vec::new();
                    for &range in &ranges {
                        if multi::block_row_reaches(&buffer.text, range, column, state.policy)? {
                            positions.push(range.start);
                        }
                    }
                    let Some(&first) = positions.first() else {
                        transaction.operations.push(EditorOperation::SetMode {
                            view: view_id,
                            mode: EditorMode::Normal,
                        });
                        return Ok(CommandOutcome {
                            transaction: Some(transaction),
                            ..CommandOutcome::empty()
                        });
                    };
                    cursor = first;
                    transaction
                        .operations
                        .push(EditorOperation::SetBlockInsert {
                            view: view_id,
                            positions,
                            append_column: None,
                        });
                }
                let edits = ranges
                    .iter()
                    .filter(|range| !range.is_empty())
                    .map(|&range| TextEdit::new(range, ""))
                    .collect();
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(buffer.text.revision(), edits),
                });
                transaction.operations.push(EditorOperation::SetCursor {
                    view: view_id,
                    position: cursor,
                });
            } else {
                transaction.undo_policy = UndoPolicy::Ignore;
                transaction.operations.push(EditorOperation::Yanked {
                    view: view_id,
                    ranges,
                });
                if yank_to_range_start
                    || matches!(
                        view.mode,
                        EditorMode::VisualChar | EditorMode::VisualLine | EditorMode::VisualBlock
                    )
                {
                    transaction.operations.push(EditorOperation::SetCursor {
                        view: view_id,
                        position: first_range.start,
                    });
                }
            }
            transaction.operations.push(EditorOperation::SetMode {
                view: view_id,
                mode: if operator == "change" {
                    EditorMode::Insert
                } else {
                    EditorMode::Normal
                },
            });
            // A Visual yank moved the cursor to the start; `gv` gets the whole selection.
            if operator == "yank"
                && matches!(
                    view.mode,
                    EditorMode::VisualChar | EditorMode::VisualLine | EditorMode::VisualBlock
                )
            {
                transaction.operations.push(EditorOperation::SetLastVisual {
                    buffer: buffer_id,
                    selection: view.selection,
                });
            }
        }
        "edit.surround" => {
            let action = string_argument(invocation, "action")?;
            let character = character_argument(invocation, "character")?
                .ok_or_else(|| CommandError::InvalidArguments("missing `character`".into()))?;
            let replacement = match action {
                "delete" => None,
                "replace" => Some(character_argument(invocation, "replacement")?.ok_or_else(
                    || CommandError::InvalidArguments("missing `replacement`".into()),
                )?),
                other => {
                    return Err(CommandError::InvalidArguments(format!(
                        "unknown surround action `{other}`"
                    )));
                }
            };
            build_surround_change(
                &mut transaction,
                &buffer.text,
                view_id,
                buffer_id,
                view.cursor,
                character,
                replacement,
            )?;
        }
        "edit.put" => {
            let before = bool_argument(invocation, "before")?;
            let register = state
                .registers
                .get(invocation.register.unwrap_or('"'))
                .ok_or_else(|| CommandError::Failed("register is empty".into()))?;
            if register.shape == RegisterShape::Block {
                let cursor_line = buffer.text.byte_to_line(view.cursor).map_err(failed)?;
                let line_start = buffer.text.line_to_byte(cursor_line).map_err(failed)?;
                let source_line = buffer.text.line(cursor_line).map_err(failed)?;
                let source_content = source_line.strip_suffix('\n').unwrap_or(&source_line);
                let width_policy = DisplayWidthPolicy {
                    tab_width: state.policy.tab_width,
                    ambiguous_wide: state.policy.ambiguous_width_wide,
                };
                let cursor_relative = if before {
                    view.cursor.get().saturating_sub(line_start.get())
                } else {
                    next_grapheme_boundary(&buffer.text, view.cursor)
                        .get()
                        .saturating_sub(line_start.get())
                };
                let column = display_column_at(source_content, cursor_relative, width_policy);
                let mut rows = register
                    .text
                    .split('\n')
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if count > 1 {
                    // Preserve raw yanks and single puts; only repeated copies need padding.
                    let mut widths = Vec::with_capacity(rows.len());
                    for row in &mut rows {
                        let mut end_column = column;
                        let mut expanded = String::new();
                        for grapheme in row.graphemes(true) {
                            let cells = grapheme_cell_width(grapheme, end_column, width_policy);
                            if grapheme == "\t" {
                                expanded.push_str(&" ".repeat(cells));
                            } else {
                                expanded.push_str(grapheme);
                            }
                            end_column = end_column.saturating_add(cells);
                        }
                        widths.push(end_column.saturating_sub(column));
                        *row = expanded;
                    }
                    let width = widths.iter().copied().max().unwrap_or(0);
                    for (row, row_width) in rows.iter_mut().zip(widths) {
                        row.push_str(&" ".repeat(width.saturating_sub(row_width)));
                    }
                }
                let mut edits = Vec::new();
                let mut extra = String::new();
                for (row, text) in rows.iter().enumerate() {
                    let target_line = cursor_line.saturating_add(row);
                    let repeated = text.repeat(count);
                    if target_line >= buffer.text.len_lines() {
                        extra.push('\n');
                        extra.push_str(&" ".repeat(column));
                        extra.push_str(&repeated);
                        continue;
                    }
                    let start = buffer.text.line_to_byte(target_line).map_err(failed)?;
                    let line = buffer.text.line(target_line).map_err(failed)?;
                    let content = line.strip_suffix('\n').unwrap_or(&line);
                    let (relative, padding_cells) =
                        insertion_at_display_column(content, column, width_policy);
                    edits.push(TextEdit::new(
                        ByteRange::empty(ByteOffset(start.get().saturating_add(relative))),
                        format!("{}{repeated}", " ".repeat(padding_cells)),
                    ));
                }
                if !extra.is_empty() {
                    let end = ByteOffset(buffer.text.len_bytes());
                    if let Some(last) = edits
                        .last_mut()
                        .filter(|edit| edit.range() == ByteRange::empty(end))
                    {
                        *last = TextEdit::new(
                            ByteRange::empty(end),
                            format!("{}{extra}", last.replacement()),
                        );
                    } else {
                        edits.push(TextEdit::new(ByteRange::empty(end), extra));
                    }
                }
                transaction.operations.push(EditorOperation::ApplyEdits {
                    buffer: buffer_id,
                    edits: TextEditSet::new(buffer.text.revision(), edits),
                });
                transaction.operations.push(EditorOperation::SetCursor {
                    view: view_id,
                    position: view.cursor,
                });
                return Ok(CommandOutcome {
                    transaction: Some(transaction),
                    ..CommandOutcome::empty()
                });
            }
            let at = match (register.shape, before) {
                (RegisterShape::Character, true) => view.cursor,
                (RegisterShape::Character, false) => {
                    let line = buffer.text.byte_to_line(view.cursor).map_err(failed)?;
                    next_grapheme_boundary(&buffer.text, view.cursor)
                        .min(line_content_end(&buffer.text, line)?)
                }
                (RegisterShape::Line, true) => {
                    let line = buffer.text.byte_to_line(view.cursor).map_err(failed)?;
                    buffer.text.line_to_byte(line).map_err(failed)?
                }
                (RegisterShape::Line, false) => {
                    let line = buffer.text.byte_to_line(view.cursor).map_err(failed)?;
                    if line + 1 < buffer.text.len_lines() {
                        buffer.text.line_to_byte(line + 1).map_err(failed)?
                    } else {
                        ByteOffset(buffer.text.len_bytes())
                    }
                }
                (RegisterShape::Block, _) => unreachable!("block registers handled above"),
            };
            let mut text = register.text.clone();
            let mut cursor = at;
            if register.shape == RegisterShape::Line {
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text = text.repeat(count);
                // Keep the logical empty line after a linewise P, or before a p.
                if before && buffer.text.is_empty() {
                    text.push('\n');
                } else if !before
                    && at.get() == buffer.text.len_bytes()
                    && !buffer.text.to_string().ends_with('\n')
                {
                    text.insert(0, '\n');
                    cursor = ByteOffset(at.get() + 1);
                }
            } else {
                text = text.repeat(count);
            }
            let edit = TextEdit::new(ByteRange::empty(at), text);
            if register.shape == RegisterShape::Character && !edit.replacement().contains('\n') {
                let last = edit
                    .replacement()
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(offset, _)| offset);
                cursor = ByteOffset(at.get().saturating_add(last));
            }
            transaction.operations.push(EditorOperation::ApplyEdits {
                buffer: buffer_id,
                edits: TextEditSet::new(buffer.text.revision(), vec![edit]),
            });
            transaction.operations.push(EditorOperation::SetCursor {
                view: view_id,
                position: cursor,
            });
        }
        "history.undo" | "history.redo" => {
            transaction.undo_policy = UndoPolicy::Ignore;
            let undo = invocation.id.as_str() == "history.undo";
            // Keep the normal empty-history error, but stop counted commands at the boundary.
            let steps = buffer.undo.available_steps(undo, count).max(1);
            for _ in 0..steps {
                transaction.operations.push(if undo {
                    EditorOperation::Undo { buffer: buffer_id }
                } else {
                    EditorOperation::Redo { buffer: buffer_id }
                });
            }
        }
        other => {
            return Err(CommandError::Unknown(CommandId::new(other)?));
        }
    }

    Ok(CommandOutcome {
        transaction: Some(transaction),
        ..CommandOutcome::empty()
    })
}

/// A selection held while typing, which only a snippet placeholder or the mouse makes.
fn insert_selection(view: &crate::editor::state::View) -> Option<ByteRange> {
    let selection = view.selection.filter(|_| view.mode == EditorMode::Insert)?;
    let (start, end) = if selection.anchor <= selection.head {
        (selection.anchor, selection.head)
    } else {
        (selection.head, selection.anchor)
    };
    ByteRange::new(start, end)
        .ok()
        .filter(|range| !range.is_empty())
}

/// Typing over a selection replaces it, as a placeholder expects.
fn push_selection_replacement(
    transaction: &mut crate::editor::transaction::EditorTransaction,
    view: crate::editor::ViewId,
    buffer_id: crate::editor::BufferId,
    revision: crate::editor::text::BufferRevision,
    range: ByteRange,
    text: &str,
    cursor_step: usize,
) {
    transaction.operations.push(EditorOperation::ApplyEdits {
        buffer: buffer_id,
        edits: TextEditSet::new(revision, vec![TextEdit::new(range, text)]),
    });
    transaction.operations.push(EditorOperation::SetSelection {
        view,
        selection: None,
    });
    transaction.operations.push(EditorOperation::SetCursor {
        view,
        position: ByteOffset(range.start.get() + cursor_step),
    });
}

fn character_argument(
    invocation: &CommandInvocation,
    name: &str,
) -> Result<Option<char>, CommandError> {
    let Some(value) = invocation.arguments.get(name) else {
        return Ok(None);
    };
    let mut characters = value.as_str()?.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) => Ok(Some(character)),
        _ => Err(CommandError::InvalidArguments(format!(
            "`{name}` must be one character"
        ))),
    }
}

/// Counts turn `G`/`gg` into line navigation and `%` into percentage navigation.
fn counted_motion<'a>(motion: &'a str, invocation: &CommandInvocation) -> &'a str {
    if invocation.count.is_some() && matches!(motion, "document_start" | "document_end") {
        "line_number"
    } else if invocation.count.is_some() && motion == "matching_pair" {
        "percentage"
    } else {
        motion
    }
}

/// Line registers include the terminator even if the edit range did not.
fn register_text(mut text: String, shape: RegisterShape) -> String {
    if shape == RegisterShape::Line && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// The part of whole lines a linewise change replaces, leaving the final newline and
/// optionally the first line's indent.
fn linewise_change_range(
    text: &crate::editor::text::TextBuffer,
    lines: ByteRange,
    keep_indent: bool,
) -> Result<ByteRange, crate::editor::text::PositionError> {
    let covered = text.slice(lines)?;
    let indent = if keep_indent {
        covered.len() - covered.trim_start_matches([' ', '\t']).len()
    } else {
        0
    };
    let end = lines.end.get() - usize::from(covered.ends_with('\n'));
    let start = (lines.start.get() + indent).min(end);
    ByteRange::new(ByteOffset(start), ByteOffset(end))
}
