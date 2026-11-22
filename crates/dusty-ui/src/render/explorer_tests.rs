//! Explorer tests for editor surfaces and frame composition.

use crate::config::{AmbiguousWidth, Config};
use crate::editor::EditorState;
use crate::grid::{Frame, Style};
use crate::render::UiState;
use crate::render::explorer::render_explorer_window;
use crate::render::{ExplorerSegmentRole, ExplorerUiRow, ExplorerUiSegment, ExplorerUiState};
use crate::windows::WindowRect;
use std::path::PathBuf;

use crate::render::composition_tests::editor_layout;
use crate::render::composition_tests::render;

#[test]
fn buffer_line_begins_after_the_explorer_dock() {
    let mut state = EditorState::new(PathBuf::from("/workspace"));
    state
        .open_text(
            crate::editor::text::TextBuffer::from_text("one"),
            Some(PathBuf::from("/workspace/one.rs")),
        )
        .unwrap();
    state
        .open_text(
            crate::editor::text::TextBuffer::from_text("two"),
            Some(PathBuf::from("/workspace/two.rs")),
        )
        .unwrap();
    let ui = UiState {
        editor_window_layout: Some(editor_layout(&state)),
        content_tabs: vec![
            crate::render::ContentTabEntry {
                identity: crate::render::ContentTabIdentity(crate::windows::ContentId(1)),
                kind: crate::render::ContentTabKind::Text,
                title: "one.rs".into(),
                active: false,
                dirty: false,
                pinned: false,
                diagnostic: None,
                icon: "R".into(),
                closeable: true,
                segments: None,
            },
            crate::render::ContentTabEntry {
                identity: crate::render::ContentTabIdentity(crate::windows::ContentId(2)),
                kind: crate::render::ContentTabKind::Text,
                title: "two.rs".into(),
                active: true,
                dirty: false,
                pinned: false,
                diagnostic: None,
                icon: "R".into(),
                closeable: true,
                segments: None,
            },
        ],
        explorer: Some(crate::render::ExplorerUiState {
            title: "Explorer".into(),
            rows: Vec::new(),
            scroll: 0,
            loading: false,
            total: 0,
            matches: 0,
            query: String::new(),
            searching: false,
        }),
        explorer_focused: true,
        ..UiState::default()
    };
    let loaded = crate::config::load_bundled_config();
    let frame = render(&state, &ui, &loaded.config, &loaded.dashboard, 80, 10);
    let panel = crate::render::explorer_panel_width(&loaded.config, 80).expect("explorer width");
    assert!(frame.grid.row_text(0).contains("Explorer"));
    let editor_line = (panel.saturating_add(1)..80)
        .map(|x| frame.grid.get(x, 0).map_or("", |cell| cell.symbol.as_str()))
        .collect::<String>();
    assert!(editor_line.contains("one.rs"));
    assert!(editor_line.contains("two.rs"));
}

#[test]
fn explorer_is_a_persistent_left_dock_that_reflows_the_editor() {
    let mut state = EditorState::new(PathBuf::from("/workspace"));
    state
        .open_text(
            crate::editor::text::TextBuffer::from_text("editor content"),
            Some(PathBuf::from("/workspace/main.rs")),
        )
        .unwrap();
    let ui = UiState {
        editor_window_layout: Some(editor_layout(&state)),
        explorer: Some(crate::render::ExplorerUiState {
            title: "Explorer".into(),
            rows: vec![
                crate::render::ExplorerUiRow {
                    node_id: 1,
                    git_line_count_segments: Vec::new(),
                    selected: false,
                    guides: vec!["├─".into()],
                    relative_path: None,
                    segments: vec![
                        crate::render::ExplorerUiSegment {
                            text: "▾".into(),
                            role: crate::render::ExplorerSegmentRole::Icon,
                        },
                        crate::render::ExplorerUiSegment {
                            text: " ".into(),
                            role: crate::render::ExplorerSegmentRole::Tree,
                        },
                        crate::render::ExplorerUiSegment {
                            text: "src".into(),
                            role: crate::render::ExplorerSegmentRole::Directory,
                        },
                    ],
                    right_segments: vec![
                        crate::render::ExplorerUiSegment {
                            text: "M".into(),
                            role: crate::render::ExplorerSegmentRole::Git,
                        },
                        crate::render::ExplorerUiSegment {
                            text: "E".into(),
                            role: crate::render::ExplorerSegmentRole::Diagnostic,
                        },
                    ],
                },
                crate::render::ExplorerUiRow {
                    node_id: 2,
                    git_line_count_segments: Vec::new(),
                    selected: true,
                    guides: vec!["│ ".into(), "└─".into()],
                    relative_path: Some(
                        "src/main.rs-with-an-intentionally-overlong-name-that-must-not-spill"
                            .into(),
                    ),
                    segments: vec![
                        crate::render::ExplorerUiSegment {
                            text: "·".into(),
                            role: crate::render::ExplorerSegmentRole::Icon,
                        },
                        crate::render::ExplorerUiSegment {
                            text:
                                " main.rs-with-an-intentionally-overlong-name-that-must-not-spill"
                                    .into(),
                            role: crate::render::ExplorerSegmentRole::File,
                        },
                    ],
                    right_segments: vec![crate::render::ExplorerUiSegment {
                        text: "?".into(),
                        role: crate::render::ExplorerSegmentRole::Diagnostic,
                    }],
                },
            ],
            scroll: 0,
            loading: false,
            total: 2,
            matches: 2,
            query: String::new(),
            searching: false,
        }),
        explorer_focused: true,
        ..UiState::default()
    };
    let loaded = crate::config::load_bundled_config();
    let panel = crate::render::explorer_panel_width(&loaded.config, 100).expect("dock width");
    assert_eq!(panel, 40);
    assert_eq!(
        crate::render::explorer_panel_width(&loaded.config, 30),
        Some(14)
    );
    assert_eq!(crate::render::explorer_panel_width(&loaded.config, 2), None);
    let frame = render(&state, &ui, &loaded.config, &loaded.dashboard, 100, 20);
    assert!(frame.grid.row_text(0).contains("Explorer"));
    assert!(frame.grid.row_text(3).contains("src"));
    assert!(frame.grid.row_text(4).contains("…"));
    assert!(frame.grid.row_text(4).contains("spill"));
    assert_eq!(
        frame.grid.get(1, 3).expect("tree segment").style,
        crate::Style::ExplorerTree
    );
    assert_eq!(
        frame.grid.get(3, 3).expect("icon segment").style,
        crate::Style::ExplorerIcon
    );
    assert_eq!(
        frame.grid.get(5, 3).expect("directory name").style,
        crate::Style::ExplorerDirectory
    );
    assert_eq!(
        frame.grid.get(37, 3).expect("git decoration").style,
        crate::Style::ExplorerGit
    );
    assert_eq!(
        frame.grid.get(38, 3).expect("diagnostic decoration").style,
        crate::Style::ExplorerDiagnostic
    );
    assert_eq!(
        frame.grid.get(1, 4).expect("selected row").style,
        crate::Style::ExplorerSelected(crate::render::ExplorerSegmentRole::Tree)
    );
    assert_eq!(
        frame.grid.get(38, 4).expect("selected diagnostic").style,
        crate::Style::ExplorerSelected(crate::render::ExplorerSegmentRole::Diagnostic)
    );
    assert_eq!(
        frame.grid.get(panel, 1).expect("dock separator").symbol,
        "│"
    );
    assert_eq!(
        frame
            .grid
            .get(panel, 2)
            .expect("preserved separator")
            .symbol,
        "│"
    );
    assert_eq!(
        frame
            .grid
            .get(panel + 1, 2)
            .expect("editor-side cell")
            .style,
        crate::Style::Muted
    );
    assert!(frame.cursor.is_some_and(|(x, y, _)| x == 0 && y == 4));
    assert!(frame.grid.row_text(19).contains("NORMAL"));
    let mut editor_ui = ui.clone();
    editor_ui.explorer_focused = false;
    let editor_frame = render(
        &state,
        &editor_ui,
        &loaded.config,
        &loaded.dashboard,
        100,
        20,
    );
    assert!(editor_frame.cursor.is_some_and(|(x, _, _)| x > panel));

    let mut ascii = loaded.config.clone();
    ascii.ui.explorer.icon_mode = crate::config::ExplorerIconMode::Ascii;
    let ascii_frame = render(&state, &ui, &ascii, &loaded.dashboard, 100, 20);
    assert_eq!(
        ascii_frame
            .grid
            .get(panel, 3)
            .expect("ASCII separator")
            .symbol,
        "|"
    );
    let mut unicode = loaded.config.clone();
    unicode.ui.explorer.icon_mode = crate::config::ExplorerIconMode::Unicode;
    let unicode_frame = render(&state, &ui, &unicode, &loaded.dashboard, 100, 20);
    assert_eq!(
        unicode_frame
            .grid
            .get(panel, 3)
            .expect("Unicode separator")
            .symbol,
        "│"
    );
}

fn tree_row(name: &str, level: usize, status: &str) -> ExplorerUiRow {
    let segment = |text: &str, role| ExplorerUiSegment {
        text: text.into(),
        role,
    };
    let mut guides = vec!["│ ".into(); level];
    if let Some(last) = guides.last_mut() {
        *last = "└─".into();
    }
    ExplorerUiRow {
        node_id: level as u64,
        selected: false,
        guides,
        segments: vec![
            segment("·", ExplorerSegmentRole::Icon),
            segment(" ", ExplorerSegmentRole::Tree),
            segment(name, ExplorerSegmentRole::File),
        ],
        right_segments: (!status.is_empty())
            .then(|| segment(status, ExplorerSegmentRole::Git))
            .into_iter()
            .collect(),
        git_line_count_segments: Vec::new(),
        relative_path: Some(name.into()),
    }
}

fn render_tree(rows: &[ExplorerUiRow], panel_width: u16, ambiguous: AmbiguousWidth) -> Frame {
    let height = rows.len() as u16 + 4;
    // Include the dock separator and one untouched cell beyond it.
    let width = panel_width + 2;
    let mut frame = Frame {
        grid: crate::CellGrid::new_with_policy(
            width,
            height,
            crate::grid::CONTROL_CHARACTER_REPLACEMENT,
            crate::grid::ZERO_WIDTH_REPLACEMENT,
            ambiguous == AmbiguousWidth::Wide,
        ),
        cursor: None,
    };
    let mut config = Config::default();
    config.editor.ambiguous_width = ambiguous;
    render_explorer_window(
        &mut frame,
        &ExplorerUiState {
            title: "Explorer".into(),
            rows: rows.to_vec(),
            scroll: 0,
            loading: false,
            total: rows.len(),
            matches: rows.len(),
            query: String::new(),
            searching: false,
        },
        &config,
        WindowRect {
            x: 0,
            y: 0,
            width: panel_width + 1,
            height,
        },
        true,
    );
    frame
}

#[test]
fn explorer_shifts_whole_indent_levels_to_show_the_selected_leaf() {
    let folders = [
        "alpha-directory",
        "bravo-directory",
        "charlie-directory",
        "delta-directory",
        "echo-directory",
        "foxtrot-directory",
        "golf-directory",
        "hotel-directory",
        "india-directory",
        "juliet-directory",
        "kilo-directory",
        "lima-directory",
        "mike-directory",
        "november-directory",
        "oscar-directory",
    ];
    let mut rows = vec![tree_row("proj", 0, "")];
    rows.extend(
        folders
            .iter()
            .enumerate()
            .map(|(index, name)| tree_row(name, index + 1, "")),
    );
    let leaf = rows.len();
    let mut file = tree_row("leaf.rs", leaf, "?");
    file.selected = true;
    file.relative_path = Some(format!("{}/leaf.rs", folders.join("/")));
    rows.push(file);
    let frame = render_tree(&rows, 40, AmbiguousWidth::Narrow);
    let y = leaf as u16 + 3;
    let line = frame.grid.row_text(y);
    assert_eq!(line, format!(" …{}└─· leaf.rs ? │ ", "│ ".repeat(12)));
    assert_eq!(frame.grid.get(2, y - 1).unwrap().symbol, "│");
    assert_eq!(frame.grid.get(2, y).unwrap().symbol, "│");
    assert_eq!(
        frame.grid.get(1, y).unwrap().style,
        Style::ExplorerSelected(ExplorerSegmentRole::Tree)
    );
    assert!(frame.grid.row_text(4).starts_with(" …"));
    let footer = frame.grid.row_text(y + 1);
    assert!(footer.starts_with("╰ …") && footer.contains("/oscar-directory/leaf.rs"));
    assert_eq!(frame.grid.get(2, y + 1).unwrap().style, Style::ExplorerTree);

    rows[leaf].selected = false;
    rows[1].selected = true;
    let top = render_tree(&rows, 40, AmbiguousWidth::Narrow);
    assert!(top.grid.row_text(4).starts_with(" └─· alpha-directory"));
    assert_eq!(
        frame.grid.row_text(3),
        top.grid.row_text(3),
        "root is unshifted"
    );
    assert_eq!(top.grid.row_text(y + 1), format!("╰{}╯ ", "─".repeat(39)));
}

#[test]
fn explorer_keeps_both_ends_of_long_names_and_leaves_a_status_gap() {
    let name = "this-is-a-really-really-really-long-file-name-that-keeps-going-and-going-until-it-overflows-the-sidebar-v2.rs";
    let mut row = tree_row(name, 1, "?");
    row.selected = true;
    let frame = render_tree(&[row], 40, AmbiguousWidth::Narrow);
    assert_eq!(
        frame.grid.row_text(3),
        " └─· this-is-a-really-re…idebar-v2.rs ? │ "
    );
    let footer = frame.grid.row_text(4);
    assert!(
        footer.contains(" …until-it-overflows-the-sidebar-v2.rs "),
        "{footer}"
    );

    let mut row = tree_row(&format!("界👩‍💻e\u{301}{}.script", "界".repeat(20)), 0, "?");
    row.selected = true;
    let frame = render_tree(&[row], 24, AmbiguousWidth::Wide);
    let line = frame.grid.row_text(3);
    assert!(
        line.contains("界👩‍💻e\u{301}") && line.contains("….script"),
        "{line}"
    );
    assert!(!frame.grid.get(24, 3).unwrap().continuation);
}

#[test]
fn explorer_replaces_control_characters_in_names_and_footer_paths() {
    let mut row = tree_row("long-name-a\nb\u{1b}c\t\u{7}.rs", 0, "");
    row.selected = true;
    row.relative_path = Some("some\nfolder/long-name-a\nb\u{1b}c\t\u{7}.rs".into());
    let frame = render_tree(std::slice::from_ref(&row), 40, AmbiguousWidth::Narrow);
    assert!(frame.grid.row_text(3).contains("a�b�c��.rs"));
    let frame = render_tree(&[row], 12, AmbiguousWidth::Narrow);
    let footer = frame.grid.row_text(4);
    assert!(footer.contains("…b�c��.rs"), "{footer}");
}

#[test]
fn explorer_reserves_space_for_counts_but_falls_back_to_letters_when_narrow() {
    let mut changed = tree_row("a-very-long-filename.rs", 1, "M");
    changed.selected = true;
    changed.git_line_count_segments = vec![
        ExplorerUiSegment {
            text: "+12".into(),
            role: ExplorerSegmentRole::GitLineCount { added: true },
        },
        ExplorerUiSegment {
            text: " −3".into(),
            role: ExplorerSegmentRole::GitLineCount { added: false },
        },
    ];
    let clean = tree_row("b-very-long-filename.rs", 1, "");
    let mut added = tree_row("short.rs", 0, "A");
    added.git_line_count_segments.push(ExplorerUiSegment {
        text: "+4".into(),
        role: ExplorerSegmentRole::GitLineCount { added: true },
    });
    let rows = [changed, clean, added];
    let frame = render_tree(&rows, 25, AmbiguousWidth::Narrow);
    assert!(frame.grid.row_text(3).contains(" +12 −3"));
    assert!(frame.grid.row_text(4).ends_with("        │ "));
    assert_eq!(
        frame.grid.get(18, 3).unwrap().style,
        Style::ExplorerSelected(ExplorerSegmentRole::GitLineCount { added: true })
    );
    assert_eq!(
        frame.grid.get(22, 3).unwrap().style,
        Style::ExplorerSelected(ExplorerSegmentRole::GitLineCount { added: false })
    );

    let frame = render_tree(&rows, 17, AmbiguousWidth::Narrow);
    let line = frame.grid.row_text(3);
    assert!(line.contains(" M │") && !line.contains("+12"), "{line}");
    assert!(frame.grid.row_text(5).contains(" +4 │"));
}

#[test]
fn explorer_reserves_two_git_cells_and_a_gap_but_keeps_only_worktree_at_narrow_width() {
    let name = "a-very-long-filename-with-both-changes.rs";
    let mut mixed = tree_row(name, 1, "M");
    mixed.selected = true;
    mixed.right_segments = vec![
        ExplorerUiSegment {
            text: "A".into(),
            role: ExplorerSegmentRole::GitStaged,
        },
        ExplorerUiSegment {
            text: "M".into(),
            role: ExplorerSegmentRole::GitModified,
        },
    ];
    let folder = tree_row("a-very-long-folder-name-with-many-children", 1, "M");
    let rows = [mixed, folder];
    let frame = render_tree(&rows, 30, AmbiguousWidth::Narrow);
    assert!(frame.grid.row_text(3).ends_with(".rs AM │ "));
    assert!(frame.grid.row_text(4).ends_with("  M │ "));
    assert_eq!(
        frame.grid.get(27, 3).unwrap().style,
        Style::ExplorerSelected(ExplorerSegmentRole::GitStaged)
    );
    assert_eq!(
        frame.grid.get(28, 3).unwrap().style,
        Style::ExplorerSelected(ExplorerSegmentRole::GitModified)
    );

    let frame = render_tree(&rows, 12, AmbiguousWidth::Narrow);
    let line = frame.grid.row_text(3);
    assert!(line.ends_with(" M│ ") && !line.contains("AM"), "{line}");
    assert_eq!(
        frame.grid.get(11, 3).unwrap().style,
        Style::ExplorerSelected(ExplorerSegmentRole::GitModified)
    );
}

#[test]
fn twelve_column_explorer_keeps_icon_and_three_name_characters_inside_the_border() {
    let mut row = tree_row("abcdefghijk.rs", 15, "?");
    row.selected = true;
    let frame = render_tree(&[row], 11, AmbiguousWidth::Wide);
    let line = frame.grid.row_text(3);
    assert!(line.starts_with("…·ab…s ?"), "{line}");
    assert_eq!(frame.grid.get(11, 3).unwrap().symbol, "|");
    assert_eq!(frame.grid.get(12, 3).unwrap().style, Style::Default);
    assert_eq!(frame.grid.get(11, 4).unwrap().symbol, "+");
}
