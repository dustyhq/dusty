//! Vim text objects (`iw`, `a(`, `i"`, `ip`, …). They are motions whose result is a
//! range rather than a cursor target, so operators and Visual mode share them.

use unicode_segmentation::UnicodeSegmentation;

use crate::editor::text::ByteOffset;

use crate::editor::text::ByteRange;

use crate::editor::SyntaxObject;
use crate::editor::SyntaxObjectKind;
use crate::editor::text::TextBuffer;

use crate::editor::command::CommandError;

use crate::editor::command::builtins::motion::word_class;

use crate::editor::command::builtins::transaction::failed;
use crate::editor::state::{RegisterShape, Selection};

#[derive(Clone, Copy)]
enum ObjectUse {
    Standard,
    Visual(Selection),
}

fn text_object_shape(motion: &str) -> RegisterShape {
    if matches!(motion, "inner_paragraph" | "a_paragraph") {
        RegisterShape::Line
    } else {
        RegisterShape::Character
    }
}

/// Vim promotes a direct inner block operator that spans whole lines to linewise.
pub(super) fn is_linewise_inner_block(
    text: &TextBuffer,
    motion: &str,
    range: ByteRange,
) -> Result<bool, CommandError> {
    if range.is_empty()
        || !matches!(
            motion,
            "inner_paren" | "inner_bracket" | "inner_brace" | "inner_angle"
        )
    {
        return Ok(false);
    }
    for endpoint in [range.start, range.end] {
        let line = text.byte_to_line(endpoint).map_err(failed)?;
        if text.line_to_byte(line).map_err(failed)? != endpoint {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Resolves a text object with the shape it imposes on an operator.
pub(super) fn shaped_text_object(
    text: &TextBuffer,
    objects: &[SyntaxObject],
    cursor: ByteOffset,
    motion: &str,
    count: usize,
) -> Result<Option<(ByteRange, RegisterShape)>, CommandError> {
    let Some(range) = text_object(text, objects, cursor, motion, count)? else {
        return Ok(None);
    };
    let shape = if is_linewise_inner_block(text, motion, range)? {
        RegisterShape::Line
    } else {
        text_object_shape(motion)
    };
    Ok(Some((range, shape)))
}

/// Visual inner quote objects select both delimiters when the interior is empty.
pub(super) fn visual_text_object(
    text: &TextBuffer,
    objects: &[SyntaxObject],
    cursor: ByteOffset,
    selection: Selection,
    motion: &str,
    count: usize,
) -> Result<Option<(ByteRange, RegisterShape)>, CommandError> {
    let Some(mut range) = resolve_text_object(
        text,
        objects,
        cursor,
        motion,
        count,
        ObjectUse::Visual(selection),
    )?
    else {
        return Ok(None);
    };
    if range.is_empty()
        && matches!(
            motion,
            "inner_double_quote" | "inner_single_quote" | "inner_backtick"
        )
    {
        // An empty quote range sits between two adjacent, single-byte delimiters.
        let closing = range.start.get();
        range = ByteRange::new(ByteOffset(closing - 1), ByteOffset(closing + 1)).map_err(failed)?;
    }
    Ok(Some((range, text_object_shape(motion))))
}

/// Returns `None` when `motion` is not a text object.
pub(super) fn text_object(
    text: &TextBuffer,
    objects: &[SyntaxObject],
    cursor: ByteOffset,
    motion: &str,
    count: usize,
) -> Result<Option<ByteRange>, CommandError> {
    resolve_text_object(text, objects, cursor, motion, count, ObjectUse::Standard)
}

fn resolve_text_object(
    text: &TextBuffer,
    objects: &[SyntaxObject],
    cursor: ByteOffset,
    motion: &str,
    count: usize,
    usage: ObjectUse,
) -> Result<Option<ByteRange>, CommandError> {
    let (around, kind) = match motion.split_once('_') {
        Some(("inner", kind)) => (false, kind),
        Some(("a", kind)) => (true, kind),
        _ => return Ok(None),
    };
    let mut range = match kind {
        "word" => word(text, cursor, around, false)?,
        "big_word" => word(text, cursor, around, true)?,
        "paragraph" => paragraph(text, cursor, around, count)?,
        "paren" => pair(text, cursor, '(', ')', around, count, usage)?,
        "bracket" => pair(text, cursor, '[', ']', around, count, usage)?,
        "brace" => pair(text, cursor, '{', '}', around, count, usage)?,
        "angle" => pair(text, cursor, '<', '>', around, count, usage)?,
        "double_quote" => quote(text, cursor, '"', around)?,
        "single_quote" => quote(text, cursor, '\'', around)?,
        "backtick" => quote(text, cursor, '`', around)?,
        "function" => syntax(objects, SyntaxObjectKind::Function, cursor, around, count),
        "class" => syntax(objects, SyntaxObjectKind::Class, cursor, around, count),
        "argument" => syntax(objects, SyntaxObjectKind::Argument, cursor, around, count),
        "comment" => syntax(objects, SyntaxObjectKind::Comment, cursor, around, count),
        "tag" => tag(text, objects, cursor, around, count),
        _ => return Ok(None),
    };
    if count > 1 && matches!(kind, "word" | "big_word") {
        let source = text.to_string();
        for _ in 1..count {
            let Some(previous) = range else {
                break;
            };
            let next = source[previous.end.get()..]
                .char_indices()
                .find(|(_, character)| {
                    if around {
                        !character.is_whitespace()
                    } else {
                        *character != '\n'
                    }
                })
                .map(|(offset, _)| ByteOffset(previous.end.get() + offset));
            let Some(next) = next else {
                break;
            };
            let following = word_in(
                &source[next.get()..],
                next.get(),
                next,
                around,
                kind == "big_word",
            )?;
            let Some(following) = following.filter(|following| following.end > previous.end) else {
                break;
            };
            range = Some(ByteRange::new(previous.start, following.end).map_err(failed)?);
        }
    }
    range
        .map(Some)
        .ok_or_else(|| CommandError::Unavailable("no matching text object at the cursor".into()))
}

/// The `count`-th smallest tree-sitter object of `kind` around the cursor, or the next one
/// after it when none surrounds it, like mini.ai.
fn syntax(
    objects: &[SyntaxObject],
    kind: SyntaxObjectKind,
    cursor: ByteOffset,
    around: bool,
    count: usize,
) -> Option<ByteRange> {
    let cursor = cursor.get();
    let mut enclosing = objects
        .iter()
        .filter(|object| object.kind == kind && object.outer.0 <= cursor && cursor < object.outer.1)
        .collect::<Vec<_>>();
    enclosing.sort_by_key(|object| object.outer.1 - object.outer.0);
    let object = if enclosing.is_empty() {
        objects
            .iter()
            .filter(|object| object.kind == kind && object.outer.0 > cursor)
            .min_by_key(|object| object.outer.0)
    } else {
        enclosing.get(count.max(1) - 1).copied()
    }?;
    let (start, end) = if around { object.outer } else { object.inner };
    ByteRange::new(ByteOffset(start), ByteOffset(end)).ok()
}

/// The pair characters a surround or text-object key names, `b`/`B` included.
pub(super) fn pair_for(character: char) -> Option<(char, char)> {
    match character {
        '(' | ')' | 'b' => Some(('(', ')')),
        '[' | ']' => Some(('[', ']')),
        '{' | '}' | 'B' => Some(('{', '}')),
        '<' | '>' => Some(('<', '>')),
        '"' | '\'' | '`' => Some((character, character)),
        _ => None,
    }
}

/// The range of the pair named by `character` around the cursor, including the pair.
pub(super) fn surrounding(
    text: &TextBuffer,
    cursor: ByteOffset,
    character: char,
) -> Result<Option<ByteRange>, CommandError> {
    match pair_for(character) {
        Some((open, close)) if open == close => quote(text, cursor, open, true),
        Some((open, close)) => pair(text, cursor, open, close, true, 1, ObjectUse::Standard),
        None => quote(text, cursor, character, true),
    }
}

fn line_bounds(text: &TextBuffer, cursor: ByteOffset) -> Result<(usize, String), CommandError> {
    let line = text.byte_to_line(cursor).map_err(failed)?;
    let start = text.line_to_byte(line).map_err(failed)?.get();
    let mut content = text.line(line).map_err(failed)?;
    if content.ends_with('\n') {
        content.pop();
    }
    Ok((start, content))
}

fn word(
    text: &TextBuffer,
    cursor: ByteOffset,
    around: bool,
    big: bool,
) -> Result<Option<ByteRange>, CommandError> {
    let (start, content) = line_bounds(text, cursor)?;
    word_in(&content, start, cursor, around, big)
}

fn word_in(
    content: &str,
    start: usize,
    cursor: ByteOffset,
    around: bool,
    big: bool,
) -> Result<Option<ByteRange>, CommandError> {
    let class = |grapheme: &str| {
        let character = grapheme.chars().next().unwrap_or(' ');
        if big {
            u8::from(!character.is_whitespace())
        } else {
            word_class(character)
        }
    };
    let relative = cursor.get().saturating_sub(start);
    let mut characters = content
        .grapheme_indices(true)
        .take_while(|(_, grapheme)| *grapheme != "\n")
        .peekable();
    let Some((mut first, grapheme)) = characters.next() else {
        return Ok(Some(ByteRange::empty(cursor)));
    };
    let mut end = first + grapheme.len();
    let mut current = class(grapheme);
    let mut leading = first;
    while characters
        .peek()
        .is_some_and(|(index, _)| *index <= relative)
    {
        let (index, grapheme) = characters.next().expect("peeked grapheme");
        let next = class(grapheme);
        if next != current {
            leading = if current == 0 { first } else { index };
            first = index;
            current = next;
        }
        end = index + grapheme.len();
    }
    while let Some((index, grapheme)) =
        characters.next_if(|(_, grapheme)| class(grapheme) == current)
    {
        end = index + grapheme.len();
    }
    if around {
        if let Some((_, grapheme)) = characters.peek()
            && (current == 0 || class(grapheme) == 0)
        {
            let next = class(grapheme);
            while let Some((index, grapheme)) =
                characters.next_if(|(_, grapheme)| class(grapheme) == next)
            {
                end = index + grapheme.len();
            }
        } else if current != 0 {
            first = leading;
        }
    }
    ByteRange::new(ByteOffset(start + first), ByteOffset(start + end))
        .map(Some)
        .map_err(failed)
}

fn paragraph(
    text: &TextBuffer,
    cursor: ByteOffset,
    around: bool,
    count: usize,
) -> Result<Option<ByteRange>, CommandError> {
    let lines = text.visible_lines();
    let blank = |line: usize| -> Result<bool, CommandError> {
        Ok(text.line(line).map_err(failed)?.trim().is_empty())
    };
    let run_end = |first: usize, is_blank: bool| -> Result<usize, CommandError> {
        let mut end = first + 1;
        while end < lines && blank(end)? == is_blank {
            end += 1;
        }
        Ok(end)
    };
    let mut first = text.byte_to_line(cursor).map_err(failed)?.min(lines - 1);
    let is_blank = blank(first)?;
    while first > 0 && blank(first - 1)? == is_blank {
        first -= 1;
    }

    let mut end = first;
    for _ in 0..count.max(1) {
        if end == lines {
            return Ok(None);
        }
        // Each inner count consumes one run, whether blank or not.
        let is_blank = blank(end)?;
        end = run_end(end, is_blank)?;
        if around {
            // Around paragraphs combine a text run with adjacent blanks. A final
            // text run can use leading blanks instead.
            if end < lines {
                end = run_end(end, !is_blank)?;
            } else if is_blank {
                return Ok(None);
            } else {
                while first > 0 && blank(first - 1)? {
                    first -= 1;
                }
            }
        }
    }

    let start = text.line_to_byte(first).map_err(failed)?;
    let end = if end < lines {
        text.line_to_byte(end).map_err(failed)?
    } else {
        ByteOffset(text.len_bytes())
    };
    ByteRange::new(start, end).map(Some).map_err(failed)
}

/// Brackets nest and may span lines. Without an enclosing pair, a count searches
/// forward for the first opener at that depth. Count-one fallback stays on the cursor line.
fn pair(
    text: &TextBuffer,
    cursor: ByteOffset,
    open: char,
    close: char,
    around: bool,
    count: usize,
    usage: ObjectUse,
) -> Result<Option<ByteRange>, CommandError> {
    let source = text.to_string();
    let at = cursor.get().min(source.len());
    // On a closing bracket the scan below finds its own opening bracket first.
    let mut wanted = count.max(1);
    let mut opening = None;
    if source[at..].starts_with(open) {
        wanted -= 1;
        if wanted == 0 {
            opening = Some(at);
        }
    }
    if opening.is_none() {
        let mut depth = 0_usize;
        for (index, character) in source[..at].char_indices().rev() {
            if character == close {
                depth += 1;
            } else if character == open {
                if depth > 0 {
                    depth -= 1;
                    continue;
                }
                wanted -= 1;
                if wanted == 0 {
                    opening = Some(index);
                    break;
                }
            }
        }
    }
    let opening = match opening {
        Some(opening) => opening,
        None if wanted < count.max(1) => return Ok(None),
        None if wanted == 1 => {
            let (start, content) = line_bounds(text, cursor)?;
            let relative = at.saturating_sub(start).min(content.len());
            let Some(found) = content[relative..].find(open) else {
                return Ok(None);
            };
            start + relative + found
        }
        None => {
            let Ok(wanted) = isize::try_from(wanted) else {
                return Ok(None);
            };
            // Like findmatchlimit with an explicit delimiter, advance before
            // inspecting a character. Each further match in current_block
            // starts after the previous match, so track their cumulative depth.
            let mut depth = 0_isize;
            let nested = source[at..]
                .char_indices()
                .skip(1)
                .find_map(|(offset, character)| {
                    if character == open {
                        depth += 1;
                        if depth == wanted {
                            return Some(at + offset);
                        }
                    } else if character == close {
                        depth -= 1;
                    }
                    None
                });
            let Some(nested) = nested else {
                return Ok(None);
            };
            nested
        }
    };
    let mut depth = 0_usize;
    let mut closing = None;
    for (index, character) in source[opening..].char_indices() {
        if character == open {
            depth += 1;
        } else if character == close {
            depth -= 1;
            if depth == 0 {
                closing = Some(opening + index);
                break;
            }
        }
    }
    let Some(closing) = closing else {
        return Ok(None);
    };
    if around {
        return ByteRange::new(ByteOffset(opening), ByteOffset(closing + close.len_utf8()))
            .map(Some)
            .map_err(failed);
    }
    let opener_end = opening + open.len_utf8();
    let interior = &source[opener_end..closing];
    // current_block advances past the opener and skips an immediate newline,
    // but whitespace following the opener is part of the object.
    let starts_on_body_line = interior.starts_with('\n');
    let mut start = opener_end + usize::from(starts_on_body_line);
    let mut end = closing;
    let is_indent = |line: &str| line.bytes().all(|byte| matches!(byte, b' ' | b'\t'));
    if let Some((body, closer_indent)) = interior.rsplit_once('\n')
        && is_indent(closer_indent)
    {
        end = opener_end + body.len() + 1;
        let first_body_line = opener_end + body.find('\n').unwrap_or(body.len()) + 1;
        // Retreat over indentation on complete lines, stopping at a truly
        // empty line or the opener line (whose padding is not indentation).
        while let Some(body) = source[first_body_line..end].strip_suffix('\n') {
            let line = body.rsplit('\n').next().unwrap_or_default();
            if line.is_empty() || !is_indent(line) {
                break;
            }
            end -= line.len() + 1;
        }
        // A direct operator starting mid-line and ending exclusively at the
        // next line's first column stops before the final newline instead.
        if matches!(usage, ObjectUse::Standard) && !starts_on_body_line {
            end -= 1;
        } else if let ObjectUse::Visual(selection) = usage
            && end == start
            && start < closing
            && (opening..=closing).contains(&at)
        {
            let old_start = selection.anchor.min(selection.head).get();
            let old_end = selection.anchor.max(selection.head).get();
            // current_block compares the raw anchor (inner start) and head
            // (trimmed back to the opener) before the sol adjustment. If
            // neither extends the old area, leave the object empty.
            if start < old_start || old_end < opening {
                // sol moves the head to the opener newline. The inner
                // anchor is inclusive, so keep the first indentation byte.
                end = start + 1;
                start = opener_end;
            }
        }
    }
    ByteRange::new(ByteOffset(start), ByteOffset(end))
        .map(Some)
        .map_err(failed)
}

/// Quotes pair up from the start of the line, skipping backslash-escaped quotes.
fn quote(
    text: &TextBuffer,
    cursor: ByteOffset,
    mark: char,
    around: bool,
) -> Result<Option<ByteRange>, CommandError> {
    let (start, content) = line_bounds(text, cursor)?;
    let relative = cursor.get().saturating_sub(start);
    let mut positions = Vec::new();
    let mut escaped = false;
    for (index, character) in content.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && mark != '\\' {
            escaped = true;
        } else if character == mark {
            positions.push(index);
        }
    }
    let pairs = positions
        .chunks_exact(2)
        .map(|pair| (pair[0], pair[1]))
        .collect::<Vec<_>>();
    let Some((opening, closing)) = pairs
        .iter()
        .find(|(opening, closing)| *opening <= relative && relative <= *closing)
        .or_else(|| pairs.iter().find(|(opening, _)| *opening > relative))
        .copied()
    else {
        return Ok(None);
    };
    let (from, to) = if around {
        (opening, closing + mark.len_utf8())
    } else {
        (opening + mark.len_utf8(), closing)
    };
    ByteRange::new(ByteOffset(start + from), ByteOffset(start + to))
        .map(Some)
        .map_err(failed)
}

/// A tag pair (`it`/`at`): from the buffer's parse where its grammar has elements (HTML,
/// XML, JSX), else the nearest enclosing `<name …>…</name>` pair in the text.
fn tag(
    text: &TextBuffer,
    objects: &[SyntaxObject],
    cursor: ByteOffset,
    around: bool,
    count: usize,
) -> Option<ByteRange> {
    if objects
        .iter()
        .any(|object| object.kind == SyntaxObjectKind::Tag)
        && let Some(range) = syntax(objects, SyntaxObjectKind::Tag, cursor, around, count)
    {
        return Some(range);
    }
    let source = text.to_string();
    let at = cursor.get();
    let pairs = tag_pairs(&source);
    let mut enclosing = pairs
        .iter()
        .filter(|pair| pair.0 <= at && at < pair.3)
        .collect::<Vec<_>>();
    enclosing.sort_by_key(|pair| pair.3 - pair.0);
    let pair = if enclosing.is_empty() {
        pairs
            .iter()
            .filter(|pair| pair.0 > at)
            .min_by_key(|pair| pair.0)
    } else {
        enclosing.get(count.max(1) - 1).copied()
    }?;
    let (start, end) = if around {
        (pair.0, pair.3)
    } else {
        (pair.1, pair.2)
    };
    ByteRange::new(ByteOffset(start), ByteOffset(end)).ok()
}

/// Each matched tag pair as (opening start, opening end, closing start, closing end).
/// Comments, `<!…>` and `<?…>` are skipped, self-closing tags never pair, and an opening
/// tag with no closing one (`<br>`, a generic `Vec<u8>`) is dropped when an outer tag closes.
fn tag_pairs(source: &str) -> Vec<(usize, usize, usize, usize)> {
    let mut stack: Vec<(&str, usize, usize)> = Vec::new();
    let mut pairs = Vec::new();
    let mut index = 0;
    while let Some(found) = source[index..].find('<') {
        let start = index + found;
        let rest = &source[start..];
        if rest.starts_with("<!--") {
            index = rest.find("-->").map_or(source.len(), |end| start + end + 3);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            index = rest.find('>').map_or(source.len(), |end| start + end + 1);
            continue;
        }
        let closing = rest.starts_with("</");
        let name_start = start + if closing { 2 } else { 1 };
        let name_end = source[name_start..]
            .find(|character: char| {
                !(character.is_alphanumeric() || matches!(character, '-' | '_' | ':' | '.'))
            })
            .map_or(source.len(), |offset| name_start + offset);
        let name = &source[name_start..name_end];
        // A name starts with a letter; an empty one is a JSX fragment, `<>` or `</>`.
        let named = name.chars().next().is_some_and(char::is_alphabetic);
        if !named && !(name.is_empty() && source[name_end..].starts_with('>')) {
            index = start + 1;
            continue;
        }
        let Some(end) = tag_end(source, name_end) else {
            index = start + 1;
            continue;
        };
        if closing {
            if let Some(depth) = stack.iter().rposition(|(open, _, _)| *open == name) {
                let (_, open_start, open_end) = stack[depth];
                stack.truncate(depth);
                pairs.push((open_start, open_end, start, end));
            }
        } else if !source[..end].ends_with("/>") {
            stack.push((name, start, end));
        }
        index = end;
    }
    pairs
}

/// The byte after a tag's `>`, past quoted attribute values and JSX `{…}` expressions.
fn tag_end(source: &str, from: usize) -> Option<usize> {
    let mut quote = None;
    let mut braces = 0_usize;
    for (offset, character) in source[from..].char_indices() {
        match (quote, character) {
            (Some(mark), _) if character == mark => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') if braces == 0 => quote = Some(character),
            (None, '{') => braces += 1,
            (None, '}') => braces = braces.saturating_sub(1),
            (None, '>') if braces == 0 => return Some(from + offset + 1),
            (None, '<') if braces == 0 => return None,
            _ => {}
        }
    }
    None
}
