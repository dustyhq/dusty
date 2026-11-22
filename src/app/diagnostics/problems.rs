//! Entries and navigation shared by the two Problems popups.

use crate::app::App;
use crate::app::diagnostics::DiagnosticKey;
use crate::app::tasks::TestKey;
use crate::editor::text::{PositionCodec, PositionEncoding};
use crate::language::lsp::path_from_file_uri;
use crate::workspace::ProblemRow;
use std::path::{Path, PathBuf};

pub(in crate::app) enum ProblemEntry<'a> {
    Diagnostic(DiagnosticKey, &'a ProblemRow),
    Test(PathBuf, usize, String, TestKey),
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::app) enum ProblemFile {
    Path(PathBuf),
    Uri(String),
}

impl ProblemFile {
    pub(in crate::app) fn from_path(root: &Path, path: &Path) -> Self {
        Self::Path(problem_path(root, path))
    }

    pub(in crate::app) fn from_uri(root: &Path, uri: &str) -> Self {
        path_from_file_uri(uri).map_or_else(
            || Self::Uri(uri.into()),
            |path| Self::from_path(root, &path),
        )
    }

    pub(in crate::app) fn label(&self, app: &App) -> String {
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

    pub(in crate::app) fn test_problem_column(
        &self,
        file: &ProblemFile,
        source: &crate::app::picker::MatcherSource,
    ) -> Option<usize> {
        // Display one-based Unicode scalar columns, never raw UTF-8/UTF-16 units.
        // A matcher for an unopened file has no text snapshot; show just its line if
        // converting the column would require a speculative read.
        let root = &self.state.workspace_root;
        let text = self.state.buffers.values().find_map(|buffer| {
            let path = buffer.path.as_deref()?;
            (ProblemFile::from_path(root, path) == *file).then_some(&buffer.text)
        });
        if let Some(text) = text {
            let codec = PositionCodec::new(text);
            codec
                .lsp_to_byte(source.line, source.column, source.encoding)
                .ok()
                .and_then(|byte| codec.byte_to_lsp(byte, PositionEncoding::Utf32).ok())
                .map(|(_, column)| column)
        } else {
            (source.encoding == PositionEncoding::Utf32).then_some(source.column)
        }
    }
}
