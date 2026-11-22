//! GitHub's suggested changes: a ```` ```suggestion ```` block in a review
//! comment reads as the lines it replaces (`−`) and the lines it suggests (`+`), not as code.

use super::lines::Line;
use super::prose::{Rich, role, strong};
use super::*;
use crate::forge::{provider::ProviderKind, request::Write};
use crate::ui::{GitRole, GitSurface, parse_unified_patch};
use unicode_width::UnicodeWidthStr;

/// A comment body cut at its suggestion blocks.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Part {
    Text(String),
    /// The suggested lines (empty: delete the commented lines).
    Suggestion(Vec<String>),
}

/// The fence that opens a suggestion: three or more backticks or tildes, then `suggestion`.
fn opens(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim();
    let mark = trimmed
        .chars()
        .next()
        .filter(|mark| *mark == '`' || *mark == '~')?;
    let run = trimmed.chars().take_while(|ch| *ch == mark).count();
    (run >= 3 && trimmed[run..].trim() == "suggestion").then_some((mark, run))
}

fn closes(line: &str, (mark, run): (char, usize)) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= run && trimmed.chars().all(|ch| ch == mark)
}

/// `body` cut into prose and suggestion blocks; `None` when it holds no suggestion.
pub(super) fn split(body: &str) -> Option<Vec<Part>> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut open: Option<((char, usize), Vec<String>)> = None;
    for line in body.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        match &mut open {
            Some((fence, lines)) => {
                if closes(line, *fence) {
                    parts.push(Part::Suggestion(std::mem::take(lines)));
                    open = None;
                } else {
                    lines.push(line.to_owned());
                }
            }
            None => match opens(line) {
                Some(fence) => {
                    if !text.trim().is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    text.clear();
                    open = Some((fence, Vec::new()));
                }
                None => {
                    text.push_str(line);
                    text.push('\n');
                }
            },
        }
    }
    // An unclosed block runs to the end, as in GitHub's Markdown.
    if let Some((_, lines)) = open {
        parts.push(Part::Suggestion(lines));
    }
    if !text.trim().is_empty() {
        parts.push(Part::Text(text));
    }
    parts
        .iter()
        .any(|part| matches!(part, Part::Suggestion(_)))
        .then_some(parts)
}

/// The suggested lines of the first suggestion in `body`.
pub(super) fn first(body: &str) -> Option<Vec<String>> {
    split(body)?.into_iter().find_map(|part| match part {
        Part::Suggestion(lines) => Some(lines),
        Part::Text(_) => None,
    })
}

/// Read exactly the requested right-side lines from a GitHub hunk excerpt.
fn original(hunk: &str, start: u64, end: u64) -> Option<Vec<String>> {
    let header = hunk.lines().next()?;
    let (left, _) = header.strip_prefix("@@ -")?.split_once(" @@")?;
    let (_, right) = left.split_once(" +")?;
    let (first, count) = match right.split_once(',') {
        Some((first, count)) => (first.parse::<u64>().ok()?, count.parse::<u64>().ok()?),
        None => (right.parse::<u64>().ok()?, 1),
    };
    if first == 0 || start < first || start > end || end - first >= count {
        return None;
    }
    // Use the Files parser's line numbers, including counted blank context lines.
    let excerpt = hunk.split_once("\n@@ ").map_or(hunk, |(first, _)| first);
    let parsed = parse_unified_patch(excerpt, "").hunks.into_iter().next()?;
    if parsed.header != header {
        return None;
    }
    let mut next = start;
    let mut lines = Vec::new();
    for entry in parsed.lines {
        let Some(number) = entry.new_number.map(u64::from) else {
            continue;
        };
        if number < start {
            continue;
        }
        if number != next {
            return None;
        }
        lines.push(entry.text);
        if number == end {
            return Some(lines);
        }
        next += 1;
    }
    None
}

fn right_range(thread: &Value) -> Option<(u64, u64)> {
    if thread["diffSide"] != "RIGHT" || thread["startDiffSide"] == "LEFT" {
        return None;
    }
    let end = thread["line"].as_u64()?;
    let start = thread["startLine"].as_u64().unwrap_or(end);
    (0 < start && start <= end).then_some((start, end))
}

fn original_lines(thread: &Value) -> Option<Vec<String>> {
    if thread["diffSide"] != "RIGHT" || thread["startDiffSide"] == "LEFT" {
        return None;
    }
    let anchor = thread["comments"]["nodes"].as_array()?.first()?;
    let end = thread["originalLine"]
        .as_u64()
        .or_else(|| anchor["originalLine"].as_u64())?;
    let start = match thread["originalStartLine"].as_u64() {
        Some(start) => start,
        // Without the original start, the current start of a range is not a safe fallback.
        None if thread["startLine"].is_number() => return None,
        None => end,
    };
    original(anchor["diffHunk"].as_str()?, start, end)
}

/// Gitea/Forgejo only provide the current coordinates. The hunk excerpt's right-side
/// tail is useful for display, but isn't proof of the original coordinates for a write.
fn preview_hunk_tail(thread: &Value) -> Option<Vec<String>> {
    let (start, end) = right_range(thread)?;
    let anchor = thread["comments"]["nodes"].as_array()?.first()?;
    if thread["originalLine"].is_number() || anchor["originalLine"].is_number() {
        return None;
    }
    let count = usize::try_from(end - start + 1).ok()?;
    let mut lines: Vec<_> = parse_unified_patch(anchor["diffHunk"].as_str()?, "")
        .hunks
        .into_iter()
        .flat_map(|hunk| hunk.lines)
        .filter(|entry| entry.new_number.is_some())
        .map(|entry| entry.text)
        .collect();
    Some(lines.split_off(lines.len().saturating_sub(count)))
}

/// Compare the historical text with the current right side, without borrowing coordinates
/// from a different file or from an earlier revision of the patch.
fn current_lines_match(patch: &str, path: &str, start: u64, end: u64, expected: &[String]) -> bool {
    use super::super::preview::header_path;
    use std::path::Path;

    patch.split("\ndiff --git ").any(|file| {
        let current = file
            .lines()
            .take_while(|line| !line.starts_with("@@"))
            .find_map(|line| header_path(line.strip_prefix("+++ ")?));
        current.as_deref() == Some(Path::new(path))
            && file.match_indices("\n@@ ").any(|(at, _)| {
                original(&file[at + 1..], start, end)
                    .is_some_and(|lines| lines.as_slice() == expected)
            })
    })
}

fn replacement(thread: &Value) -> Option<(&str, Vec<String>)> {
    array(&thread["comments"], "nodes")
        .iter()
        .find_map(|comment| Some((comment["id"].as_str()?, first(comment["body"].as_str()?)?)))
}

/// `Suggested change`, then the replaced lines and the suggested ones, as a diff.
pub(super) fn rows(
    rows: &mut Vec<Line>,
    original: &[String],
    suggested: &[String],
    width: usize,
    indent: &str,
) {
    let mut heading = Rich::new()
        .plain(indent)
        .push("Suggested change", strong(GitRole::Text));
    if original.is_empty() {
        heading = heading.sep().push(
            "the lines it replaces are not in this diff",
            role(GitRole::Muted),
        );
    }
    let mut heading = heading.line();
    heading.band = Some(GitSurface::Code);
    rows.push(heading);
    let room = width.saturating_sub(indent.width() + 2).max(8);
    let sides = original
        .iter()
        .map(|line| ('−', line))
        .chain(suggested.iter().map(|line| ('+', line)));
    for (sign, line) in sides {
        let added = sign == '+';
        let text = super::super::shell::chrome::ellipsize(&line.replace('\t', "    "), room);
        let tone = if added {
            GitRole::Added
        } else {
            GitRole::Removed
        };
        let mut row = Rich::new()
            .plain(indent)
            .push(format!("{sign} "), role(tone))
            .push(text, role(tone))
            .line();
        row.band = Some(if added {
            GitSurface::AddedBand
        } else {
            GitSurface::RemovedBand
        });
        rows.push(row);
    }
    if suggested.is_empty() {
        let mut row = Rich::new()
            .plain(indent)
            .push("Deletes these lines", role(GitRole::Muted))
            .line();
        row.band = Some(GitSurface::Code);
        rows.push(row);
    }
}

/// The reviewed lines and their revision, kept with the form until it submits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::screen) struct Snapshot {
    target: ForgeTarget,
    pub(super) number: u64,
    thread: String,
    comment: String,
    path: String,
    start: u64,
    end: u64,
    original: Vec<String>,
    replacement: Vec<String>,
    branch: String,
    head: String,
}

fn lines_text(start: u64, end: u64) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}–{end}")
    }
}

/// The default commit message, GitHub's.
const MESSAGE: &str = "Apply suggestion from code review";

impl Workspace {
    fn suggestion_thread(&self, number: u64, id: &str) -> Option<&Value> {
        self.review_threads(number)
            .into_iter()
            .find(|thread| thread["id"] == id)
    }

    pub(super) fn suggestion_original(&self, number: u64, id: &str) -> Vec<String> {
        self.suggestion_thread(number, id)
            .map(|thread| self.suggestion_preview(thread))
            .unwrap_or_default()
    }

    pub(super) fn suggestion_preview(&self, thread: &Value) -> Vec<String> {
        original_lines(thread)
            .or_else(|| match self.forge_kind() {
                Some(ProviderKind::Gitea | ProviderKind::Forgejo) => preview_hunk_tail(thread),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn can_commit_suggestion(&self) -> bool {
        !self.read_only()
            && !matches!(
                self.forge_kind(),
                Some(ProviderKind::GitLab | ProviderKind::Gitea | ProviderKind::Forgejo)
            )
    }

    /// Conversation and Activity only need the Files patch when they can offer a suggestion.
    pub(super) fn wants_suggestion_patch(&self, number: u64) -> bool {
        matches!(self.pr_tab, pr::CONVERSATION | 4 | 5)
            && self.can_commit_suggestion()
            && self.review_threads(number).into_iter().any(|thread| {
                thread["isResolved"] == false
                    && right_range(thread).is_some()
                    && replacement(thread).is_some()
                    && original_lines(thread).is_some()
            })
    }

    fn suggestion(&self, number: u64, id: &str) -> Option<Snapshot> {
        if !self.can_commit_suggestion() {
            return None;
        }
        let thread = self.suggestion_thread(number, id)?;
        if thread["isResolved"] != false {
            return None;
        }
        let head = self.review_head(number);
        self.check_inline_head(number, head).ok()?;
        let branch = string(self.value(&Key::Item(2, number))?, "headRefName");
        let path = string(thread, "path");
        if branch.is_empty() || path.is_empty() {
            return None;
        }
        let (start, end) = right_range(thread)?;
        let original = original_lines(thread)?;
        let (comment, replacement) = replacement(thread)?;
        let patch = self.raw(&Key::Diff(number))?;
        if !current_lines_match(patch, path, start, end, &original) {
            return None;
        }
        Some(Snapshot {
            target: self.target(),
            number,
            thread: id.to_owned(),
            comment: comment.to_owned(),
            path: path.to_owned(),
            start,
            end,
            original,
            replacement,
            branch: branch.to_owned(),
            head: head.to_owned(),
        })
    }

    /// Offer the action only on a signed-in, open thread with a current right-side hunk.
    pub(super) fn offers_commit(&self, number: u64, thread: &str) -> bool {
        self.suggestion(number, thread).is_some()
    }

    /// The sheet for `Commit suggestion`: the commit message, GitHub's default filled in.
    pub(super) fn suggestion_form(&self, action: &Action) -> Result<Option<Form>, String> {
        let Action::CommitSuggestion(number, thread) = action else {
            return Ok(None);
        };
        self.check_inbox_action(action)?;
        let snapshot = self
            .suggestion(*number, thread)
            .ok_or("That suggestion is no longer on the pull request · R refreshes")?;
        let fields = vec![super::actions::field(
            "Commit message",
            MESSAGE,
            true,
            false,
        )];
        let title = format!(
            "Commit suggestion to {}:{}",
            snapshot.path,
            lines_text(snapshot.start, snapshot.end)
        );
        Ok(Some(self.form(
            &title,
            Action::ConfirmSuggestion(Box::new(snapshot)),
            fields,
        )))
    }

    /// The line under the sheet's title: where the commit goes and what it changes.
    pub(super) fn suggestion_note(&self, snapshot: &Snapshot) -> String {
        let words = |count: usize| format!("{count} line{}", if count == 1 { "" } else { "s" });
        let replaced = words(snapshot.original.len());
        let change = if snapshot.replacement.is_empty() {
            format!("deletes {replaced}")
        } else {
            format!(
                "replaces {replaced} with {}",
                words(snapshot.replacement.len())
            )
        };
        format!("Commits to {} · {change}", snapshot.branch)
    }

    /// The write uses exactly the destination and suggestion confirmed in the sheet.
    pub(super) fn suggestion_write(
        &self,
        snapshot: &Snapshot,
        values: &[String],
    ) -> Result<Write, String> {
        if self.suggestion(snapshot.number, &snapshot.thread).as_ref() != Some(snapshot) {
            return Err("The suggestion changed. Refresh and open it again.".into());
        }
        let message = values
            .first()
            .map(|message| message.trim())
            .filter(|message| !message.is_empty())
            .unwrap_or(MESSAGE);
        Ok(Write::CommitSuggestion {
            number: snapshot.number,
            path: snapshot.path.clone(),
            start: snapshot.start,
            end: snapshot.end,
            original: snapshot.original.clone(),
            replacement: snapshot.replacement.clone(),
            head: snapshot.head.clone(),
            message: message.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suggestion_block_splits_from_the_prose_around_it() {
        let body = "Name it after what it bounds.\r\n```suggestion\r\n    bounded = min(attempt, 6)\r\n```\r\nThanks!";
        assert_eq!(
            split(body),
            Some(vec![
                Part::Text("Name it after what it bounds.\n".into()),
                Part::Suggestion(vec!["    bounded = min(attempt, 6)".into()]),
                Part::Text("Thanks!\n".into()),
            ])
        );
        assert_eq!(split("```rust\nfn main() {}\n```"), None);
        assert_eq!(first("````suggestion\n````"), Some(Vec::new()));
    }

    #[test]
    fn a_hunk_must_cover_the_exact_right_side_range() {
        let hunk = "@@ -1,3 +5,4 @@\n keep\n-old\n+one\n+two\n\\ No newline at end of file";
        assert_eq!(original(hunk, 6, 7), Some(vec!["one".into(), "two".into()]));
        assert_eq!(
            original(hunk, 5, 6),
            Some(vec!["keep".into(), "one".into()])
        );
        assert_eq!(original(hunk, 6, 8), None);
        assert_eq!(original(hunk, 1, 2), None);
    }

    #[test]
    fn a_suggestion_requires_a_complete_right_side_hunk_on_the_current_head() {
        let mut workspace = Workspace::new(PathBuf::new());
        let number = 7;
        let key = Key::Threads(number);
        let head = "a".repeat(40);
        workspace.cache.insert(
            Key::Item(2, number),
            Ok(Reply {
                text: String::new(),
                value: serde_json::json!({"headRefOid": head, "headRefName": "feature"}),
            }),
        );
        const PATCH: &str = "diff --git a/file.rs b/file.rs\n--- a/file.rs\n+++ b/file.rs\n@@ -1,4 +1,4 @@\n keep\n\n one\n-old\n+two\n";
        workspace.cache.insert(
            Key::Diff(number),
            Ok(Reply {
                text: PATCH.into(),
                value: serde_json::json!({"headRefOid": head}),
            }),
        );
        let mut thread = serde_json::json!({
            "id": "THREAD", "isResolved": false, "path": "file.rs", "line": 4,
            "startLine": 3, "originalLine": 4, "originalStartLine": 3,
            "diffSide": "RIGHT", "startDiffSide": "RIGHT", "diffHeadOid": head,
            "comments": { "nodes": [{
                "id": "COMMENT", "body": "```suggestion\nnew\n```"
            }]}
        });
        let wrap = |thread| Reply {
            text: String::new(),
            value: serde_json::json!({"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[thread]}}}}}),
        };
        workspace
            .cache
            .insert(key.clone(), Ok(wrap(thread.clone())));
        assert!(!workspace.offers_commit(number, "THREAD"), "missing hunk");

        thread["comments"]["nodes"][0]["diffHunk"] = "@@ -1,4 +1,4 @@\n keep\n\n one".into();
        workspace
            .cache
            .insert(key.clone(), Ok(wrap(thread.clone())));
        assert!(!workspace.offers_commit(number, "THREAD"), "short hunk");

        thread["comments"]["nodes"][0]["diffHunk"] = "@@ -1,4 +1,4 @@\n keep\n\n one\n two".into();
        thread["diffSide"] = "LEFT".into();
        workspace
            .cache
            .insert(key.clone(), Ok(wrap(thread.clone())));
        assert!(!workspace.offers_commit(number, "THREAD"), "left side");

        thread["diffSide"] = "RIGHT".into();
        workspace.cache.insert(key, Ok(wrap(thread)));
        assert!(
            workspace.offers_commit(number, "THREAD"),
            "both hunks count the blank context line before the suggestion"
        );
        let snapshot = workspace.suggestion(number, "THREAD").unwrap();
        let Write::CommitSuggestion {
            head: confirmed,
            original,
            ..
        } = workspace.suggestion_write(&snapshot, &[]).unwrap()
        else {
            panic!("expected suggestion write")
        };
        assert_eq!(confirmed, head);
        assert_eq!(original, ["one", "two"]);
        let patch = workspace
            .cache
            .get_mut(&Key::Diff(number))
            .unwrap()
            .as_mut()
            .unwrap();
        patch.text = PATCH.replace("+two", "+different");
        assert!(
            !workspace.offers_commit(number, "THREAD"),
            "the current text differs"
        );

        let new_head = "b".repeat(40);
        let patch = workspace
            .cache
            .get_mut(&Key::Diff(number))
            .unwrap()
            .as_mut()
            .unwrap();
        patch.text = PATCH.into();
        patch.value["headRefOid"] = serde_json::json!(new_head);
        workspace
            .cache
            .get_mut(&Key::Item(2, number))
            .unwrap()
            .as_mut()
            .unwrap()
            .value["headRefOid"] = serde_json::json!(new_head);
        let refreshed = workspace
            .suggestion(number, "THREAD")
            .expect("the unchanged text is still available after both heads advance");
        assert_eq!(refreshed.head, new_head);
        assert_eq!(refreshed.original, snapshot.original);
        assert_eq!(
            workspace.suggestion_write(&snapshot, &[]).unwrap_err(),
            "The suggestion changed. Refresh and open it again."
        );
    }

    #[test]
    fn gitea_and_forgejo_preview_the_hunk_without_an_original_line() {
        let mut workspace = Workspace::new(PathBuf::new());
        let thread = serde_json::json!({
            "line": 9, "diffSide": "RIGHT", "comments": { "nodes": [{
                "line": 9, "diffHunk": "@@ -7,2 +7,3 @@\n keep\n-old\n+inserted\n+replace me"
            }]}
        });
        for host in ["gitea.com", "codeberg.org"] {
            workspace.host = host.into();
            assert_eq!(
                workspace.suggestion_preview(&thread),
                ["replace me"],
                "{host}"
            );
        }
        workspace.host = "github.com".into();
        assert!(workspace.suggestion_preview(&thread).is_empty());
        assert!(
            original_lines(&thread).is_none(),
            "writes require original coordinates"
        );
    }

    #[test]
    fn a_suggestion_reads_as_a_diff() {
        let mut out = Vec::new();
        rows(&mut out, &["old()".into()], &["new()".into()], 60, "  ");
        let text = out.iter().map(|row| row.text.as_str()).collect::<Vec<_>>();
        assert_eq!(text, vec!["  Suggested change", "  − old()", "  + new()"]);
        assert_eq!(out[1].band, Some(GitSurface::RemovedBand));
        assert_eq!(out[2].band, Some(GitSurface::AddedBand));
    }
}
