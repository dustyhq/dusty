//! Picker behavior and application integration tests.

use crate::app::App;
use crate::app::AppEffect;
use crate::app::PickerKind;
use crate::app::picker::MatcherSource;
use crate::app::picker::PickerOrigin;
use crate::app::picker::PickerPayload;
use crate::app::tests::*;
use crate::editor::InvocationSource;
use crate::editor::text::TextBuffer;
use crate::ui::PickerSurfaceKind;
use crate::windows::WindowRect;
use std::fs;
use std::thread;
use std::time::Duration;
use std::time::Instant;

#[test]
fn recovery_picker_discards_only_after_explicit_confirmation() {
    use crate::ui::ConfirmationChoice;
    use crate::ui::PromptKind;

    let (_dirs, mut app) = app();
    app.recovery.skip_maintenance();
    let directory = app.recovery.directory();
    fs::create_dir_all(directory).unwrap();
    let journal = directory.join("buffer-draft.toml");
    let file = app.state.workspace_root.join("draft.txt");
    let record = crate::files::RecoveryRecord::new(Some(&file), 1, "one\ntwo\n".into());
    crate::files::write_recovery(&journal, &record).unwrap();
    app.recovery
        .fixture_records(vec![(journal.clone(), record)]);

    app.open_recovery_picker();
    let selected_row = |app: &App| {
        app.search
            .picker()
            .unwrap()
            .selected_item()
            .unwrap()
            .id
            .clone()
    };
    let row_id = selected_row(&app);
    app.process_effect(AppEffect::PickerRemove);
    wait_vfs(&mut app);
    let prompt = app.ui.prompt.as_ref().expect("discard must ask first");
    assert_eq!(prompt.kind, PromptKind::RecoveryDiscard);
    let confirmation = prompt.confirmation.as_ref().unwrap();
    assert_eq!(confirmation.title, "Discard unsaved work");
    assert_eq!(
        confirmation.message,
        "Discard the unsaved changes to draft.txt (2 lines)? This can't be undone."
    );
    assert_eq!(confirmation.selected, ConfirmationChoice::No);
    assert!(journal.exists());

    // Enter takes the default No; Escape also returns to the same picker row.
    for key in ["enter", "escape"] {
        app.handle_configured_key(key.parse().unwrap());
        wait_vfs(&mut app);
        assert!(app.ui.prompt.is_none());
        assert!(journal.exists());
        assert_eq!(selected_row(&app), row_id);
        app.process_effect(AppEffect::PickerRemove);
    }

    app.handle_configured_key("y".parse().unwrap());
    wait_vfs(&mut app);
    assert!(app.ui.prompt.is_none());
    assert!(app.ui.picker.is_none());
    assert!(!journal.exists());
}

#[test]
fn replacement_select_uses_native_row_actions_and_rejects_old_answers() {
    let (_dirs, mut app) = app();
    let (sender, requests) = std::sync::mpsc::sync_channel(8);
    app.replace_select(Some(sender), crate::editor::CommandAuthority::Host);
    let rows = || {
        vec![(
            crate::workspace::PickerItem {
                id: crate::workspace::PickerItemId("notice".into()),
                label: "Read notice".into(),
                detail: String::new(),
                source: String::new(),
                badge: String::new(),
                action: crate::workspace::PickerAction::None,
                kind: crate::workspace::PickerItemKind::Entry,
            },
            PickerPayload::Notice {
                title: "Notice".into(),
                text: "selected".into(),
            },
        )]
    };
    app.install_picker("Select".into(), rows());
    let old = requests.try_recv().unwrap();
    assert_eq!(old.picker.title, "Select");
    assert!(app.ui.picker.is_none());
    app.install_picker("New select".into(), rows());
    assert!(
        old.reply
            .send(Some(crate::workspace::PickerItemId("notice".into())))
            .is_err()
    );
    let current = requests.try_recv().unwrap();
    current
        .reply
        .send(Some(crate::workspace::PickerItemId("notice".into())))
        .unwrap();
    app.process_select_replacement();
    assert!(app.search.picker_projection().is_none());
    assert!(app.ui.documentation_popup.is_some());
    app.install_picker("Cancel".into(), rows());
    let cancelled = requests.try_recv().unwrap();
    app.handle_configured_key("escape".parse().unwrap());
    assert!(cancelled.reply.send(None).is_err());
}

#[test]
fn unknown_replacement_selection_does_not_run_another_rows_action() {
    let (_dirs, mut app) = app();
    let (sender, requests) = std::sync::mpsc::sync_channel(1);
    app.replace_select(Some(sender), crate::editor::CommandAuthority::Host);
    app.install_picker("Empty".into(), Vec::new());
    requests
        .try_recv()
        .unwrap()
        .reply
        .send(Some(crate::workspace::PickerItemId("missing".into())))
        .unwrap();
    app.process_select_replacement();
    assert!(app.ui.picker.is_some());
    assert_eq!(
        app.ui.notification.as_deref(),
        Some("select replacement returned an unknown item")
    );
}

#[test]
fn file_preview_retries_after_capacity_returns_without_picker_input() {
    let (_app_dirs, mut app) = app();
    let path = app.state.workspace_root.join("preview.txt");
    fs::write(&path, "ready\n").unwrap();
    let roots = vec![app.state.workspace_root.clone()];
    let mut requests = Vec::new();
    loop {
        match app
            .documents
            .read_scoped_bounded(path.clone(), roots.clone(), 64)
        {
            Ok(id) => requests.push(id),
            Err(crate::files::VfsError::QueueFull) => break,
            Err(error) => panic!("unexpected submission error: {error}"),
        }
    }
    app.install_picker(
        "Preview".into(),
        vec![(
            crate::workspace::PickerItem {
                id: crate::workspace::PickerItemId("preview".into()),
                label: "preview.txt".into(),
                detail: String::new(),
                source: String::new(),
                badge: String::new(),
                action: crate::workspace::PickerAction::None,
                kind: crate::workspace::PickerItemKind::Entry,
            },
            PickerPayload::MatcherSource(MatcherSource {
                path,
                line: 0,
                column: 0,
                encoding: crate::editor::text::PositionEncoding::Utf8,
            }),
        )],
    );
    assert!(app.search.preview_needs_retry());
    for id in requests {
        app.documents.cancel_request(id);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while app.search.preview_needs_retry() || app.documents.has_work() {
        app.process_vfs_results();
        assert!(Instant::now() < deadline, "preview retry timed out");
        thread::yield_now();
    }
    let text = app
        .ui
        .picker
        .as_ref()
        .unwrap()
        .preview
        .as_ref()
        .unwrap()
        .as_text()
        .unwrap();
    assert_eq!(text.lines(), ["ready"]);
    assert!(!app.search.preview_needs_retry());
}

#[test]
fn closing_help_returns_the_palette_with_its_query_and_selection_intact() {
    let (_app_dirs, mut app) = app();
    app.open_builtin_picker(crate::app::commands::PickerKind::Commands);
    app.picker_insert("overlay.help.");
    let palette = app.search.picker_projection().expect("palette");
    let query_before = palette.query.clone();
    let selected_before = palette.selected.expect("a row is selected");
    let label_before = palette.rows[selected_before].label.clone();
    let rows_before = palette.rows.len();
    assert!(!query_before.is_empty(), "the palette was filtered");

    app.invoke(
        crate::editor::CommandInvocation::new("overlay.help.reference", InvocationSource::Test)
            .expect("command id"),
    );
    let reference = app.search.picker_projection().expect("reference page");
    assert_eq!(
        reference.query, "",
        "the help page is its own surface, not a filtered palette"
    );

    app.search.close_picker();

    // MEASURED: closing help restores the surface help was opened over, with its query text,
    // its row count and its selected row all unchanged.
    let restored = app
        .search
        .picker_projection()
        .expect("closing help returned to the palette");
    assert_eq!(
        restored.query, query_before,
        "the palette's query text survived help"
    );
    assert_eq!(
        restored.rows.len(),
        rows_before,
        "the palette's rows survived help"
    );
    assert_eq!(
        restored.selected,
        Some(selected_before),
        "the palette's selected row survived help"
    );
    assert_eq!(
        restored.rows[selected_before].label, label_before,
        "the row selected before help is the row selected after it"
    );

    // A help page opened with no picker underneath restores nothing: the editor-context case.
    app.search.close_picker();
    assert!(app.search.picker_projection().is_none());
    app.invoke(
        crate::editor::CommandInvocation::new("overlay.help.contextual", InvocationSource::Test)
            .expect("command id"),
    );
    assert!(
        app.search.picker_projection().is_some(),
        "help opened alone"
    );
    app.search.close_picker();
    assert!(
        app.search.picker_projection().is_none(),
        "closing help that had nothing underneath leaves no surface rather than inventing one"
    );
}

pub(in crate::app) fn palette_row_for(app: &mut App, query: &str) -> String {
    app.open_builtin_picker(crate::app::commands::PickerKind::Commands);
    app.picker_insert(query);
    let palette = app.search.picker_projection().expect("palette");
    let selected = palette
        .selected
        .unwrap_or_else(|| panic!("no row for `{query}`"));
    palette.rows[selected].label.clone()
}

#[test]
fn palette_finds_commands_by_the_words_people_type() {
    let (_app_dirs, mut app) = app();
    for (query, expected) in [
        ("theme", "Select Color Theme"),
        ("color", "Select Color Theme"),
        ("colorscheme", "Select Color Theme"),
        ("mason", "Tool Manager"),
        ("quit dusty", "Quit Dusty"),
        ("close buffer", "Close buffer"),
        ("rename symbol", "Rename symbol"),
        ("open settings", "Open Settings"),
        ("undo", "Undo"),
    ] {
        let row = palette_row_for(&mut app, query);
        assert!(row.contains(expected), "`{query}` picked `{row}`");
    }
}

#[test]
fn palette_footer_describes_the_single_escape_that_restores_editor_focus() {
    let (_dirs, mut app) = app();
    app.state
        .open_text(TextBuffer::from_text("editor"), None)
        .unwrap();
    app.set_dashboard_visibility(false);
    app.reconcile_editor_windows();
    let view = app.state.active_view();
    app.open_command_palette();
    assert_eq!(
        app.ui.picker.as_ref().unwrap().footer.as_deref(),
        Some("Enter Run · Esc Close")
    );
    app.handle_configured_key("escape".parse().unwrap());
    assert!(app.ui.picker.is_none());
    assert_eq!(app.active_input_context(), "editor.normal");
    assert_eq!(app.state.active_view(), view);
}

#[test]
fn palette_runs_flagged_commands_with_the_flag_off() {
    let (_app_dirs, mut app) = app();
    let (buffer, _) = app
        .state
        .open_text(TextBuffer::from_text("unsaved\n"), None)
        .unwrap();
    app.state.buffers[buffer].dirty = true;
    palette_row_for(&mut app, "quit dusty");
    app.accept_picker();
    assert!(!app.should_quit, "ordinary quit refuses unsaved changes");
    assert!(app.ui.notification_error);
}

#[test]
fn palette_rows_that_need_text_open_a_command_line_that_runs_them() {
    let (_app_dirs, mut app) = app();
    palette_row_for(&mut app, "file.open");
    app.accept_picker();
    assert_eq!(app.input.prompt().expect("prompt").input, "e ");
    let path = app.state.workspace_root.join("palette-open.txt");
    fs::write(&path, "opened from the palette\n").unwrap();
    app.prompt_insert("palette-open.txt");
    app.submit_prompt();
    wait_vfs(&mut app);
    let buffer = app.state.active_buffer_id().expect("opened buffer");
    assert_eq!(app.state.buffers[buffer].path.as_ref(), Some(&path));
    assert_eq!(
        app.state.buffers[buffer].text.to_string(),
        "opened from the palette\n"
    );

    let settings = app.configuration.settings();
    for (id, initial) in &settings.command_palette.prompts {
        let invocation = crate::editor::input::parse_ex(
            &format!("{initial}value"),
            &settings.input.command_aliases,
        )
        .unwrap_or_else(|error| panic!("`{id}` prompt `{initial}`: {error}"));
        assert_eq!(invocation.id.as_str(), id, "prompt `{initial}`");
        app.commands
            .validate_invocation_shape(&invocation)
            .unwrap_or_else(|error| panic!("`{id}` prompt `{initial}`: {error}"));
    }
}

#[test]
fn file_picker_can_filter_an_entry_after_the_display_limit() {
    let (_app_dirs, mut app) = app();
    app.start_workspace_file_picker();
    let items = (0..10_001)
        .map(|i| {
            (
                crate::workspace::PickerItem {
                    id: crate::workspace::PickerItemId(format!("file-{i}")),
                    label: format!("file-{i}"),
                    detail: String::new(),
                    badge: String::new(),
                    kind: crate::workspace::PickerItemKind::Entry,
                    source: String::new(),
                    action: crate::workspace::PickerAction::None,
                },
                PickerPayload::Command(String::new()),
            )
        })
        .collect();
    app.install_surface_picker_state(
        PickerSurfaceKind::Generic,
        "Files".into(),
        items,
        String::new(),
        true,
        PickerOrigin::Files,
    );
    app.picker_insert("file-10000");
    assert_eq!(app.ui.picker.as_ref().unwrap().rows[0].label, "file-10000");
}

#[test]
fn dashboard_and_picker_are_stable_graph_overlays_with_projected_geometry() {
    let (_app_dirs, mut app) = app();
    app.window_session.observe_size((100, 30));
    app.reconcile_editor_windows();
    let dashboard = app
        .window_session
        .native_ids()
        .dashboard()
        .expect("dashboard resources");
    let dashboard_rect = app
        .compositor_projection()
        .and_then(|projection| projection.window(dashboard.window))
        .expect("dashboard projection")
        .rect;
    assert_eq!(
        dashboard_rect,
        WindowRect {
            x: 0,
            y: 0,
            width: 100,
            height: 30,
        }
    );

    app.state
        .open_text(crate::editor::text::TextBuffer::from_text("editor"), None)
        .unwrap();
    app.set_dashboard_visibility(false);
    app.ui.picker = Some(crate::ui::PickerUiState {
        preview_side_scroll: (0, 0),
        footer: None,
        input_hint: String::new(),
        surface: crate::ui::PickerSurfaceKind::Generic,
        title: "Search".into(),
        query: "needle".into(),
        rows: Vec::new(),
        selected: None,
        scroll: 0,
        total: 0,
        loading: false,
        preview: None,
    });
    app.reconcile_editor_windows();
    let picker = app
        .window_session
        .native_ids()
        .picker()
        .expect("picker resources");
    let expected = crate::ui::picker_geometry(WindowRect {
        x: 0,
        y: 0,
        width: 100,
        height: 30,
    })
    .expect("picker geometry");
    let projection = app.compositor_projection().expect("picker projection");
    assert_eq!(
        projection
            .window(picker.window)
            .expect("picker window")
            .rect,
        expected
    );
    assert_eq!(
        projection.hit_test(expected.x.saturating_add(1), expected.y.saturating_add(1)),
        Some(picker.window)
    );

    app.ui.picker = None;
    app.reconcile_editor_windows();
    assert_eq!(app.window_session.native_ids().dashboard(), Some(dashboard));
    assert_eq!(
        app.window_session
            .native_ids()
            .picker_for(PickerSurfaceKind::Generic),
        Some(picker)
    );
    assert!(
        app.compositor_projection()
            .expect("hidden picker projection")
            .window(picker.window)
            .is_none()
    );

    app.close_active_buffer(false);
    app.reconcile_editor_windows();
    assert!(app.ui.dashboard);
    assert_eq!(app.window_session.native_ids().dashboard(), Some(dashboard));
    assert!(
        app.compositor_projection()
            .expect("dashboard after last buffer")
            .window(dashboard.window)
            .is_some()
    );
}

#[test]
fn live_logs_and_output_refresh_preserve_picker_normal_without_leaking_it_to_new_sources() {
    let (_app_dirs, mut app) = app();
    for logs in [false, true] {
        app.close_picker();
        if logs {
            app.show_lsp_logs();
        } else {
            app.show_output_picker("one");
        }
        app.handle_configured_key("j".parse().unwrap());
        app.handle_configured_key("escape".parse().unwrap());
        for _ in 0..3 {
            if logs {
                app.show_lsp_logs();
            } else {
                app.show_output_picker("one");
            }
            assert_eq!(app.active_input_context(), "picker_normal");
            assert_eq!(app.ui.picker.as_ref().unwrap().query, "j");
            assert!(
                app.ui
                    .picker
                    .as_ref()
                    .unwrap()
                    .input_hint
                    .starts_with("Normal")
            );
        }
        app.show_output_picker("different-source");
        assert_eq!(app.active_input_context(), "picker");
        assert_eq!(app.ui.picker.as_ref().unwrap().query, "");
    }
}

#[test]
fn picker_popups_stay_off_the_content_line_and_keep_their_graph_identity() {
    let (_app_dirs, mut app) = app();
    app.state
        .open_text(TextBuffer::from_text("editor"), None)
        .unwrap();
    app.set_dashboard_visibility(false);
    for kind in [PickerSurfaceKind::Problems, PickerSurfaceKind::Output] {
        if kind == PickerSurfaceKind::Problems {
            app.open_builtin_picker(PickerKind::Diagnostics);
        } else {
            app.show_output_picker("missing-task");
        }
        app.reconcile_editor_windows();
        let ids = app.window_session.native_ids().picker_for(kind).unwrap();
        assert!(
            !app.ui
                .content_tabs
                .iter()
                .any(|entry| entry.identity.0 == ids.content),
            "a popup is not a tab"
        );
        app.close_picker();
        app.reconcile_editor_windows();
        assert!(app.window_session.graph().content(ids.content).is_some());
        assert!(
            !app.ui
                .content_tabs
                .iter()
                .any(|entry| entry.identity.0 == ids.content)
        );
    }
}

#[test]
fn workspace_review_refresh_keeps_the_query_and_selected_file_but_new_proposals_reset() {
    use crate::app::picker::PickerContent;
    use crate::workspace::{PickerAction, PickerItem, PickerItemId, PickerItemKind, ProposalId};
    let (_dirs, mut app) = app();
    let scope = app.workspace.version();
    let configuration = app.configuration.binding_revision();
    let content = |id: u64, selected: bool| PickerContent {
        surface: PickerSurfaceKind::Generic,
        origin: PickerOrigin::WorkspaceEdit(ProposalId(id)),
        title: "Replacement".into(),
        query: String::new(),
        complete: true,
        items: (0..40)
            .map(|index| {
                (
                    PickerItem {
                        id: PickerItemId(format!("file:{index}")),
                        label: format!("invoice_{index:02}.rs"),
                        detail: if index == 35 && !selected {
                            "excluded"
                        } else {
                            "included"
                        }
                        .into(),
                        source: String::new(),
                        badge: String::new(),
                        action: PickerAction::None,
                        kind: PickerItemKind::Entry,
                    },
                    PickerPayload::ProposalFile(index),
                )
            })
            .collect(),
    };
    app.search
        .install_picker(content(1, true), 64, scope.clone(), configuration)
        .unwrap();
    app.search.picker_insert("invoice_3");
    let selected = PickerItemId("file:35".into());
    app.search.select_picker_item(&selected);
    assert!(app.search.picker_scroll(4).unwrap() > 0);
    let epoch = app.search.picker_epoch().unwrap().clone();

    app.search
        .install_picker(content(1, false), 64, scope.clone(), configuration)
        .unwrap();
    let picker = app.search.picker().unwrap();
    assert_eq!(picker.query(), "invoice_3");
    assert_eq!(picker.presentation().rows.len(), 13);
    assert_eq!(picker.selected_item().unwrap().id, selected);
    assert_eq!(picker.selected_item().unwrap().detail, "excluded");
    assert!(app.search.picker_scroll(4).unwrap() > 0);
    assert!(app.search.owns_picker_epoch(&epoch));
    assert!(matches!(
        app.search.selected_picker_payload(),
        Some(PickerPayload::ProposalFile(35))
    ));

    app.search
        .install_picker(content(2, true), 64, scope, configuration)
        .unwrap();
    let picker = app.search.picker().unwrap();
    assert_eq!(picker.query(), "");
    assert_eq!(picker.selected_item().unwrap().id.0, "file:0");
    assert!(!app.search.owns_picker_epoch(&epoch));
}

fn publish_problems(
    app: &mut App,
    file: &str,
    diagnostics: &[(usize, crate::workspace::DiagnosticSeverity, &str)],
) {
    use crate::app::diagnostics::{DiagnosticBatch, DiagnosticDocument, DiagnosticValidity};
    use crate::editor::text::{ByteOffset, ByteRange};
    use crate::workspace::{Diagnostic, DiagnosticOwner, DiagnosticProducer};

    let path = app.state.workspace_root.join(file);
    let (buffer, _) = app
        .state
        .open_text(
            TextBuffer::from_text("let n = 1;\nlet m = n;"),
            Some(path.clone()),
        )
        .unwrap();
    let uri = crate::language::lsp::file_uri(&path);
    let producer =
        DiagnosticProducer::new(DiagnosticOwner::new(format!("picker-test:{file}")).unwrap());
    app.diagnostics.activate(producer.clone());
    app.diagnostics
        .publish(
            DiagnosticBatch {
                producer,
                uri: uri.clone(),
                document: DiagnosticDocument::capture(&app.state, buffer),
                validity: DiagnosticValidity::RequestedSnapshot,
                disk_points: Default::default(),
                items: diagnostics
                    .iter()
                    .map(|&(offset, severity, message)| Diagnostic {
                        uri: uri.clone(),
                        version: None,
                        range: ByteRange::empty(ByteOffset(offset)),
                        severity,
                        source: "rustc".into(),
                        code: Some(if severity == crate::workspace::DiagnosticSeverity::Error {
                            "E0308".into()
                        } else {
                            serde_json::json!(7)
                        }),
                        message: message.into(),
                        related: Vec::new(),
                        tags: Vec::new(),
                        fixes: Vec::new(),
                        data: None,
                        timestamp: std::time::SystemTime::UNIX_EPOCH,
                    })
                    .collect(),
            },
            &app.state,
        )
        .unwrap();
}

#[test]
fn problems_rows_are_sorted_and_show_severity_file_location_message_and_source() {
    use crate::app::tests::driver::TestSession;
    use crate::config::ExplorerIconMode::{Ascii, NerdFont};
    use crate::workspace::DiagnosticSeverity::{Error, Hint, Information, Warning};
    let rust = crate::files::file_icon("a.rs").unwrap().glyph;
    for (mode, glyphs, file, custom) in [
        (
            NerdFont,
            ["\u{f057}", "\u{f071}", "\u{f05a}", "\u{f0eb}"],
            rust,
            "*",
        ),
        (Ascii, ["E", "W", "I", "H"], ".", "."),
    ] {
        let mut loaded = crate::config::load_bundled_config();
        loaded.config.ui.explorer.icon_mode = mode;
        loaded
            .config
            .ui
            .buffer_line
            .extension_icons
            .insert("toml".into(), "*".into());
        let mut session = TestSession::new(loaded);
        let app = session.application_mut();
        publish_problems(
            app,
            "a.rs",
            &[
                (0, Hint, "expected due to this"),
                (1, Warning, "unused variable"),
                (11, Error, "later line"),
                (2, Error, "later column"),
                (0, Error, "mismatched types\nnot visible"),
            ],
        );
        publish_problems(
            app,
            "b.toml",
            &[(0, Information, "type details"), (0, Error, "other file")],
        );
        app.set_dashboard_visibility(false);
        session.resize(120, 40);
        for key in ["space", "s", "d"] {
            session.key(key);
        }
        let rows = &session.application().ui.picker.as_ref().unwrap().rows;
        assert_eq!(
            rows.iter()
                .map(|row| (row.label.as_str(), row.detail.as_str(), row.source.as_str()))
                .collect::<Vec<_>>(),
            [
                ("a.rs:1:1", "mismatched types", "rustc E0308"),
                ("a.rs:1:3", "later column", "rustc E0308"),
                ("a.rs:2:1", "later line", "rustc E0308"),
                ("b.toml:1:1", "other file", "rustc E0308"),
                ("a.rs:1:2", "unused variable", "rustc 7"),
                ("b.toml:1:1", "type details", "rustc 7"),
                ("a.rs:1:1", "expected due to this", "rustc 7"),
            ]
        );
        let frame = session.frame();
        let (error, warning, info, hint) = (glyphs[0], glyphs[1], glyphs[2], glyphs[3]);
        for (glyph, file, location, detail, source) in [
            (error, file, "a.rs:1:1", "mismatched types", "rustc E0308"),
            (warning, file, "a.rs:1:2", "unused variable", "rustc 7"),
            (info, custom, "b.toml:1:1", "type details", "rustc 7"),
            (hint, file, "a.rs:1:1", "expected due to this", "rustc 7"),
        ] {
            let left = format!("{glyph} {file} {location}  {detail}");
            assert!(
                (0..frame.grid.height())
                    .map(|y| frame.grid.row_text(y))
                    .any(|line| {
                        line.split_once(&left).is_some_and(|(_, rest)| {
                            rest.starts_with(' ') && rest.trim_start().starts_with(source)
                        })
                    }),
                "{left} with {source}"
            );
        }
    }
}

#[test]
fn problems_in_this_file_filters_by_buffer_and_refreshes_the_open_popup() {
    use crate::app::tests::driver::TestSession;
    use crate::workspace::DiagnosticSeverity::{Error, Hint};
    let mut loaded = crate::config::load_bundled_config();
    loaded.config.lsp.inline_diagnostics = crate::config::InlineDiagnostics::Off;
    let mut session = TestSession::new(loaded);
    let app = session.application_mut();
    publish_problems(app, "other.rs", &[(0, Error, "other file")]);
    publish_problems(app, "main.rs", &[(0, Hint, "local hint")]);
    app.set_dashboard_visibility(false);
    for key in ["space", "s", "D"] {
        session.key(key);
    }
    let picker = session.application().ui.picker.as_ref().unwrap();
    assert_eq!(picker.title, "Problems in main.rs");
    assert_eq!(picker.rows.len(), 1);
    assert_eq!(picker.rows[0].detail, "local hint");
    let app = session.application_mut();
    publish_problems(app, "main.rs", &[(0, Error, "new local error")]);
    app.sync_ui_diagnostics();
    let picker = app.ui.picker.as_ref().unwrap();
    assert_eq!(picker.rows.len(), 1);
    assert_eq!(picker.rows[0].detail, "new local error");
}

#[test]
fn leader_x_x_opens_a_popup_without_taking_editor_rows() {
    use crate::app::tests::driver::TestSession;
    let mut session = TestSession::new(crate::config::load_bundled_config());
    let app = session.application_mut();
    app.state
        .open_text(TextBuffer::from_text("editor\n"), None)
        .unwrap();
    app.set_dashboard_visibility(false);
    app.reconcile_editor_windows();
    let before = session.application().workspace_bounds().unwrap();
    for key in ["space", "x", "x"] {
        session.key(key);
    }
    let app = session.application();
    assert_eq!(
        app.ui.picker.as_ref().unwrap().surface,
        PickerSurfaceKind::Problems
    );
    assert!(
        app.ui
            .content_tabs
            .iter()
            .all(|tab| tab.title != "Problems"),
        "the popup must not add a tab: {:?}",
        app.ui
            .content_tabs
            .iter()
            .map(|tab| &tab.title)
            .collect::<Vec<_>>()
    );
    let popup = app.window_session.native_ids().picker().unwrap();
    assert!(matches!(
        app.window_session
            .graph()
            .window(popup.window)
            .unwrap()
            .placement,
        crate::windows::WindowPlacement::Overlay(_)
    ));
    assert_eq!(app.workspace_bounds(), Some(before));
    session.key("escape");
    assert_eq!(session.context(), "picker_normal");
    session.key("escape");
    assert!(session.application().ui.picker.is_none());
}

#[test]
fn tab_problem_badge_uses_error_count_not_warning_or_related_hints() {
    use crate::app::tests::driver::TestSession;
    use crate::workspace::DiagnosticSeverity::{Error, Hint, Information, Warning};
    let mut session = TestSession::new(crate::config::load_bundled_config());
    let app = session.application_mut();
    publish_problems(
        app,
        "main.rs",
        &[
            (0, Error, "mismatched types"),
            (1, Hint, "expected due to this"),
            (2, Hint, "type fixed here"),
            (3, Information, "context"),
            (4, Warning, "unused item"),
        ],
    );
    app.set_dashboard_visibility(false);
    app.sync_ui_diagnostics();
    app.reconcile_editor_windows();
    let tab = app
        .ui
        .content_tabs
        .iter()
        .find(|tab| tab.title == "main.rs")
        .unwrap();
    let segment = crate::ui::render::overlays::builtin_content_tab_segments(
        tab,
        app.configuration.settings(),
    )
    .into_iter()
    .find(|segment| segment.name == "diagnostics")
    .unwrap();
    assert_eq!(segment.text, " \u{f057} 1");
    assert_eq!(
        segment.style,
        crate::ui::Style::BufferLineDiagnostic(
            crate::ui::ContentTabDiagnosticSeverity::Error,
            tab.active
        )
    );
}

#[test]
fn picker_title_changes_update_the_retained_window_content() {
    let (_app_dirs, mut app) = app();
    app.open_builtin_picker(PickerKind::Buffers);
    app.reconcile_editor_windows();
    let files = app.window_session.native_ids().picker().unwrap();
    assert_eq!(
        app.window_session
            .graph()
            .content(files.content)
            .unwrap()
            .title,
        "Buffers"
    );
    app.open_builtin_picker(PickerKind::Commands);
    app.reconcile_editor_windows();
    assert_eq!(app.window_session.native_ids().picker(), Some(files));
    assert_eq!(
        app.window_session
            .graph()
            .content(files.content)
            .unwrap()
            .title,
        "Commands"
    );
    app.process_effect(AppEffect::WindowList);
    assert!(
        app.ui
            .picker
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .any(|row| row.label.contains("Commands"))
    );
}
