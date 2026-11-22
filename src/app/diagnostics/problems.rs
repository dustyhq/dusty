//! Persistent Problems dock and the entries it shares with the floating picker.

use crate::app::App;
use crate::app::commands::{AppEffect, register};
use crate::app::diagnostics::{DiagnosticAnchor, DiagnosticKey, document_uri};
use crate::app::tasks::TestKey;
use crate::app::windows::FocusTarget;
use crate::editor::input::{KeyEvent, TerminalMouseInput};
use crate::editor::text::{ByteRange, PositionCodec, PositionEncoding};
use crate::editor::{
    ArgumentSchema, CommandError, CommandRegistry, ContextRequirement, EditorState,
};
use crate::language::lsp::path_from_file_uri;
use crate::ui::render::problems::{ProblemsPanelUi, ProblemsRow};
use crate::windows::{SpatialDirection, WindowRect};
use crate::workspace::{DiagnosticSeverity, ProblemRow};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const WHEEL_ROWS: isize = 3;
const NO_ROOM: &str = "Not enough room for the Problems panel";

/// The backing entries used for both presentations.
pub(in crate::app) enum ProblemEntry<'a> {
    Diagnostic(DiagnosticKey, &'a ProblemRow),
    Test(PathBuf, usize, String, TestKey),
}

#[derive(Clone, Debug)]
enum Target {
    Diagnostic(DiagnosticKey),
    Test(TestKey),
}

#[derive(Clone, Debug)]
enum Identity {
    Diagnostic(Box<ProblemRow>, Option<DiagnosticAnchor>),
    Test,
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum ProblemFile {
    Path(PathBuf),
    Uri(String),
}

impl ProblemFile {
    fn from_path(root: &Path, path: &Path) -> Self {
        Self::Path(problem_path(root, path))
    }

    fn from_uri(root: &Path, uri: &str) -> Self {
        path_from_file_uri(uri).map_or_else(
            || Self::Uri(uri.into()),
            |path| Self::from_path(root, &path),
        )
    }

    fn label(&self, app: &App) -> String {
        match self {
            Self::Path(path) => app.workspace_relative_label(path),
            Self::Uri(uri) if uri.starts_with("dusty-buffer:") => "[Scratch]".into(),
            Self::Uri(uri) => uri.clone(),
        }
    }
}

fn problem_path(root: &Path, path: &Path) -> PathBuf {
    let absolute = root.join(path);
    // Work on path components, not URI spelling. In particular, a matcher may print
    // "./src/…" while the corresponding open document has no dot component.
    let path: PathBuf = absolute.components().collect();
    #[cfg(windows)]
    let path = path_from_file_uri(&crate::language::lsp::file_uri(&path)).unwrap_or(path);
    path
}

fn same_diagnostic(a: &ProblemRow, b: &ProblemRow) -> bool {
    a.owner == b.owner
        && a.uri == b.uri
        && a.severity == b.severity
        && a.source == b.source
        && a.code == b.code
        && a.message == b.message
        && a.data == b.data
}

fn mapped_range(
    row: &ProblemRow,
    anchor: Option<&DiagnosticAnchor>,
    editor: &EditorState,
) -> ByteRange {
    anchor
        .and_then(|anchor| anchor.map_range(editor, row.range))
        .unwrap_or(row.range)
}

struct Item {
    file: ProblemFile,
    path: String,
    identity: Identity,
    target: Target,
    location: String,
    severity: DiagnosticSeverity,
    message: String,
    source: String,
}

/// The dock owns its own list and keyboard focus, independently of the modal picker.
pub(in crate::app) struct ProblemsPanel {
    file: Option<ProblemFile>,
    items: Vec<Item>,
    rows: Arc<[ProblemsRow]>,
    // Visual rows include file headers, whereas the selection advances by problem.
    row_for_item: Vec<usize>,
    selected: usize,
    scroll: usize,
    capacity: usize,
    clicked: Option<(usize, Instant)>,
}

impl ProblemsPanel {
    fn new(file: Option<ProblemFile>) -> Self {
        Self {
            file,
            items: Vec::new(),
            rows: Arc::from([]),
            row_for_item: Vec::new(),
            selected: 0,
            scroll: 0,
            capacity: 0,
            clicked: None,
        }
    }

    fn replace(&mut self, mut items: Vec<Item>, editor: &EditorState) {
        items.sort_by(|a, b| (&a.path, &a.file, a.severity).cmp(&(&b.path, &b.file, b.severity)));
        if let Some(old) = self.items.get(self.selected) {
            let found = match &old.identity {
                Identity::Diagnostic(old, anchor) => {
                    let range = mapped_range(old, anchor.as_ref(), editor);
                    // If edits collapse ranges, preserve the selected occurrence in the
                    // merged range too, using the old source order among equal diagnostics.
                    let occurrence = self.items[..self.selected]
                        .iter()
                        .filter(|item| {
                            matches!(&item.identity, Identity::Diagnostic(previous, anchor)
                            if same_diagnostic(old, previous)
                                && mapped_range(previous, anchor.as_ref(), editor) == range)
                        })
                        .count();
                    items
                        .iter()
                        .enumerate()
                        .filter_map(|(index, item)| match &item.identity {
                            Identity::Diagnostic(new, _)
                                if new.range == range && same_diagnostic(old, new) =>
                            {
                                Some(index)
                            }
                            _ => None,
                        })
                        .nth(occurrence)
                }
                Identity::Test => items.iter().position(|item| {
                    matches!((&old.target, &item.target),
                        (Target::Test(old), Target::Test(new)) if old.same(new))
                }),
            };
            self.selected = found.unwrap_or(self.selected);
        }
        self.selected = self.selected.min(items.len().saturating_sub(1));
        let mut rows = Vec::new();
        self.row_for_item.clear();
        for group in items.chunk_by(|a, b| a.file == b.file) {
            rows.push(ProblemsRow::File {
                path: group[0].path.clone(),
                count: group.len(),
            });
            for item in group {
                self.row_for_item.push(rows.len());
                rows.push(ProblemsRow::Problem {
                    severity: item.severity,
                    message: item.message.clone(),
                    source: item.source.clone(),
                    location: item.location.clone(),
                });
            }
        }
        self.rows = rows.into();
        self.items = items;
        self.clicked = None;
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.max_scroll());
    }

    fn max_scroll(&self) -> usize {
        self.rows.len().saturating_sub(self.capacity)
    }

    fn projection(&mut self, capacity: usize, reveal: bool) -> ProblemsPanelUi {
        let resized = self.capacity != capacity;
        self.capacity = capacity;
        if (reveal || resized)
            && let Some(&row) = self.row_for_item.get(self.selected)
        {
            if row < self.scroll {
                self.scroll = row.saturating_sub(1);
            } else if row >= self.scroll.saturating_add(capacity) {
                self.scroll = (row + 1).saturating_sub(capacity);
            }
        }
        self.scroll = self.scroll.min(self.max_scroll());
        ProblemsPanelUi {
            rows: Arc::clone(&self.rows),
            scroll: self.scroll,
            selected_row: self.row_for_item.get(self.selected).copied(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PanelCommand {
    Toggle,
    ThisFile,
    Next,
    Previous,
    Jump,
    JumpAndClose,
    Close,
}

pub(super) fn register_commands(
    registry: &mut CommandRegistry<AppEffect>,
) -> Result<(), CommandError> {
    for (id, label, command) in [
        (
            "diagnostic.panel.toggle",
            "Problems panel",
            PanelCommand::Toggle,
        ),
        (
            "diagnostic.panel.buffer",
            "Problems (this file)",
            PanelCommand::ThisFile,
        ),
        ("diagnostic.panel.next", "Next problem", PanelCommand::Next),
        (
            "diagnostic.panel.previous",
            "Previous problem",
            PanelCommand::Previous,
        ),
        ("diagnostic.panel.jump", "Go to problem", PanelCommand::Jump),
        (
            "diagnostic.panel.jump_close",
            "Go to problem and close panel",
            PanelCommand::JumpAndClose,
        ),
        (
            "diagnostic.panel.close",
            "Close Problems panel",
            PanelCommand::Close,
        ),
    ] {
        register(
            registry,
            id,
            label,
            ContextRequirement::Application,
            ArgumentSchema::default(),
            move |_| AppEffect::ProblemsPanel(command),
        )?;
    }
    Ok(())
}

impl App {
    pub(in crate::app) fn problem_entries(&self) -> impl Iterator<Item = ProblemEntry<'_>> {
        self.diagnostics
            .choices()
            .map(|(key, problem, _)| ProblemEntry::Diagnostic(key, problem))
            .chain(
                self.tasks
                    .failed_tests()
                    .into_iter()
                    .map(|(path, line, summary, key)| ProblemEntry::Test(path, line, summary, key)),
            )
    }

    pub(in crate::app) fn jump_to_problem_diagnostic(&mut self, key: &DiagnosticKey) {
        if let Some(row) = self.diagnostics.resolve(key, &self.state) {
            let (uri, range) = (row.uri.clone(), row.range);
            self.navigate_byte_location(&uri, range);
        } else {
            self.show_error("That problem is no longer available");
        }
    }

    pub(in crate::app) fn jump_to_problem_test(&mut self, key: &TestKey) {
        if let Some(mut source) = self.tasks.resolve_test(key) {
            let root = &self.state.workspace_root;
            let path = problem_path(root, &source.path);
            source.path = self
                .state
                .buffers
                .values()
                .filter_map(|buffer| buffer.path.as_ref())
                .find(|open| problem_path(root, open) == path)
                .cloned()
                .unwrap_or(path);
            self.navigate_matcher_source(source);
        } else {
            self.show_error("That problem is no longer available");
        }
    }

    pub(in crate::app) fn refresh_problems_panel(&mut self) {
        let Some(panel) = &self.problems_panel else {
            return;
        };
        let root = &self.state.workspace_root;
        let matches =
            |candidate: &ProblemFile| panel.file.as_ref().is_none_or(|file| file == candidate);
        let first_line =
            |message: &str| message.lines().next().unwrap_or_default().trim().to_owned();
        let items = self
            .problem_entries()
            .filter_map(|entry| match entry {
                ProblemEntry::Diagnostic(key, problem) => {
                    let file = ProblemFile::from_uri(root, &problem.uri);
                    if !matches(&file) {
                        return None;
                    }
                    let path = file.label(self);
                    let location = self
                        .diagnostics
                        .point(problem, &self.state)
                        .map_or_else(String::new, |(line, column)| {
                            format!("{}:{}", line + 1, column + 1)
                        });
                    let code = problem.code.as_ref().map(|code| {
                        code.as_str()
                            .map_or_else(|| code.to_string(), str::to_owned)
                    });
                    let source = match code {
                        Some(code) if problem.source.is_empty() => code,
                        Some(code) => format!("{} {}", problem.source, code),
                        None => problem.source.clone(),
                    };
                    Some(Item {
                        file,
                        path,
                        identity: Identity::Diagnostic(
                            Box::new(problem.clone()),
                            self.diagnostics.anchor(problem),
                        ),
                        target: Target::Diagnostic(key),
                        severity: problem.severity,
                        message: first_line(&problem.message),
                        source,
                        location,
                    })
                }
                ProblemEntry::Test(path, _, summary, key) => {
                    let file = ProblemFile::from_path(root, &path);
                    if !matches(&file) {
                        return None;
                    }
                    let source = self.tasks.resolve_test(&key)?;
                    let location = self.test_problem_location(&file, &source);
                    Some(Item {
                        path: file.label(self),
                        file,
                        identity: Identity::Test,
                        severity: DiagnosticSeverity::Error,
                        message: first_line(&summary),
                        source: key.name.clone(),
                        target: Target::Test(key),
                        location,
                    })
                }
            })
            .collect();
        if let Some(panel) = &mut self.problems_panel {
            panel.replace(items, &self.state);
        }
        self.project_problems_panel(true);
    }

    fn test_problem_location(
        &self,
        file: &ProblemFile,
        source: &crate::app::picker::MatcherSource,
    ) -> String {
        // Display one-based Unicode scalar columns, never raw UTF-8/UTF-16 units.
        // A matcher for an unopened file has no text snapshot; show just its line if
        // converting the column would require a speculative read.
        let root = &self.state.workspace_root;
        let text = self.state.buffers.values().find_map(|buffer| {
            let path = buffer.path.as_deref()?;
            (ProblemFile::from_path(root, path) == *file).then_some(&buffer.text)
        });
        let line = source.line.saturating_add(1);
        let column = if let Some(text) = text {
            let codec = PositionCodec::new(text);
            codec
                .lsp_to_byte(source.line, source.column, source.encoding)
                .ok()
                .and_then(|byte| codec.byte_to_lsp(byte, PositionEncoding::Utf32).ok())
                .map(|(_, column)| column)
        } else {
            (source.encoding == PositionEncoding::Utf32).then_some(source.column)
        };
        column.map_or_else(
            || line.to_string(),
            |column| format!("{line}:{}", column.saturating_add(1)),
        )
    }

    pub(in crate::app) fn problems_panel_rect(&self) -> Option<WindowRect> {
        let window = self.window_session.native_ids().problems_panel()?.window;
        let rect = self.compositor_projection()?.window(window)?.rect;
        (ProblemsPanelUi::capacity(rect) > 0).then_some(rect)
    }

    pub(in crate::app) fn sync_problems_viewport(&mut self) {
        // Resize invalidates the old projection before publishing its replacement.
        if self.problems_panel.is_some() && self.compositor_projection().is_none() {
            return;
        }
        let capacity = self
            .problems_panel_rect()
            .map_or(0, ProblemsPanelUi::capacity);
        if let Some(panel) = &self.problems_panel
            && panel.capacity != capacity
        {
            self.project_problems_panel(false);
            if capacity == 0 {
                self.notify_warning(NO_ROOM.into());
            }
        } else if self.problems_panel.is_none() && self.ui.problems_panel.is_some() {
            self.ui.problems_panel = None;
        }
    }

    fn project_problems_panel(&mut self, reveal: bool) {
        let capacity = self
            .problems_panel_rect()
            .map_or(0, ProblemsPanelUi::capacity);
        let next = self
            .problems_panel
            .as_mut()
            .map(|panel| panel.projection(capacity, reveal));
        let unchanged = match (&self.ui.problems_panel, &next) {
            (Some(before), Some(after)) => {
                Arc::ptr_eq(&before.rows, &after.rows)
                    && before.scroll == after.scroll
                    && before.selected_row == after.selected_row
            }
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            self.ui.problems_panel = next;
        }
    }

    fn open_problems_panel(&mut self, file: Option<ProblemFile>) {
        self.problems_panel = Some(ProblemsPanel::new(file));
        self.reconcile_editor_windows();
        if self.problems_panel_rect().is_none() {
            self.close_problems_panel();
            self.notify_warning(NO_ROOM.into());
            return;
        }
        self.window_session.focus_problems_panel();
        self.resize_active_terminal_to_panel();
        self.sync_terminal_panel();
        self.refresh_problems_panel();
        self.ensure_cursor_visible();
    }

    pub(in crate::app) fn close_problems_panel(&mut self) {
        if self.problems_panel.take().is_some() {
            self.window_session.focus_editor_from_problems();
            self.resize_active_terminal_to_panel();
            self.sync_terminal_panel();
            self.ensure_cursor_visible();
        }
    }

    fn jump_from_problems_panel(&mut self, close: bool) {
        let Some(target) = self
            .problems_panel
            .as_ref()
            .and_then(|panel| panel.items.get(panel.selected))
            .map(|item| item.target.clone())
        else {
            return;
        };
        if close {
            self.close_problems_panel();
        } else {
            self.window_session.focus_editor_from_problems();
            self.reconcile_editor_windows();
        }
        match target {
            Target::Diagnostic(key) => self.jump_to_problem_diagnostic(&key),
            Target::Test(key) => self.jump_to_problem_test(&key),
        }
    }

    pub(in crate::app) fn select_panel_diagnostic(&mut self, key: &DiagnosticKey) {
        if let Some(panel) = &mut self.problems_panel
            && let Some(index) = panel.items.iter().position(
                |item| matches!(&item.target, Target::Diagnostic(candidate) if key.same(candidate)),
            )
        {
            panel.selected = index;
            panel.clicked = None;
            self.project_problems_panel(true);
        }
    }

    fn move_problems_selection(&mut self, delta: isize) {
        let Some(panel) = &mut self.problems_panel else {
            return;
        };
        let index = panel.selected.saturating_add_signed(delta);
        if index != panel.selected && index < panel.items.len() {
            panel.selected = index;
            panel.clicked = None;
            self.project_problems_panel(true);
        } else if delta < 0 {
            self.focus_surface(SpatialDirection::Up);
        } else {
            self.focus_surface(SpatialDirection::Down);
        }
    }

    pub(super) fn handle_problems_command(&mut self, command: PanelCommand) {
        match command {
            PanelCommand::Toggle if self.problems_panel.is_some() => self.close_problems_panel(),
            PanelCommand::Toggle => self.open_problems_panel(None),
            PanelCommand::ThisFile => {
                let Some(buffer) = self.state.active_buffer_id() else {
                    return;
                };
                let file = self.state.buffers[buffer].path.as_deref().map_or_else(
                    || ProblemFile::Uri(document_uri(buffer, None)),
                    |path| ProblemFile::from_path(&self.state.workspace_root, path),
                );
                self.open_problems_panel(Some(file));
            }
            PanelCommand::Next => self.move_problems_selection(1),
            PanelCommand::Previous => self.move_problems_selection(-1),
            PanelCommand::Jump => self.jump_from_problems_panel(false),
            PanelCommand::JumpAndClose => self.jump_from_problems_panel(true),
            PanelCommand::Close => self.close_problems_panel(),
        }
    }

    pub(in crate::app) fn handle_problems_mouse(
        &mut self,
        key: &KeyEvent,
        column: u16,
        row: u16,
    ) -> bool {
        let Some(rect) = self.problems_panel_rect() else {
            return false;
        };
        let Some(window) = self
            .window_session
            .native_ids()
            .problems_panel()
            .map(|ids| ids.window)
        else {
            return false;
        };
        if self
            .compositor_projection()
            .and_then(|projection| projection.hit_test(column, row))
            != Some(window)
        {
            if key.terminal_mouse() == Some(TerminalMouseInput::LeftDown)
                && let Some(panel) = &mut self.problems_panel
            {
                panel.clicked = None;
            }
            return false;
        }
        let Some(panel) = &mut self.problems_panel else {
            return false;
        };
        match key.terminal_mouse() {
            Some(mouse @ (TerminalMouseInput::ScrollUp | TerminalMouseInput::ScrollDown)) => {
                panel.clicked = None;
                panel.scroll_by(if mouse == TerminalMouseInput::ScrollUp {
                    -WHEEL_ROWS
                } else {
                    WHEEL_ROWS
                });
                self.project_problems_panel(false);
            }
            Some(TerminalMouseInput::LeftDown) => {
                let target = row.checked_sub(rect.y + 1).and_then(|row| {
                    panel
                        .row_for_item
                        .binary_search(&(panel.scroll + usize::from(row)))
                        .ok()
                });
                let now = self.clock.monotonic();
                let double = target.is_some_and(|index| {
                    panel.clicked.is_some_and(|(last, at)| {
                        last == index && now.duration_since(at) <= DOUBLE_CLICK
                    })
                });
                panel.clicked = if double {
                    None
                } else {
                    target.map(|index| (index, now))
                };
                if let Some(index) = target {
                    panel.selected = index;
                }
                if self.window_session.focus() != FocusTarget::ProblemsPanel {
                    self.window_session.focus_problems_panel();
                    self.reconcile_editor_windows();
                }
                self.project_problems_panel(false);
                if double {
                    self.jump_from_problems_panel(false);
                }
            }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::diagnostics::{DiagnosticBatch, DiagnosticDocument, DiagnosticValidity};
    use crate::app::tests::driver::TestSession;
    use crate::editor::BufferId;
    use crate::editor::text::{ByteOffset, ByteRange, TextBuffer};
    use crate::windows::{CompositorLayer, DockEdge, WindowPlacement};
    use crate::workspace::{Diagnostic, DiagnosticOwner, DiagnosticProducer};

    fn keys(session: &mut TestSession, sequence: &[&str]) {
        for key in sequence {
            session.key(key);
        }
    }

    fn publish(
        app: &mut App,
        producer: &DiagnosticProducer,
        buffer: BufferId,
        items: &[(usize, DiagnosticSeverity, &str)],
    ) {
        let uri = document_uri(buffer, app.state.buffers[buffer].path.as_deref());
        let items = items
            .iter()
            .map(|&(start, severity, message)| Diagnostic {
                uri: uri.clone(),
                version: None,
                range: ByteRange::new(ByteOffset(start), ByteOffset(start + 1)).unwrap(),
                severity,
                source: "check".into(),
                code: Some("E123".into()),
                message: message.into(),
                related: Vec::new(),
                tags: Vec::new(),
                fixes: Vec::new(),
                data: None,
                timestamp: std::time::SystemTime::UNIX_EPOCH,
            })
            .collect();
        app.diagnostics
            .publish(
                DiagnosticBatch {
                    producer: producer.clone(),
                    document: DiagnosticDocument::capture(&app.state, buffer),
                    validity: DiagnosticValidity::UnversionedReport,
                    disk_points: Default::default(),
                    uri,
                    items,
                },
                &app.state,
            )
            .unwrap();
        app.sync_ui_diagnostics();
    }

    fn selection(app: &App) -> Option<&str> {
        let panel = app.ui.problems_panel.as_ref()?;
        match panel.rows.get(panel.selected_row?)? {
            ProblemsRow::Problem { message, .. } => Some(message),
            _ => None,
        }
    }

    #[test]
    fn problems_dock_is_live_and_independent_of_the_picker() {
        let mut session = TestSession::new(crate::config::load_bundled_config());
        session.resize(100, 30);
        let app = session.application_mut();
        let root = app.state.workspace_root.clone();
        let (b, _) = app
            .state
            .open_text(
                TextBuffer::from_text("other code\n"),
                Some(root.join("b.txt")),
            )
            .unwrap();
        let (a, view) = app
            .state
            .open_text(
                TextBuffer::from_text("editor code\none\ntwo\n"),
                Some(root.join("a.txt")),
            )
            .unwrap();
        app.set_dashboard_visibility(false);
        app.reconcile_editor_windows();
        let producer = DiagnosticProducer::new(DiagnosticOwner::new("check").unwrap());
        app.diagnostics.activate(producer.clone());
        let first = (12, DiagnosticSeverity::Error, "first");
        let second = (16, DiagnosticSeverity::Warning, "second\nmore detail");
        publish(app, &producer, a, &[first, second]);
        publish(
            app,
            &producer,
            b,
            &[(0, DiagnosticSeverity::Error, "other")],
        );

        keys(&mut session, &["space", "x", "x"]);
        let frame = session.frame();
        let app = session.application();
        let window = app
            .window_session
            .native_ids()
            .problems_panel()
            .unwrap()
            .window;
        let projected = app.compositor_projection().unwrap().window(window).unwrap();
        assert_eq!(projected.layer, CompositorLayer::Docked);
        assert_eq!(
            app.window_session.graph().window(window).unwrap().placement,
            WindowPlacement::Docked {
                edge: DockEdge::Bottom,
                size: 10
            }
        );
        assert_eq!(app.window_session.focus(), FocusTarget::ProblemsPanel);
        assert!((0..projected.rect.y).any(|row| frame.grid.row_text(row).contains("editor code")));
        assert_eq!(selection(app), Some("first"));

        session.key("j");
        assert_eq!(selection(session.application()), Some("second"));
        // Same producer and file, new batch and row indices. Keep the warning selected.
        publish(session.application_mut(), &producer, a, &[second]);
        assert_eq!(selection(session.application()), Some("second"));

        session.key("enter");
        let app = session.application();
        assert_eq!(app.window_session.focus(), FocusTarget::Editor);
        assert_eq!(app.state.views[view].cursor, ByteOffset(16));
        assert!(app.ui.problems_panel.is_some());

        publish(session.application_mut(), &producer, a, &[first, second]);
        keys(&mut session, &["[", "d"]);
        assert_eq!(selection(session.application()), Some("first"));
        keys(&mut session, &["]", "d"]);
        assert_eq!(selection(session.application()), Some("second"));

        keys(&mut session, &["space", "f", "f"]);
        assert_readable_with_picker(&mut session, crate::ui::PickerSurfaceKind::Files, "second");
        session.application_mut().close_picker();
        session.open_grep();
        assert_readable_with_picker(&mut session, crate::ui::PickerSurfaceKind::Grep, "second");
        session.application_mut().close_picker();

        keys(&mut session, &["space", "x", "X"]);
        let app = session.application();
        let panel = app.ui.problems_panel.as_ref().unwrap();
        let files: Vec<_> = panel
            .rows
            .iter()
            .filter_map(|row| match row {
                ProblemsRow::File { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(files, ["a.txt"]);
        session.key("o");
        assert!(session.application().ui.problems_panel.is_none());
        assert_eq!(
            session.application().state.views[view].cursor,
            ByteOffset(12)
        );

        keys(&mut session, &["space", "x", "x"]);
        assert!(session.application().ui.problems_panel.is_some());
        keys(&mut session, &["space", "x", "x"]);
        assert!(session.application().ui.problems_panel.is_none());

        keys(&mut session, &["space", "s", "d"]);
        assert_eq!(
            session.application().ui.picker.as_ref().unwrap().surface,
            crate::ui::PickerSurfaceKind::Problems
        );
    }

    fn document(
        session: &mut TestSession,
        text: &str,
    ) -> (BufferId, crate::editor::ViewId, DiagnosticProducer) {
        let app = session.application_mut();
        let path = app.state.workspace_root.join("document.txt");
        let (buffer, view) = app
            .state
            .open_text(TextBuffer::from_text(text), Some(path))
            .unwrap();
        app.set_dashboard_visibility(false);
        app.reconcile_editor_windows();
        let producer = DiagnosticProducer::new(DiagnosticOwner::new("check").unwrap());
        app.diagnostics.activate(producer.clone());
        (buffer, view, producer)
    }

    fn assert_readable_with_picker(
        session: &mut TestSession,
        kind: crate::ui::PickerSurfaceKind,
        message: &str,
    ) {
        let frame = session.frame();
        let app = session.application();
        assert_eq!(app.ui.picker.as_ref().unwrap().surface, kind);
        let layout = app.window_session.layout().unwrap();
        let picker = layout
            .projection
            .window(layout.surfaces.picker.unwrap())
            .unwrap()
            .rect;
        let panel = app.problems_panel_rect().unwrap();
        assert!(picker.y + picker.height <= panel.y);
        assert!(frame.grid.row_text(panel.y).contains("Problems"));
        assert!(
            (panel.y + 1..panel.y + panel.height)
                .any(|row| frame.grid.row_text(row).contains(message))
        );
    }

    #[test]
    fn terminal_drag_and_release_are_captured_over_problems() {
        use crate::editor::input::KeyCode::{MouseLeftDown, MouseLeftDrag, MouseLeftUp};
        use crate::process::PtyId;
        let mut session = TestSession::new(crate::config::load_bundled_config());
        session.resize(100, 30);
        document(&mut session, "editor code");
        session.terminal_fixture();
        let app = session.application_mut();
        app.place_active_terminal(WindowPlacement::Docked {
            edge: DockEdge::Bottom,
            size: 8,
        });
        app.set_terminal_focus(false);
        app.reconcile_editor_windows();
        keys(&mut session, &["space", "x", "x"]);
        let terminal = session.application().terminal_rect(PtyId(1)).unwrap();
        let panel = session.application().problems_panel_rect().unwrap();
        let drag_y = terminal.y - 2;
        assert!(panel.y <= drag_y && drag_y < panel.y + panel.height);

        session.mouse(MouseLeftDown, 70, terminal.y);
        assert!(session.application().dock_drag.is_some());
        session.mouse(MouseLeftDrag, 70, drag_y);
        let app = session.application();
        let resized = app.terminal_rect(PtyId(1)).unwrap();
        assert_eq!(resized.y, drag_y);
        assert_eq!(resized.height, terminal.height + 2);
        assert_eq!(
            app.terminal_session.fixture_dimensions(PtyId(1)),
            (usize::from(resized.width), usize::from(resized.height - 1))
        );
        let release_y = app.problems_panel_rect().unwrap().y + 1;
        session.mouse(MouseLeftUp, 70, release_y);
        assert!(session.application().dock_drag.is_none());
    }

    #[test]
    fn editor_divider_drag_and_release_are_captured_over_problems() {
        use crate::app::windows::LayoutAction;
        use crate::editor::input::KeyCode::{MouseLeftDown, MouseLeftDrag, MouseLeftUp};
        use crate::windows::TileAxis;

        let mut session = TestSession::new(crate::config::load_bundled_config());
        session.resize(100, 30);
        document(&mut session, "editor code");
        session
            .application_mut()
            .process_effect(AppEffect::Layout(LayoutAction::Split(TileAxis::Horizontal)));
        keys(&mut session, &["space", "x", "x"]);
        let divider = |session: &TestSession| {
            session
                .application()
                .compositor_projection()
                .unwrap()
                .borders
                .iter()
                .copied()
                .find(|border| border.axis == TileAxis::Horizontal)
                .unwrap()
        };
        let initial = divider(&session);
        let panel = session.application().problems_panel_rect().unwrap();
        let drag_x = initial.x + 10;
        let drag_y = panel.y + 1;
        session.mouse(MouseLeftDown, initial.x, initial.y + initial.length - 1);
        assert!(session.application().border_drag.is_some());
        session.mouse(MouseLeftDrag, drag_x, drag_y);
        assert!(divider(&session).x.abs_diff(drag_x) <= 1);

        session.mouse(MouseLeftUp, drag_x, drag_y);
        assert!(session.application().border_drag.is_none());
    }

    #[test]
    fn clicking_editor_text_returns_keyboard_focus_without_closing_problems() {
        use crate::editor::{EditorMode, input::KeyCode::MouseLeftDown};

        for problems_open in [false, true] {
            let mut session = TestSession::new(crate::config::load_bundled_config());
            let (buffer, view, _) = document(&mut session, "first line\nsecond line");
            if problems_open {
                keys(&mut session, &["space", "x", "x"]);
                assert_eq!(
                    session.application().window_session.focus(),
                    FocusTarget::ProblemsPanel
                );
            }
            let app = session.application();
            let pane = app
                .compositor_projection()
                .unwrap()
                .window(app.state.focused_window().unwrap())
                .unwrap()
                .rect;
            let gutter = crate::ui::editor_state_gutter_width(
                &app.state,
                &app.state.buffers[buffer].text,
                &app.configuration.settings().editor,
                pane.width,
            );
            session.mouse(MouseLeftDown, pane.x + gutter, pane.y + 1);
            let app = session.application();
            assert_eq!(app.state.views[view].cursor, ByteOffset(11));
            assert_eq!(app.window_session.focus(), FocusTarget::Editor);
            keys(&mut session, &["i", "z"]);
            assert_eq!(session.editor_mode(), EditorMode::Insert);
            let app = session.application();
            assert_eq!(
                app.state.buffers[buffer].text.to_string(),
                "first line\nzsecond line"
            );
            assert_eq!(app.problems_panel_rect().is_some(), problems_open);
        }
    }

    #[test]
    fn clicking_below_eof_returns_keyboard_focus_without_moving_the_cursor() {
        use crate::editor::{EditorMode, input::KeyCode::MouseLeftDown};

        let mut session = TestSession::new(crate::config::load_bundled_config());
        let (buffer, view, _) = document(&mut session, "editor code");
        keys(&mut session, &["l", "l", "space", "x", "x"]);
        let app = session.application();
        assert_eq!(app.window_session.focus(), FocusTarget::ProblemsPanel);
        let cursor = app.state.views[view].cursor;
        assert_eq!(cursor, ByteOffset(2));
        let pane = crate::ui::active_editor_pane_viewport(&app.state, &app.ui).unwrap();
        assert!(pane.height > 3);

        session.mouse(MouseLeftDown, pane.x + pane.width / 2, pane.y + 3);
        let app = session.application();
        assert_eq!(app.window_session.focus(), FocusTarget::Editor);
        assert_eq!(app.state.views[view].cursor, cursor);
        assert!(app.problems_panel_rect().is_some());
        keys(&mut session, &["i", "z"]);
        assert_eq!(session.editor_mode(), EditorMode::Insert);
        assert_eq!(
            session.application().state.buffers[buffer].text.to_string(),
            "edzitor code"
        );
    }

    fn bottom_dock(size: u16) -> WindowPlacement {
        WindowPlacement::Docked {
            edge: DockEdge::Bottom,
            size,
        }
    }

    fn saved_terminal_rule(
        saved: &crate::persistence::PersistedSession,
    ) -> &crate::persistence::PersistedWindowPlacement {
        &saved
            .window_rules
            .iter()
            .find(|rule| rule.content_type == "builtin.terminal")
            .expect("saved terminal rule")
            .placement
    }

    #[test]
    fn terminal_hide_and_show_restores_the_placement_not_the_problems_cap() {
        use crate::app::windows::NativeSurfaceTarget;
        use crate::persistence::PersistedWindowPlacement;
        use crate::process::PtyId;

        let mut session = TestSession::new(crate::config::load_bundled_config());
        session.resize(80, 24);
        document(&mut session, "editor code");
        session.terminal_fixture();
        let id = PtyId(1);
        let target = NativeSurfaceTarget::Terminal(id.0);
        drag_bottom_terminal(&mut session, id, 20);
        let app = session.application_mut();
        assert_eq!(
            app.current_native_placement(target).unwrap(),
            bottom_dock(20)
        );
        let original = app.terminal_rect(id).unwrap();
        app.set_terminal_focus(false);
        app.reconcile_editor_windows();

        session.resize(80, 16);
        let shrunk = session.application().terminal_rect(id).unwrap();
        assert!(shrunk.height < original.height);
        keys(&mut session, &["space", "x", "x"]);
        let app = session.application();
        assert!(app.terminal_rect(id).unwrap().height < shrunk.height);
        assert_eq!(
            app.current_native_placement(target).unwrap(),
            bottom_dock(20)
        );
        assert_eq!(
            saved_terminal_rule(&app.capture_session().unwrap()),
            &PersistedWindowPlacement::Docked {
                edge: "bottom".into(),
                size: 20
            },
        );

        session.key("ctrl-j");
        assert_eq!(
            session.application().window_session.focused_terminal(),
            Some(id.0)
        );
        session.key("ctrl-/");
        assert!(session.application().terminal_rect(id).is_none());

        keys(&mut session, &["space", "x", "x"]);
        assert!(session.application().problems_panel_rect().is_none());
        session.key("ctrl-/");
        let app = session.application();
        assert_eq!(app.window_session.focused_terminal(), Some(id.0));
        assert_eq!(app.terminal_rect(id), Some(shrunk));
        assert_eq!(
            app.current_native_placement(target).unwrap(),
            bottom_dock(20)
        );

        session.resize(80, 24);
        assert_eq!(session.application().terminal_rect(id), Some(original));
    }

    #[test]
    fn hidden_session_default_can_be_revealed_with_ctrl_slash() {
        use crate::persistence::PersistedWindowPlacement;
        use crate::process::PtyId;

        let id = PtyId(1);
        let mut first = TestSession::new(crate::config::load_bundled_config());
        first.resize(100, 30);
        document(&mut first, "editor code");
        first.terminal_fixture();
        first.key("ctrl-/");
        assert!(first.application().terminal_rect(id).is_none());
        let saved = first.application().capture_session().unwrap();
        assert_eq!(
            saved_terminal_rule(&saved),
            &PersistedWindowPlacement::Hidden
        );

        let mut restarted = TestSession::new(crate::config::load_bundled_config());
        restarted.resize(100, 12);
        document(&mut restarted, "editor code");
        restarted
            .application_mut()
            .apply_persisted_window_rules(&saved.window_rules)
            .unwrap();
        restarted.terminal_fixture();
        assert!(restarted.application().terminal_rect(id).is_none());

        restarted.key("ctrl-/");
        assert!(restarted.application().terminal_rect(id).unwrap().height <= 9);
        restarted.resize(100, 30);
        let app = restarted.application();
        assert_eq!(app.window_session.focused_terminal(), Some(id.0));
        assert_eq!(app.terminal_rect(id).unwrap().height, 16);
    }

    fn drag_bottom_terminal(session: &mut TestSession, id: crate::process::PtyId, height: u16) {
        use crate::editor::input::KeyCode::{MouseLeftDown, MouseLeftDrag, MouseLeftUp};
        let rect = session.application().terminal_rect(id).unwrap();
        let y = rect.y + rect.height - height;
        let x = rect.x + rect.width - 10;
        session.mouse(MouseLeftDown, x, rect.y);
        session.mouse(MouseLeftDrag, x, y);
        session.mouse(MouseLeftUp, x, y);
    }

    fn clipped_24_row_terminal() -> TestSession {
        use crate::app::windows::NativeSurfaceTarget;
        let mut session = TestSession::new(crate::config::load_bundled_config());
        session.resize(100, 30);
        document(&mut session, "editor code");
        session.terminal_fixture();
        let id = crate::process::PtyId(1);
        drag_bottom_terminal(&mut session, id, 24);
        let app = session.application_mut();
        assert_eq!(app.terminal_rect(id).unwrap().height, 24);
        app.set_terminal_focus(false);
        app.reconcile_editor_windows();

        session.resize(100, 20);
        let without_problems = session.application().terminal_rect(id).unwrap();
        keys(&mut session, &["space", "x", "x"]);
        let app = session.application();
        let panel = app.problems_panel_rect().unwrap();
        let clipped = app.terminal_rect(id).unwrap();
        assert_eq!(clipped.height + panel.height, without_problems.height);
        assert_eq!(
            app.current_native_placement(NativeSurfaceTarget::Terminal(id.0))
                .unwrap(),
            bottom_dock(24)
        );
        session
    }

    #[test]
    fn problems_open_and_close_resize_an_idle_terminal() {
        use crate::app::windows::NativeSurfaceTarget;
        use crate::process::PtyId;

        let mut session = clipped_24_row_terminal();
        let id = PtyId(1);
        let app = session.application();
        let clipped = app.terminal_rect(id).unwrap();
        assert_eq!(
            app.terminal_session.fixture_dimensions(id),
            (usize::from(clipped.width), usize::from(clipped.height - 1))
        );

        // Don't pump terminal output or resize the host between the key and the assertion.
        session.key("escape");
        let app = session.application();
        assert!(app.problems_panel_rect().is_none());
        let restored = app.terminal_rect(id).unwrap();
        assert!(restored.height > clipped.height);
        assert_eq!(
            app.terminal_session.fixture_dimensions(id),
            (
                usize::from(restored.width),
                usize::from(restored.height - 1)
            )
        );
        assert_eq!(
            app.current_native_placement(NativeSurfaceTarget::Terminal(id.0))
                .unwrap(),
            bottom_dock(24)
        );
    }

    #[test]
    fn ctrl_up_uses_the_graph_size_while_problems_clips_the_terminal() {
        use crate::app::windows::NativeSurfaceTarget;

        let id = crate::process::PtyId(1);
        let target = NativeSurfaceTarget::Terminal(id.0);
        let mut session = clipped_24_row_terminal();
        session.key("ctrl-j");
        session.application_mut().enter_terminal_normal_mode();
        assert_eq!(session.context(), "terminal_normal");
        session.key("ctrl-up");
        let app = session.application_mut();
        assert_eq!(
            app.current_native_placement(target).unwrap(),
            bottom_dock(26)
        );
        assert!(app.terminal_rect(id).unwrap().height < 26);
        app.set_terminal_focus(false);
        app.reconcile_editor_windows();

        keys(&mut session, &["space", "x", "x"]);
        assert!(session.application().problems_panel_rect().is_none());
        // Leave room for the buffer line, status line and minimum editor pane.
        session.resize(100, 32);
        assert_eq!(session.application().terminal_rect(id).unwrap().height, 26);
    }

    #[test]
    fn sideways_border_drag_does_not_save_the_clipped_terminal_height() {
        use crate::app::windows::NativeSurfaceTarget;
        use crate::editor::input::KeyCode::{MouseLeftDown, MouseLeftDrag, MouseLeftUp};
        use crate::persistence::PersistedWindowPlacement;

        let id = crate::process::PtyId(1);
        let target = NativeSurfaceTarget::Terminal(id.0);
        let mut session = clipped_24_row_terminal();
        let rect = session.application().terminal_rect(id).unwrap();
        session.mouse(MouseLeftDown, 70, rect.y);
        assert!(session.application().dock_drag.is_some());
        session.mouse(MouseLeftDrag, 80, rect.y);
        session.mouse(MouseLeftUp, 80, rect.y);
        let app = session.application();
        assert_eq!(app.terminal_rect(id), Some(rect));
        assert_eq!(
            app.current_native_placement(target).unwrap(),
            bottom_dock(24)
        );
        assert_eq!(app.window_session.terminal_height(), 24);
        assert_eq!(
            saved_terminal_rule(&app.capture_session().unwrap()),
            &PersistedWindowPlacement::Docked {
                edge: "bottom".into(),
                size: 24
            },
        );
        let app = session.application_mut();
        app.set_terminal_focus(false);
        app.reconcile_editor_windows();

        keys(&mut session, &["space", "x", "x"]);
        assert!(session.application().problems_panel_rect().is_none());
        session.resize(100, 30);
        assert_eq!(session.application().terminal_rect(id).unwrap().height, 24);
    }

    #[test]
    fn saving_an_automatic_terminal_uses_the_same_size_with_problems_open() {
        use crate::process::PtyId;

        let mut session = TestSession::new(crate::config::load_bundled_config());
        session.resize(100, 18);
        document(&mut session, "editor code");
        session.terminal_fixture();
        let app = session.application_mut();
        app.set_terminal_focus(false);
        app.reconcile_editor_windows();
        let id = PtyId(1);
        let normal = app.terminal_rect(id).unwrap();
        let saved = app.capture_session().unwrap();

        keys(&mut session, &["space", "x", "x"]);
        let app = session.application();
        assert!(app.problems_panel_rect().is_some());
        assert!(app.terminal_rect(id).unwrap().height < normal.height);
        assert_eq!(
            saved_terminal_rule(&app.capture_session().unwrap()),
            saved_terminal_rule(&saved),
        );
    }

    #[test]
    fn automatic_terminal_grows_when_the_host_grows() {
        let mut session = TestSession::new(crate::config::load_bundled_config());
        session.resize(100, 12);
        document(&mut session, "editor code");
        session.terminal_fixture();

        let id = crate::process::PtyId(1);
        let app = session.application();
        assert!(app.problems_panel_rect().is_none());
        assert!(app.terminal_rect(id).unwrap().height <= 9);

        session.resize(100, 30);
        let app = session.application();
        assert_eq!(app.window_session.focused_terminal(), Some(id.0));
        assert_eq!(app.terminal_rect(id).unwrap().height, 16);
    }

    #[cfg(unix)]
    fn shell_session_at_100_by_60() -> TestSession {
        let mut loaded = crate::config::load_bundled_config();
        let profile = loaded
            .config
            .terminals
            .profiles
            .get_mut(&loaded.config.terminals.default_profile)
            .unwrap();
        profile.program = "/bin/cat".into();
        profile.arguments.clear();
        profile.float = false;
        let mut session = TestSession::new(loaded);
        session.resize(100, 60);
        document(&mut session, "editor code");
        session.trust_owned_terminal_fixture();
        session
    }

    #[cfg(unix)]
    fn wait_for_terminal(
        session: &mut TestSession,
        previous: Option<crate::process::PtyId>,
    ) -> (crate::process::PtyId, WindowRect) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let app = session.application();
            if let Some(id) = app.terminal_session.selected()
                && Some(id) != previous
                && let Some(rect) = app.terminal_rect(id)
            {
                return (id, rect);
            }
            assert!(Instant::now() < deadline, "{:?}", session.notification());
            session.pump_terminal();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[cfg(unix)]
    fn two_terminals_with_a_45_row_shared_height()
    -> (TestSession, crate::process::PtyId, crate::process::PtyId) {
        let mut session = shell_session_at_100_by_60();
        session.key("ctrl-/");
        let (first, _) = wait_for_terminal(&mut session, None);
        drag_bottom_terminal(&mut session, first, 45);
        let app = session.application();
        assert_eq!(app.terminal_rect(first).unwrap().height, 45);
        assert_eq!(app.window_session.terminal_height(), 45);
        assert!(app.problems_panel_rect().is_none());

        session.key("ctrl-t");
        let (second, rect) = wait_for_terminal(&mut session, Some(first));
        assert_eq!(rect.height, 40);
        assert_eq!(session.application().window_session.terminal_height(), 45);
        (session, first, second)
    }

    #[cfg(unix)]
    #[test]
    fn ctrl_down_shrinks_a_40_row_automatic_terminal_to_38() {
        let (mut session, _, second) = two_terminals_with_a_45_row_shared_height();
        session.application_mut().enter_terminal_normal_mode();
        assert_eq!(session.context(), "terminal_normal");
        session.key("ctrl-down");
        assert_eq!(
            session.application().terminal_rect(second).unwrap().height,
            38
        );
    }

    #[cfg(unix)]
    #[test]
    fn saving_or_hiding_the_40_row_terminal_keeps_its_head_size() {
        use crate::persistence::PersistedWindowPlacement;
        let (mut session, first, second) = two_terminals_with_a_45_row_shared_height();
        let original = session.application().terminal_rect(second).unwrap();
        let saved = session.application().capture_session().unwrap();
        assert_eq!(
            saved_terminal_rule(&saved),
            &PersistedWindowPlacement::Docked {
                edge: "bottom".into(),
                size: 40
            },
        );

        let mut restarted = shell_session_at_100_by_60();
        restarted
            .application_mut()
            .apply_persisted_window_rules(&saved.window_rules)
            .unwrap();
        restarted.key("ctrl-/");
        let (_, rect) = wait_for_terminal(&mut restarted, None);
        assert_eq!(rect.height, 40);

        session.key("ctrl-/");
        assert!(session.application().terminal_rect(second).is_none());
        session.key("ctrl-/");
        let app = session.application();
        assert_eq!(app.window_session.focused_terminal(), Some(second.0));
        assert_eq!(app.terminal_rect(second), Some(original));
        assert_eq!(app.window_session.terminal_height(), 45);

        session
            .application_mut()
            .process_effect(AppEffect::TerminalFocus(first.0));
        let app = session.application();
        assert_eq!(app.window_session.focused_terminal(), Some(first.0));
        assert_eq!(app.terminal_rect(first).unwrap().height, 45);
        assert_eq!(app.window_session.terminal_height(), 45);
    }

    #[test]
    fn short_layout_keeps_usable_docks_and_readable_problems_with_files_and_grep() {
        use crate::process::PtyId;
        for problems_first in [false, true] {
            let mut session = TestSession::new(crate::config::load_bundled_config());
            session.resize(80, 24);
            let (buffer, _, producer) = document(&mut session, "editor code");
            let message = "readable diagnostic";
            publish(
                session.application_mut(),
                &producer,
                buffer,
                &[(0, DiagnosticSeverity::Error, message)],
            );
            if problems_first {
                keys(&mut session, &["space", "x", "x"]);
            }
            session.terminal_fixture();
            let app = session.application_mut();
            app.place_active_terminal(WindowPlacement::Docked {
                edge: DockEdge::Bottom,
                size: 20,
            });
            app.set_terminal_focus(false);
            app.reconcile_editor_windows();
            if !problems_first {
                keys(&mut session, &["space", "x", "x"]);
                assert_eq!(
                    session.application().window_session.focus(),
                    FocusTarget::ProblemsPanel
                );
            }
            let app = session.application();
            assert!(app.problems_panel_rect().unwrap().height >= 2);
            assert!(app.terminal_rect(PtyId(1)).unwrap().height >= 3);

            keys(&mut session, &["space", "f", "f"]);
            assert_readable_with_picker(&mut session, crate::ui::PickerSurfaceKind::Files, message);
            session.application_mut().close_picker();
            session.open_grep();
            assert_readable_with_picker(&mut session, crate::ui::PickerSurfaceKind::Grep, message);
            session.application_mut().close_picker();

            session.resize(80, 13);
            let app = session.application();
            let panel = app.problems_panel_rect().unwrap();
            let terminal = app.terminal_rect(PtyId(1)).unwrap();
            assert!(panel.height >= 2 && terminal.height >= 3);
            assert_eq!(panel.y + panel.height, terminal.y);
            assert_eq!(
                app.terminal_session.fixture_dimensions(PtyId(1)).1,
                usize::from(terminal.height - 1)
            );
            session.mouse(
                crate::editor::input::KeyCode::MouseLeftDown,
                panel.x + 1,
                panel.y,
            );
            assert_eq!(
                session.application().window_session.focus(),
                FocusTarget::ProblemsPanel
            );
            session.resize(80, 4);
            assert!(session.application().problems_panel_rect().is_none());
            assert_ne!(
                session.application().window_session.focus(),
                FocusTarget::ProblemsPanel
            );
            assert_eq!(session.notification(), Some(NO_ROOM));
            session.application_mut().close_problems_panel();
            session.application_mut().open_problems_panel(None);
            assert_ne!(
                session.application().window_session.focus(),
                FocusTarget::ProblemsPanel
            );
            assert_eq!(
                session.notification(),
                Some("Not enough room for the Problems panel")
            );
        }
    }

    #[test]
    fn duplicate_diagnostics_keep_their_occurrence_after_an_edit() {
        let mut session = TestSession::new(crate::config::load_bundled_config());
        let (buffer, view, producer) = document(&mut session, "top\nbad\nbad\n");
        publish(
            session.application_mut(),
            &producer,
            buffer,
            &[
                (4, DiagnosticSeverity::Error, "same problem"),
                (8, DiagnosticSeverity::Error, "same problem"),
            ],
        );
        keys(&mut session, &["space", "x", "x", "j", "ctrl-k", "i"]);
        session.paste("new\n");
        session.key("escape");
        session.application_mut().sync_ui_diagnostics();
        let app = session.application();
        let panel = app.ui.problems_panel.as_ref().unwrap();
        assert!(
            matches!(&panel.rows[panel.selected_row.unwrap()], ProblemsRow::Problem { location, .. } if location == "4:1")
        );
        session.key("ctrl-j");
        session.key("enter");
        assert_eq!(
            session.application().state.views[view].cursor,
            ByteOffset(12)
        );
    }

    #[test]
    fn movement_and_scroll_share_large_problem_row_storage() {
        let mut session = TestSession::new(crate::config::load_bundled_config());
        let text = "problem\n".repeat(1_000);
        let (buffer, _, producer) = document(&mut session, &text);
        let items = (0..1_000)
            .map(|index| (index * 8, DiagnosticSeverity::Error, "problem"))
            .collect::<Vec<_>>();
        publish(session.application_mut(), &producer, buffer, &items);
        keys(&mut session, &["space", "x", "x"]);
        let app = session.application();
        let panel = app.problems_panel_rect().unwrap();
        let rows = Arc::clone(&app.ui.problems_panel.as_ref().unwrap().rows);
        assert_eq!(rows.len(), 1_001);
        for _ in 0..24 {
            session.key("j");
            session.mouse(
                crate::editor::input::KeyCode::MouseScrollDown,
                panel.x + 1,
                panel.y + 1,
            );
            session.frame();
            let visible = session.application().ui.problems_panel.as_ref().unwrap();
            assert!(Arc::ptr_eq(&rows, &visible.rows));
        }
        assert_eq!(
            session
                .application()
                .ui
                .problems_panel
                .as_ref()
                .unwrap()
                .selected_row,
            Some(25)
        );
    }
}
