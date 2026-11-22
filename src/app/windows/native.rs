//! Native surface resource bindings and placement policy.
use crate::app::windows::content::sanitize_title;
use crate::ui::PickerSurfaceKind;
use crate::windows::{
    CompositorSnapshot, ContentAdapterKind, ContentContract, ContentId, ContentLifecycle,
    ContentTypeId, ViewId, WindowError, WindowGraph, WindowId, WindowPlacement, WindowRect,
    WorkspaceTabId,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::app) struct NativeSurfaceSignature {
    pub editor_tab: Option<WorkspaceTabId>,
    pub editor_window: Option<WindowId>,
    pub focused_plugin: Option<WindowId>,
    pub explorer_visible: bool,
    pub explorer_views: Arc<[u64]>,
    pub focused_explorer: Option<u64>,
    pub explorer_size: u16,
    pub explorer_edge: crate::windows::DockEdge,
    pub active_terminal: Option<u64>,
    /// Terminals that keep their windows while another is active: the panel's terminal and side
    /// terminals such as a docked agent.
    pub shown_terminals: Arc<[u64]>,
    pub terminal_focused: bool,
    pub agent_placement: Option<WindowPlacement>,
    pub agent_focused: bool,
    pub acp_agent_shown: bool,
    pub shown_agent_terminal: Option<u64>,
    pub agent_terminals: Arc<[u64]>,
    pub terminal_size: u16,
    pub terminal_shrunk: bool,
    pub terminal_set_revision: Option<u64>,
    pub terminal_width: u16,
    pub terminal_height: u16,
    pub dashboard_visible: bool,
    pub picker_rect: Option<crate::windows::WindowRect>,
    pub picker_surface: Option<PickerSurfaceKind>,
    pub picker_title: String,
    pub problems_panel: bool,
    pub problems_top_inset: u16,
    pub problems_panel_focused: bool,
    pub protected_prompt_height: u16,
    pub configuration_reload_id: u64,
    pub screen_bottom: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoveringAgent {
    Acp,
    Terminal(u64),
}

impl CoveringAgent {
    fn is_shown(self, signature: &NativeSurfaceSignature) -> bool {
        match self {
            Self::Acp => signature.acp_agent_shown,
            Self::Terminal(id) => signature.shown_agent_terminal == Some(id),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CoveredShell {
    id: u64,
    agent: CoveringAgent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) struct NativeSurfaceIds {
    pub content: ContentId,
    pub view: ViewId,
    pub window: WindowId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) enum NativeSurfaceTarget {
    Explorer,
    ExplorerView(u64),
    Terminal(u64),
    Dashboard,
    Picker(PickerSurfaceKind),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(in crate::app) struct NativeSurfaces {
    explorer: Option<NativeSurfaceIds>,
    explorer_views: BTreeMap<u64, NativeSurfaceIds>,
    dashboard: Option<NativeSurfaceIds>,
    agent: Option<NativeSurfaceIds>,
    pickers: BTreeMap<PickerSurfaceKind, NativeSurfaceIds>,
    terminals: HashMap<u64, NativeSurfaceIds>,
    explorer_placement: Option<WindowPlacement>,
    explorer_restore_placement: Option<WindowPlacement>,
    explorer_view_placements: BTreeMap<u64, WindowPlacement>,
    explorer_view_restore_placements: BTreeMap<u64, WindowPlacement>,
    explorer_closed: bool,
    dashboard_placement: Option<WindowPlacement>,
    dashboard_restore_placement: Option<WindowPlacement>,
    picker_placements: BTreeMap<PickerSurfaceKind, WindowPlacement>,
    picker_restore_placements: BTreeMap<PickerSurfaceKind, WindowPlacement>,
    terminal_default_placement: Option<WindowPlacement>,
    terminal_placements: HashMap<u64, WindowPlacement>,
    terminal_restore_placements: HashMap<u64, WindowPlacement>,
    covered_shell: Option<CoveredShell>,
    configured_placements: BTreeMap<String, WindowPlacement>,
    workspace_tab: Option<WorkspaceTabId>,
    signature: Option<NativeSurfaceSignature>,
}

/// Bottom terminals stay at the outside edge; Problems is nearest the editor regardless of
/// which surface was opened first. Other window layers keep their order.
fn order_problems_dock(snapshot: &mut CompositorSnapshot, panel: WindowId) {
    if let Some(index) = snapshot.windows.iter().position(|window| {
        window.window == panel
            && matches!(
                window.placement,
                WindowPlacement::Docked {
                    edge: crate::windows::DockEdge::Bottom,
                    ..
                }
            )
    }) {
        let window = snapshot.windows.remove(index);
        snapshot.windows.push(window);
    }
}

impl NativeSurfaces {
    pub(in crate::app) fn is_current(&self, signature: &NativeSurfaceSignature) -> bool {
        signature.terminal_set_revision.is_some() && self.signature.as_ref() == Some(signature)
    }

    pub(in crate::app) fn invalidate(&mut self) {
        self.signature = None;
    }

    pub(in crate::app) fn reconcile(
        &mut self,
        graph: &mut WindowGraph,
        signature: NativeSurfaceSignature,
        explorer_title: &str,
        dashboard_title: &str,
        picker_title: &str,
        terminals: &[(u64, String, ContentLifecycle)],
    ) -> Result<(), WindowError> {
        let explorer_title = sanitize_title(explorer_title);
        let dashboard_title = sanitize_title(dashboard_title);
        let picker_title = sanitize_title(picker_title);
        let owner = "builtin.surfaces";
        let instance_id = 1;
        // Hidden retained native windows still need a host while editor tabs
        // are replaced; skipping them leaves stale tabs non-empty.
        if self.workspace_tab.is_none()
            && signature.editor_tab.is_none()
            && !signature.explorer_visible
            && !signature.dashboard_visible
            && signature.agent_placement.is_none()
            && signature.picker_rect.is_none()
            && !signature.problems_panel
            && terminals.is_empty()
        {
            self.signature = Some(signature);
            return Ok(());
        }
        if signature.explorer_views.first().copied() != Some(1)
            || !signature
                .explorer_views
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        {
            return Err(WindowError::UnknownView);
        }
        let live_ids = |ids: NativeSurfaceIds, graph: &WindowGraph| {
            graph.content(ids.content).is_some()
                && graph.view(ids.view).is_some()
                && graph.window(ids.window).is_some()
        };
        self.explorer = self.explorer.filter(|ids| live_ids(*ids, graph));
        self.explorer_views.retain(|_, ids| live_ids(*ids, graph));
        let semantic_explorer_views = signature
            .explorer_views
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let closed_explorer_views = self
            .explorer_views
            .iter()
            .filter_map(|(view, ids)| {
                (!semantic_explorer_views.contains(view)).then_some((*view, *ids))
            })
            .collect::<Vec<_>>();
        for (view, ids) in closed_explorer_views {
            graph.close_window(ids.window)?;
            graph.remove_view(ids.view)?;
            self.explorer_views.remove(&view);
            self.explorer_view_placements.remove(&view);
            self.explorer_view_restore_placements.remove(&view);
        }
        self.dashboard = self.dashboard.filter(|ids| live_ids(*ids, graph));
        self.agent = self.agent.filter(|ids| live_ids(*ids, graph));
        self.pickers.retain(|_, ids| live_ids(*ids, graph));
        self.terminals.retain(|_, ids| live_ids(*ids, graph));

        let previous_tab = self.workspace_tab;
        let retained_plugin_focus = signature
            .focused_plugin
            .or(graph.focused())
            .filter(|window| {
                graph.window(*window).is_some_and(|descriptor| {
                    descriptor.owner.starts_with("plugin.")
                        && descriptor.focusable
                        && descriptor.placement != WindowPlacement::Hidden
                })
            });
        let retained_tab = self
            .workspace_tab
            .or(graph.active_tab())
            .filter(|tab| graph.workspace_tab(*tab).is_some());
        let tab = if let Some(tab) = signature.editor_tab.or(retained_tab) {
            tab
        } else {
            let tab = graph.create_workspace_tab(owner, instance_id)?;
            self.workspace_tab = Some(tab);
            tab
        };
        self.workspace_tab = Some(tab);
        let agent_placement = signature.agent_placement.unwrap_or(WindowPlacement::Hidden);
        let fullscreen_agent = if agent_placement == WindowPlacement::Fullscreen {
            Some(CoveringAgent::Acp)
        } else {
            signature
                .shown_agent_terminal
                .filter(|id| {
                    self.requested_terminal_placement(*id) == Some(WindowPlacement::Fullscreen)
                })
                .map(CoveringAgent::Terminal)
        };
        let previously_covered = self.covered_shell.filter(|shell| {
            terminals.iter().any(|(id, ..)| *id == shell.id)
                && self.requested_terminal_placement(shell.id) == Some(WindowPlacement::Fullscreen)
        });
        // Only an actual fullscreen takeover starts a cover. Once started, it follows the
        // agent through placement changes until the agent is hidden or removed.
        self.covered_shell = previously_covered
            .filter(|shell| shell.agent.is_shown(&signature))
            .or_else(|| {
                let agent = fullscreen_agent?;
                self.terminals.iter().find_map(|(&id, ids)| {
                    (agent != CoveringAgent::Terminal(id)
                        && terminals.iter().any(|(live, ..)| *live == id)
                        && graph.window(ids.window).is_some_and(|window| {
                            window.tab == previous_tab.unwrap_or(tab)
                                && window.placement == WindowPlacement::Fullscreen
                        }))
                    .then_some(CoveredShell { id, agent })
                })
            });
        let covered_shell = self.covered_shell.map(|shell| shell.id);
        let restoring_shell = previously_covered.filter(|_| covered_shell.is_none());

        if let Some(ids) = self.agent
            && agent_placement != WindowPlacement::Fullscreen
        {
            graph.rehost(ids.window, tab, ids.view, agent_placement, true)?;
        }
        // Release outgoing fullscreen terminals before placing the explorer.
        for (id, ids) in &self.terminals {
            let remains_fullscreen = (signature.active_terminal == Some(*id)
                || signature.shown_terminals.contains(id))
                && terminals.iter().any(|(live, ..)| live == id)
                && self
                    .terminal_placement(*id)
                    .or_else(|| self.configured_placements.get("builtin.terminal").copied())
                    == Some(WindowPlacement::Fullscreen);
            if (!remains_fullscreen || covered_shell == Some(*id))
                && graph
                    .window(ids.window)
                    .is_some_and(|window| window.placement == WindowPlacement::Fullscreen)
            {
                graph.place(ids.window, WindowPlacement::Hidden)?;
            }
        }
        let default_explorer_placement = WindowPlacement::Docked {
            edge: signature.explorer_edge,
            size: signature.explorer_size.max(1),
        };
        if signature.focused_explorer == Some(1)
            && self.explorer_placement == Some(WindowPlacement::Hidden)
        {
            self.explorer_placement = self
                .explorer_restore_placement
                .filter(|p| *p != WindowPlacement::Hidden);
        }
        if let Some(view) = signature.focused_explorer.filter(|view| *view > 1)
            && self.explorer_view_placements.get(&view) == Some(&WindowPlacement::Hidden)
        {
            self.explorer_view_placements.remove(&view);
            if let Some(placement) = self
                .explorer_view_restore_placements
                .get(&view)
                .copied()
                .filter(|p| *p != WindowPlacement::Hidden)
            {
                self.explorer_view_placements.insert(view, placement);
            }
        }
        let explorer_placement = if signature.explorer_visible {
            self.explorer_placement
                .or_else(|| self.configured_placements.get("builtin.explorer").copied())
                .unwrap_or(default_explorer_placement)
        } else {
            WindowPlacement::Hidden
        };
        let explorer = if self.explorer_closed {
            None
        } else if let Some(ids) = self.explorer {
            graph.update_content(ids.content, explorer_title.to_owned(), false)?;
            graph.rehost(ids.window, tab, ids.view, explorer_placement, true)?;
            Some(ids)
        } else {
            let mut capabilities =
                crate::windows::ContentCapabilities::for_adapter(ContentAdapterKind::DeclarativeUi);
            capabilities.clone_view = true;
            let content = graph.create_content_with_contract(
                ContentTypeId::new("builtin.explorer")?,
                ContentContract::declared(ContentAdapterKind::DeclarativeUi, capabilities),
                explorer_title.to_owned(),
                false,
                owner,
                instance_id,
            )?;
            let view = graph.create_view(content, owner, instance_id)?;
            let window =
                graph.open_window(tab, view, explorer_placement, true, owner, instance_id)?;
            let ids = NativeSurfaceIds {
                content,
                view,
                window,
            };
            self.explorer = Some(ids);
            Some(ids)
        };
        if let Some(explorer) = explorer {
            for view in signature
                .explorer_views
                .iter()
                .copied()
                .filter(|view| *view > 1)
            {
                let placement = self.explorer_view_placements.get(&view).copied().unwrap_or(
                    WindowPlacement::Floating(WindowRect {
                        x: 2,
                        y: 2,
                        width: 40,
                        height: 20,
                    }),
                );
                let shown_placement = if signature.explorer_visible {
                    placement
                } else {
                    WindowPlacement::Hidden
                };
                if let Some(ids) = self.explorer_views.get(&view).copied() {
                    graph.rehost(ids.window, tab, ids.view, shown_placement, true)?;
                } else {
                    let cloned_view = graph.create_view(explorer.content, owner, instance_id)?;
                    let window = graph.open_window(
                        tab,
                        cloned_view,
                        shown_placement,
                        true,
                        owner,
                        instance_id,
                    )?;
                    self.explorer_views.insert(
                        view,
                        NativeSurfaceIds {
                            content: explorer.content,
                            view: cloned_view,
                            window,
                        },
                    );
                    self.explorer_view_placements.insert(view, placement);
                    self.explorer_view_restore_placements
                        .insert(view, placement);
                }
            }
        }

        let dashboard_placement = if signature.dashboard_visible {
            self.dashboard_placement
                .or_else(|| self.configured_placements.get("builtin.dashboard").copied())
                .unwrap_or(WindowPlacement::Overlay(crate::windows::WindowRect {
                    x: 0,
                    y: 0,
                    width: signature.terminal_width.max(1),
                    height: signature.terminal_height.max(1),
                }))
        } else {
            WindowPlacement::Hidden
        };
        let dashboard = if let Some(ids) = self.dashboard {
            graph.update_content(ids.content, dashboard_title.to_owned(), false)?;
            graph.rehost(ids.window, tab, ids.view, dashboard_placement, true)?;
            Some(ids)
        } else if signature.dashboard_visible {
            let content = graph.create_content(
                ContentTypeId::new("builtin.dashboard")?,
                ContentAdapterKind::DeclarativeUi,
                dashboard_title.to_owned(),
                false,
                owner,
                instance_id,
            )?;
            let view = graph.create_view(content, owner, instance_id)?;
            let window =
                graph.open_window(tab, view, dashboard_placement, true, owner, instance_id)?;
            let ids = NativeSurfaceIds {
                content,
                view,
                window,
            };
            self.dashboard = Some(ids);
            Some(ids)
        } else {
            None
        };

        let active_picker = signature.picker_rect.zip(signature.picker_surface);
        for (kind, ids) in &self.pickers {
            if active_picker.is_none_or(|(_, active)| active != *kind)
                && (*kind != PickerSurfaceKind::ProblemsPanel || !signature.problems_panel)
            {
                graph.rehost(ids.window, tab, ids.view, WindowPlacement::Hidden, true)?;
            }
        }
        let picker = if let Some((rect, kind)) = active_picker {
            let content_type = match kind {
                PickerSurfaceKind::Generic
                | PickerSurfaceKind::Choice
                | PickerSurfaceKind::Files
                | PickerSurfaceKind::Grep => "builtin.picker",
                PickerSurfaceKind::Problems => "builtin.problems",
                PickerSurfaceKind::ProblemsPanel => "builtin.problems.panel",
                PickerSurfaceKind::Output => "builtin.output",
                PickerSurfaceKind::Logs => "builtin.logs",
                PickerSurfaceKind::Debug => "builtin.debug",
                PickerSurfaceKind::Theme => "builtin.theme",
                PickerSurfaceKind::Changes => "builtin.changes",
            };
            let placement = self
                .picker_placements
                .get(&kind)
                .copied()
                .or_else(|| self.configured_placements.get(content_type).copied())
                .unwrap_or(WindowPlacement::Overlay(rect));
            if let Some(ids) = self.pickers.get(&kind).copied() {
                graph.update_content(ids.content, picker_title.to_owned(), false)?;
                graph.rehost(ids.window, tab, ids.view, placement, true)?;
                Some(ids)
            } else {
                let content = graph.create_content(
                    ContentTypeId::new(content_type)?,
                    ContentAdapterKind::DeclarativeUi,
                    picker_title.to_owned(),
                    false,
                    owner,
                    instance_id,
                )?;
                let view = graph.create_view(content, owner, instance_id)?;
                let window = graph.open_window(tab, view, placement, true, owner, instance_id)?;
                let ids = NativeSurfaceIds {
                    content,
                    view,
                    window,
                };
                self.pickers.insert(kind, ids);
                Some(ids)
            }
        } else {
            None
        };

        let problems_panel = if signature.problems_panel {
            let kind = PickerSurfaceKind::ProblemsPanel;
            let placement =
                self.picker_placements
                    .get(&kind)
                    .copied()
                    .unwrap_or(WindowPlacement::Docked {
                        edge: crate::windows::DockEdge::Bottom,
                        size: 10,
                    });
            let ids = if let Some(ids) = self.pickers.get(&kind).copied() {
                graph.rehost(ids.window, tab, ids.view, placement, true)?;
                ids
            } else {
                let content = graph.create_content(
                    ContentTypeId::new("builtin.problems.panel")?,
                    ContentAdapterKind::DeclarativeUi,
                    "Problems".into(),
                    false,
                    owner,
                    instance_id,
                )?;
                let view = graph.create_view(content, owner, instance_id)?;
                let window = graph.open_window(tab, view, placement, true, owner, instance_id)?;
                let ids = NativeSurfaceIds {
                    content,
                    view,
                    window,
                };
                self.pickers.insert(kind, ids);
                ids
            };
            Some(ids)
        } else {
            None
        };

        let live = terminals
            .iter()
            .map(|(id, ..)| *id)
            .collect::<std::collections::BTreeSet<_>>();
        let stale = self
            .terminals
            .iter()
            .filter_map(|(id, ids)| (!live.contains(id)).then_some((*id, *ids)))
            .collect::<Vec<_>>();
        for (id, ids) in stale {
            graph.close_window(ids.window)?;
            graph.remove_view(ids.view)?;
            graph.remove_content(ids.content)?;
            self.terminals.remove(&id);
            self.terminal_placements.remove(&id);
            self.terminal_restore_placements.remove(&id);
        }
        let mut placements = terminals
            .iter()
            .map(|(id, profile, lifecycle)| {
                let placement = if signature.active_terminal == Some(*id)
                    || signature.shown_terminals.contains(id)
                    || restoring_shell.is_some_and(|shell| shell.id == *id)
                {
                    self.requested_terminal_placement(*id)
                        .unwrap_or(WindowPlacement::Docked {
                            edge: crate::windows::DockEdge::Bottom,
                            size: signature.terminal_size.max(1),
                        })
                } else {
                    WindowPlacement::Hidden
                };
                let placement = match placement {
                    WindowPlacement::Fullscreen if covered_shell == Some(*id) => {
                        // Keep the shell's requested placement intact so it can return.
                        WindowPlacement::Hidden
                    }
                    WindowPlacement::Docked {
                        edge:
                            edge @ (crate::windows::DockEdge::Top
                            | crate::windows::DockEdge::Bottom),
                        size,
                    } if signature.terminal_shrunk => WindowPlacement::Docked {
                        edge,
                        size: size.min(signature.terminal_size.max(1)),
                    },
                    placement => placement,
                };
                (id, profile, lifecycle, placement)
            })
            .collect::<Vec<_>>();
        // Release the outgoing fullscreen host before showing its replacement.
        placements.sort_by_key(|(_, _, _, placement)| *placement == WindowPlacement::Fullscreen);
        for (id, profile, lifecycle, placement) in placements {
            let profile = sanitize_title(profile);
            if let Some(ids) = self.terminals.get(id).copied() {
                graph.update_content_lifecycle(ids.content, profile.clone(), false, *lifecycle)?;
                graph.rehost(ids.window, tab, ids.view, placement, true)?;
            } else {
                let content = graph.create_content_with_contract(
                    ContentTypeId::new("builtin.terminal")?,
                    ContentContract::adapter_defaults(ContentAdapterKind::TerminalSession)
                        .with_lifecycle(*lifecycle),
                    profile.clone(),
                    false,
                    owner,
                    instance_id,
                )?;
                let view = graph.create_view(content, owner, instance_id)?;
                let window = graph.open_window(tab, view, placement, true, owner, instance_id)?;
                self.terminals.insert(
                    *id,
                    NativeSurfaceIds {
                        content,
                        view,
                        window,
                    },
                );
            }
        }

        if let Some(ids) = self.agent {
            if agent_placement == WindowPlacement::Fullscreen {
                graph.rehost(ids.window, tab, ids.view, agent_placement, true)?;
            }
        } else if agent_placement != WindowPlacement::Hidden {
            let content = graph.create_content(
                ContentTypeId::new("builtin.agent")?,
                ContentAdapterKind::DeclarativeUi,
                "Agent".into(),
                false,
                owner,
                instance_id,
            )?;
            let view = graph.create_view(content, owner, instance_id)?;
            let window = graph.open_window(tab, view, agent_placement, true, owner, instance_id)?;
            self.agent = Some(NativeSurfaceIds {
                content,
                view,
                window,
            });
        }

        let problems_visible = if let Some(ids) = problems_panel {
            let mut snapshot = graph
                .compositor_snapshot(tab)
                .ok_or(WindowError::UnknownWorkspaceTab)?;
            let visible = self.fit_problems_dock(
                &mut snapshot,
                ids.window,
                picker.map(|ids| ids.window),
                &signature,
            );
            // Only visibility affects graph focus. The dock sizes are projection-only.
            if !visible {
                graph.place(ids.window, WindowPlacement::Hidden)?;
            }
            visible
        } else {
            false
        };

        let mut focused = false;
        if signature.picker_rect.is_some()
            && let Some(picker) = picker
            && graph
                .window(picker.window)
                .is_some_and(|window| window.placement != WindowPlacement::Hidden)
            && graph.can_focus(picker.window)
        {
            graph.focus(picker.window)?;
            focused = true;
        }
        if !focused
            && signature.agent_focused
            && let Some(ids) = self.agent
            && graph.can_focus(ids.window)
        {
            graph.focus(ids.window)?;
            focused = true;
        }
        if !focused
            && signature.problems_panel_focused
            && problems_visible
            && let Some(ids) = problems_panel
            && graph.can_focus(ids.window)
        {
            graph.focus(ids.window)?;
            focused = true;
        }
        if !focused
            && signature.terminal_focused
            && let Some(ids) = signature
                .active_terminal
                .and_then(|id| self.terminals.get(&id))
            && graph.can_focus(ids.window)
        {
            graph.focus(ids.window)?;
            focused = true;
        }
        if !focused {
            if let Some(view) = signature.focused_explorer {
                let ids = if view == 1 {
                    explorer
                } else {
                    self.explorer_views.get(&view).copied()
                };
                if let Some(ids) = ids
                    && graph.can_focus(ids.window)
                {
                    graph.focus(ids.window)?;
                }
            } else if let Some(editor) = signature.editor_window {
                if graph
                    .window(editor)
                    .is_some_and(|window| window.placement != WindowPlacement::Hidden)
                {
                    if graph.can_focus(editor) {
                        graph.focus(editor)?;
                    }
                } else if signature.dashboard_visible
                    && let Some(dashboard) = dashboard
                    && graph.can_focus(dashboard.window)
                {
                    graph.focus(dashboard.window)?;
                }
            } else if signature.dashboard_visible
                && let Some(dashboard) = dashboard
                && graph.can_focus(dashboard.window)
            {
                graph.focus(dashboard.window)?;
            }
        }
        if signature.picker_rect.is_none()
            && !signature.agent_focused
            && !signature.problems_panel_focused
            && !signature.terminal_focused
            && signature.focused_explorer.is_none()
            && let Some(plugin) = retained_plugin_focus
            && graph
                .window(plugin)
                .is_some_and(|descriptor| descriptor.tab == tab)
            && graph.can_focus(plugin)
        {
            graph.focus(plugin)?;
        }
        if signature.editor_tab == Some(tab) {
            self.workspace_tab = Some(tab);
        }
        if let Some(previous) = previous_tab.filter(|previous| *previous != tab)
            && graph
                .workspace_tab(previous)
                .is_some_and(|_| !graph.tab_has_windows(previous))
        {
            graph.remove_workspace_tab(previous)?;
        }
        graph.validate()?;
        self.signature = Some(signature);
        Ok(())
    }

    /// Fit only the compositor's temporary layout input. Graph placements, including
    /// the normal terminal size used by hide, resize and session save, are unchanged.
    pub(super) fn fit_problems_projection(&self, snapshot: &mut CompositorSnapshot) {
        if let Some(signature) = &self.signature
            && signature.problems_panel
            && let Some(panel) = self.problems_panel()
        {
            self.fit_problems_dock(
                snapshot,
                panel.window,
                self.picker().map(|ids| ids.window),
                signature,
            );
        }
    }

    /// Budget both docks before projecting either, regardless of creation order.
    fn fit_problems_dock(
        &self,
        snapshot: &mut CompositorSnapshot,
        panel: WindowId,
        picker: Option<WindowId>,
        signature: &NativeSurfaceSignature,
    ) -> bool {
        use crate::windows::{DockEdge, PANE_MINIMUM_HEIGHT, PANE_MINIMUM_WIDTH, TiledInsets};
        const MINIMUM_PROBLEMS: u16 = 2; // title and one selectable/empty row
        const MINIMUM_TERMINAL: u16 = 3; // terminal renderer's minimum
        const MINIMUM_PICKER: u16 = 6;

        let Some(panel_index) = snapshot
            .windows
            .iter()
            .position(|window| window.window == panel)
        else {
            return false;
        };
        let panel_placement = snapshot.windows[panel_index].placement;
        let bounds = WindowRect {
            x: 0,
            y: 0,
            width: signature.terminal_width,
            height: signature.terminal_height,
        };
        let insets = TiledInsets {
            screen_bottom: signature.screen_bottom,
            top: signature.problems_top_inset,
            ..TiledInsets::default()
        };
        let picker_index =
            picker.and_then(|id| snapshot.windows.iter().position(|w| w.window == id));
        let picker_placement = picker_index.map(|index| snapshot.windows[index].placement);
        if let WindowPlacement::Docked {
            edge: DockEdge::Bottom,
            size: requested,
        } = panel_placement
        {
            let main_terminal = signature
                .active_terminal
                .into_iter()
                .chain(signature.shown_terminals.iter().copied())
                .filter_map(|id| self.terminals.get(&id))
                .find_map(|ids| {
                    let index = snapshot
                        .windows
                        .iter()
                        .position(|window| window.window == ids.window)?;
                    match snapshot.windows[index].placement {
                        WindowPlacement::Docked {
                            edge: edge @ (DockEdge::Top | DockEdge::Bottom),
                            size,
                        } => Some((index, edge, size)),
                        _ => None,
                    }
                });
            let workspace_min = if matches!(
                picker_placement,
                Some(WindowPlacement::Floating(_) | WindowPlacement::Overlay(_))
            ) {
                PANE_MINIMUM_HEIGHT.max(MINIMUM_PICKER)
            } else {
                PANE_MINIMUM_HEIGHT
            };
            let budget = snapshot.windows.iter().enumerate().fold(
                bounds
                    .height
                    .saturating_sub(insets.screen_bottom)
                    .saturating_sub(insets.top)
                    .saturating_sub(workspace_min),
                |available, (index, window)| match window.placement {
                    WindowPlacement::Docked {
                        edge: DockEdge::Top | DockEdge::Bottom,
                        size,
                    } if window.window != panel
                        && main_terminal.is_none_or(|(terminal, _, _)| index != terminal) =>
                    {
                        available.saturating_sub(size)
                    }
                    _ => available,
                },
            );
            let terminal_min = main_terminal.map_or(0, |(_, _, size)| size.min(MINIMUM_TERMINAL));
            if bounds.width == 0
                || budget < MINIMUM_PROBLEMS + terminal_min
                || snapshot
                    .windows
                    .iter()
                    .any(|window| window.placement == WindowPlacement::Fullscreen)
            {
                return false;
            }
            let terminal_rows =
                main_terminal.map_or(0, |(_, _, size)| size.min(budget - MINIMUM_PROBLEMS));
            if let Some((index, edge, _)) = main_terminal {
                snapshot.windows[index].placement = WindowPlacement::Docked {
                    edge,
                    size: terminal_rows,
                };
            }
            snapshot.windows[panel_index].placement = WindowPlacement::Docked {
                edge: DockEdge::Bottom,
                size: requested.clamp(MINIMUM_PROBLEMS, budget - terminal_rows),
            };
        }

        order_problems_dock(snapshot, panel);
        let projected = snapshot.project_with_tiled_insets(
            bounds,
            PANE_MINIMUM_WIDTH,
            PANE_MINIMUM_HEIGHT,
            insets,
        );
        let Some(panel_rect) = projected
            .window(panel)
            .map(|window| window.rect)
            .filter(|rect| rect.height >= MINIMUM_PROBLEMS)
        else {
            return false;
        };
        if let Some(
            placement @ (WindowPlacement::Overlay(mut rect) | WindowPlacement::Floating(mut rect)),
        ) = picker_placement
            && panel_rect.y < rect.y.saturating_add(rect.height)
            && rect.y < panel_rect.y.saturating_add(panel_rect.height)
        {
            // Put the picker above the dock, rather than painting it and masking its rows.
            // Renderer and mouse input use the same projected rectangle.
            rect.height = rect.height.min(panel_rect.y);
            rect.y = panel_rect.y.saturating_sub(rect.height) / 2;
            let placement = if matches!(placement, WindowPlacement::Overlay(_)) {
                WindowPlacement::Overlay(rect)
            } else {
                WindowPlacement::Floating(rect)
            };
            if rect.height >= MINIMUM_PICKER
                && let Some(window) = snapshot
                    .windows
                    .iter_mut()
                    .find(|window| Some(window.window) == picker)
            {
                window.placement = placement;
            }
        }
        true
    }

    pub(in crate::app) fn workspace_tab(&self) -> Option<WorkspaceTabId> {
        self.workspace_tab
    }

    pub(in crate::app) fn explorer(&self) -> Option<NativeSurfaceIds> {
        self.explorer
    }

    pub(in crate::app) fn explorer_view(&self, view: u64) -> Option<NativeSurfaceIds> {
        if view == 1 {
            self.explorer
        } else {
            self.explorer_views.get(&view).copied()
        }
    }

    pub(in crate::app) fn explorer_view_count(&self) -> usize {
        usize::from(self.explorer.is_some()).saturating_add(self.explorer_views.len())
    }

    pub(in crate::app) fn explorer_views(
        &self,
    ) -> impl Iterator<Item = (u64, NativeSurfaceIds)> + '_ {
        self.explorer
            .map(|ids| (1, ids))
            .into_iter()
            .chain(self.explorer_views.iter().map(|(view, ids)| (*view, *ids)))
    }

    pub(in crate::app) fn covered_shell_id(&self) -> Option<u64> {
        self.covered_shell.map(|shell| shell.id)
    }

    pub(in crate::app) fn terminal_agent_covers_shell(&self, agent: u64) -> bool {
        self.covered_shell
            .is_some_and(|shell| shell.agent == CoveringAgent::Terminal(agent))
    }

    /// Restore the shell in the same candidate that hides its covering agent PTY.
    pub(super) fn take_shell_covered_by_terminal_agent(&mut self, agent: u64) -> Option<WindowId> {
        if !self.terminal_agent_covers_shell(agent) {
            return None;
        }
        let shell = self.covered_shell.take()?;
        if self.requested_terminal_placement(shell.id) != Some(WindowPlacement::Fullscreen) {
            return None;
        }
        self.terminal(shell.id).map(|ids| ids.window)
    }

    pub(in crate::app) fn saved_terminal_placement(&self, id: u64) -> Option<WindowPlacement> {
        self.terminal_restore_placements.get(&id).copied()
    }

    pub(super) fn is_agent_terminal(&self, id: u64) -> bool {
        self.signature
            .as_ref()
            .is_some_and(|signature| signature.agent_terminals.contains(&id))
    }

    pub(in crate::app) fn agent(&self) -> Option<NativeSurfaceIds> {
        self.agent
    }

    pub(in crate::app) fn dashboard(&self) -> Option<NativeSurfaceIds> {
        self.dashboard
    }

    pub(in crate::app) fn picker(&self) -> Option<NativeSurfaceIds> {
        self.signature
            .as_ref()
            .and_then(|signature| signature.picker_surface)
            .and_then(|kind| self.pickers.get(&kind).copied())
    }

    pub(in crate::app) fn problems_panel(&self) -> Option<NativeSurfaceIds> {
        self.pickers.get(&PickerSurfaceKind::ProblemsPanel).copied()
    }

    #[cfg(test)]
    pub(in crate::app) fn picker_for(&self, kind: PickerSurfaceKind) -> Option<NativeSurfaceIds> {
        self.pickers.get(&kind).copied()
    }

    pub(in crate::app) fn terminal(&self, id: u64) -> Option<NativeSurfaceIds> {
        self.terminals.get(&id).copied()
    }

    pub(in crate::app) fn terminal_placement(&self, id: u64) -> Option<WindowPlacement> {
        self.terminal_placements
            .get(&id)
            .copied()
            .or(self.terminal_default_placement)
    }

    /// The semantic placement can differ from the graph while an agent covers a shell.
    pub(super) fn requested_terminal_placement(&self, id: u64) -> Option<WindowPlacement> {
        self.terminal_placement(id)
            .or_else(|| self.configured_placements.get("builtin.terminal").copied())
    }

    pub(in crate::app) fn target_for_window(
        &self,
        window: WindowId,
    ) -> Option<NativeSurfaceTarget> {
        if self.explorer.is_some_and(|ids| ids.window == window) {
            return Some(NativeSurfaceTarget::Explorer);
        }
        if let Some(view) = self
            .explorer_views
            .iter()
            .find_map(|(view, ids)| (ids.window == window).then_some(*view))
        {
            return Some(NativeSurfaceTarget::ExplorerView(view));
        }
        if self.dashboard.is_some_and(|ids| ids.window == window) {
            return Some(NativeSurfaceTarget::Dashboard);
        }
        if let Some(target) = self.pickers.iter().find_map(|(kind, ids)| {
            (ids.window == window).then_some(NativeSurfaceTarget::Picker(*kind))
        }) {
            return Some(target);
        }
        self.terminals.iter().find_map(|(id, ids)| {
            (ids.window == window).then_some(NativeSurfaceTarget::Terminal(*id))
        })
    }

    pub(in crate::app) fn terminal_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.terminals.keys().copied()
    }

    pub(in crate::app) fn ids_for_target(
        &self,
        target: NativeSurfaceTarget,
    ) -> Option<NativeSurfaceIds> {
        match target {
            NativeSurfaceTarget::Explorer => self.explorer,
            NativeSurfaceTarget::ExplorerView(view) => self.explorer_views.get(&view).copied(),
            NativeSurfaceTarget::Terminal(id) => self.terminals.get(&id).copied(),
            NativeSurfaceTarget::Dashboard => self.dashboard,
            NativeSurfaceTarget::Picker(kind) => self.pickers.get(&kind).copied(),
        }
    }

    pub(in crate::app) fn placement_for_target(
        &self,
        target: NativeSurfaceTarget,
    ) -> Option<WindowPlacement> {
        match target {
            NativeSurfaceTarget::Explorer => self.explorer_placement,
            NativeSurfaceTarget::ExplorerView(view) => {
                self.explorer_view_placements.get(&view).copied()
            }
            NativeSurfaceTarget::Terminal(id) => self.terminal_placement(id),
            NativeSurfaceTarget::Dashboard => self.dashboard_placement,
            NativeSurfaceTarget::Picker(kind) => self.picker_placements.get(&kind).copied(),
        }
    }

    pub(in crate::app) fn place_target(
        &mut self,
        target: NativeSurfaceTarget,
        placement: WindowPlacement,
    ) -> Result<NativeSurfaceIds, WindowError> {
        placement.validate()?;
        let ids = match target {
            NativeSurfaceTarget::Explorer => {
                let ids = self.explorer.ok_or(WindowError::UnknownWindow)?;
                if placement == WindowPlacement::Hidden {
                    if let Some(current) = self.explorer_placement
                        && current != WindowPlacement::Hidden
                    {
                        self.explorer_restore_placement = Some(current);
                    }
                } else {
                    self.explorer_restore_placement = Some(placement);
                }
                self.explorer_placement = Some(placement);
                ids
            }
            NativeSurfaceTarget::ExplorerView(view) => {
                let ids = self
                    .explorer_views
                    .get(&view)
                    .copied()
                    .ok_or(WindowError::UnknownWindow)?;
                if placement == WindowPlacement::Hidden {
                    if let Some(current) = self.explorer_view_placements.get(&view).copied()
                        && current != WindowPlacement::Hidden
                    {
                        self.explorer_view_restore_placements.insert(view, current);
                    }
                } else {
                    self.explorer_view_restore_placements
                        .insert(view, placement);
                }
                self.explorer_view_placements.insert(view, placement);
                ids
            }
            NativeSurfaceTarget::Terminal(id) => {
                return self.place_terminal(id, placement);
            }
            NativeSurfaceTarget::Dashboard => {
                let ids = self.dashboard.ok_or(WindowError::UnknownWindow)?;
                if placement != WindowPlacement::Hidden {
                    self.dashboard_restore_placement = Some(placement);
                }
                self.dashboard_placement = Some(placement);
                ids
            }
            NativeSurfaceTarget::Picker(kind) => {
                let ids = self
                    .pickers
                    .get(&kind)
                    .copied()
                    .ok_or(WindowError::UnknownWindow)?;
                if placement != WindowPlacement::Hidden {
                    self.picker_restore_placements.insert(kind, placement);
                }
                self.picker_placements.insert(kind, placement);
                ids
            }
        };
        self.signature = None;
        Ok(ids)
    }

    pub(in crate::app) fn show_target(
        &mut self,
        target: NativeSurfaceTarget,
        explorer_fallback: WindowPlacement,
        terminal_fallback: WindowPlacement,
    ) -> Result<NativeSurfaceIds, WindowError> {
        let placement = match target {
            NativeSurfaceTarget::Explorer => self
                .explorer_restore_placement
                .or_else(|| self.configured_placements.get("builtin.explorer").copied())
                .unwrap_or(explorer_fallback),
            NativeSurfaceTarget::ExplorerView(view) => self
                .explorer_view_restore_placements
                .get(&view)
                .copied()
                .or_else(|| self.configured_placements.get("builtin.explorer").copied())
                .unwrap_or(explorer_fallback),
            NativeSurfaceTarget::Terminal(id) => self
                .terminal_restore_placements
                .get(&id)
                .copied()
                .filter(|placement| *placement != WindowPlacement::Hidden)
                .or(self
                    .terminal_default_placement
                    .filter(|placement| *placement != WindowPlacement::Hidden))
                .unwrap_or(terminal_fallback),
            NativeSurfaceTarget::Dashboard => self
                .dashboard_restore_placement
                .unwrap_or(WindowPlacement::Fullscreen),
            NativeSurfaceTarget::Picker(kind) => self
                .picker_restore_placements
                .get(&kind)
                .copied()
                .unwrap_or(WindowPlacement::Floating(crate::windows::WindowRect {
                    x: 1,
                    y: 1,
                    width: 60,
                    height: 20,
                })),
        };
        self.place_target(target, placement)
    }

    pub(in crate::app) fn close_target(
        &mut self,
        graph: &mut WindowGraph,
        target: NativeSurfaceTarget,
    ) -> Result<NativeSurfaceIds, WindowError> {
        let ids = match target {
            NativeSurfaceTarget::Explorer => self.explorer.ok_or(WindowError::UnknownWindow)?,
            NativeSurfaceTarget::ExplorerView(view) => self
                .explorer_views
                .get(&view)
                .copied()
                .ok_or(WindowError::UnknownWindow)?,
            NativeSurfaceTarget::Terminal(id) => self
                .terminals
                .get(&id)
                .copied()
                .ok_or(WindowError::UnknownWindow)?,
            NativeSurfaceTarget::Dashboard => self.dashboard.ok_or(WindowError::UnknownWindow)?,
            NativeSurfaceTarget::Picker(kind) => self
                .pickers
                .get(&kind)
                .copied()
                .ok_or(WindowError::UnknownWindow)?,
        };
        graph.close_window(ids.window)?;
        graph.remove_view(ids.view)?;
        if graph
            .content_snapshots()
            .into_iter()
            .find(|content| content.id == ids.content)
            .is_some_and(|content| content.view_count == 0)
        {
            graph.remove_content(ids.content)?;
        }
        match target {
            NativeSurfaceTarget::Explorer => {
                self.explorer = None;
                self.explorer_closed = true;
            }
            NativeSurfaceTarget::ExplorerView(view) => {
                self.explorer_views.remove(&view);
                self.explorer_view_placements.remove(&view);
                self.explorer_view_restore_placements.remove(&view);
            }
            NativeSurfaceTarget::Terminal(id) => {
                self.terminals.remove(&id);
                self.terminal_placements.remove(&id);
                self.terminal_restore_placements.remove(&id);
            }
            NativeSurfaceTarget::Dashboard => {
                self.dashboard = None;
                self.dashboard_placement = None;
                self.dashboard_restore_placement = None;
            }
            NativeSurfaceTarget::Picker(kind) => {
                self.pickers.remove(&kind);
                self.picker_placements.remove(&kind);
                self.picker_restore_placements.remove(&kind);
            }
        }
        self.signature = None;
        Ok(ids)
    }

    pub(in crate::app) fn clone_explorer(
        &mut self,
        graph: &mut WindowGraph,
        source: NativeSurfaceTarget,
        view: u64,
        axis: crate::windows::TileAxis,
    ) -> Result<NativeSurfaceIds, WindowError> {
        if view <= 1 || self.explorer_views.contains_key(&view) {
            return Err(WindowError::UnknownView);
        }
        let source = self
            .ids_for_target(source)
            .ok_or(WindowError::UnknownWindow)?;
        graph.require_clone_view(source.content)?;
        let source_window = graph
            .window(source.window)
            .ok_or(WindowError::UnknownWindow)?;
        let source_tab = source_window.tab;
        let placement = match source_window.placement {
            WindowPlacement::Floating(mut rect) => {
                match axis {
                    crate::windows::TileAxis::Horizontal => rect.x = rect.x.saturating_add(2),
                    crate::windows::TileAxis::Vertical => rect.y = rect.y.saturating_add(1),
                }
                WindowPlacement::Floating(rect)
            }
            _ => WindowPlacement::Floating(WindowRect {
                x: 2,
                y: 2,
                width: 40,
                height: 20,
            }),
        };
        let owner = "builtin.surfaces";
        let instance_id = 1;
        let universal_view = graph.create_view(source.content, owner, instance_id)?;
        let window = graph.open_window(
            source_tab,
            universal_view,
            placement,
            true,
            owner,
            instance_id,
        )?;
        let ids = NativeSurfaceIds {
            content: source.content,
            view: universal_view,
            window,
        };
        self.explorer_views.insert(view, ids);
        self.explorer_view_placements.insert(view, placement);
        self.signature = None;
        Ok(ids)
    }

    pub(in crate::app) fn allow_explorer(&mut self) {
        self.explorer_closed = false;
        self.signature = None;
    }

    pub(in crate::app) fn set_configured_placements(
        &mut self,
        placements: BTreeMap<String, WindowPlacement>,
    ) -> Result<(), WindowError> {
        const SUPPORTED: &[&str] = &[
            "builtin.explorer",
            "builtin.terminal",
            "builtin.dashboard",
            "builtin.picker",
            "builtin.problems",
            "builtin.output",
            "builtin.logs",
            "builtin.debug",
            "builtin.theme",
        ];
        for (content_type, placement) in &placements {
            if !SUPPORTED.contains(&content_type.as_str()) {
                return Err(WindowError::InvalidContentType(content_type.clone()));
            }
            placement.validate()?;
            if matches!(
                placement,
                WindowPlacement::Tiled | WindowPlacement::Overlay(_)
            ) {
                return Err(WindowError::InvalidPlacement);
            }
        }
        if self.configured_placements != placements {
            let previous = std::mem::replace(&mut self.configured_placements, placements);
            if let Err(error) = self.validate_effective_semantic_defaults() {
                self.configured_placements = previous;
                return Err(error);
            }
            self.signature = None;
        }
        Ok(())
    }

    fn validate_effective_semantic_defaults(&self) -> Result<(), WindowError> {
        let fullscreen = [
            self.explorer_placement
                .or_else(|| self.configured_placements.get("builtin.explorer").copied()),
            self.terminal_default_placement
                .or_else(|| self.configured_placements.get("builtin.terminal").copied()),
            self.configured_placements.get("builtin.dashboard").copied(),
            self.configured_placements.get("builtin.picker").copied(),
            self.configured_placements.get("builtin.problems").copied(),
            self.configured_placements.get("builtin.output").copied(),
            self.configured_placements.get("builtin.logs").copied(),
            self.configured_placements.get("builtin.debug").copied(),
            self.configured_placements.get("builtin.theme").copied(),
        ]
        .into_iter()
        .flatten()
        .filter(|placement| *placement == WindowPlacement::Fullscreen)
        .count();
        if fullscreen > 1 {
            Err(WindowError::MultipleFullscreenWindows)
        } else {
            Ok(())
        }
    }

    pub(in crate::app) fn set_semantic_default(
        &mut self,
        content_type: &str,
        placement: WindowPlacement,
    ) -> Result<(), WindowError> {
        placement.validate()?;
        let previous_explorer = self.explorer_placement;
        let previous_explorer_restore = self.explorer_restore_placement;
        let previous_terminal = self.terminal_default_placement;
        match content_type {
            "builtin.explorer" => {
                self.explorer_placement = Some(placement);
                if placement != WindowPlacement::Hidden {
                    self.explorer_restore_placement = Some(placement);
                }
            }
            "builtin.terminal" => self.terminal_default_placement = Some(placement),
            value => return Err(WindowError::InvalidContentType(value.to_owned())),
        }
        if let Err(error) = self.validate_effective_semantic_defaults() {
            self.explorer_placement = previous_explorer;
            self.explorer_restore_placement = previous_explorer_restore;
            self.terminal_default_placement = previous_terminal;
            return Err(error);
        }
        self.signature = None;
        Ok(())
    }

    pub(in crate::app) fn semantic_defaults(
        &self,
        selected_terminal: Option<u64>,
    ) -> Vec<(&'static str, WindowPlacement)> {
        let mut rules = Vec::new();
        if let Some(placement) = self.explorer_placement {
            rules.push(("builtin.explorer", placement));
        }
        if let Some(placement) = selected_terminal
            .and_then(|id| self.terminal_placements.get(&id))
            .copied()
            .or(self.terminal_default_placement)
        {
            rules.push(("builtin.terminal", placement));
        }
        rules
    }

    /// Records where a terminal's window goes before the window exists; the next reconcile
    /// creates it there.
    pub(in crate::app) fn prefer_terminal_placement(
        &mut self,
        id: u64,
        placement: WindowPlacement,
    ) {
        self.terminal_placements.insert(id, placement);
        self.terminal_restore_placements.insert(id, placement);
        self.signature = None;
    }

    pub(in crate::app) fn place_terminal(
        &mut self,
        id: u64,
        placement: WindowPlacement,
    ) -> Result<NativeSurfaceIds, WindowError> {
        placement.validate()?;
        let ids = self
            .terminals
            .get(&id)
            .copied()
            .ok_or(WindowError::UnknownWindow)?;
        if placement == WindowPlacement::Hidden {
            if let Some(current) = self.terminal_placements.get(&id).copied()
                && current != WindowPlacement::Hidden
            {
                self.terminal_restore_placements.insert(id, current);
            }
        } else {
            self.terminal_restore_placements.insert(id, placement);
        }
        self.terminal_placements.insert(id, placement);
        self.signature = None;
        Ok(ids)
    }

    #[cfg(test)]
    pub(in crate::app) fn reset(&mut self, graph: &mut WindowGraph) -> Result<(), WindowError> {
        graph.remove_owner_instance("builtin.surfaces", 1)?;
        let configured_placements = std::mem::take(&mut self.configured_placements);
        let explorer_placement = self.explorer_placement;
        let explorer_restore_placement = self.explorer_restore_placement;
        let explorer_view_placements = std::mem::take(&mut self.explorer_view_placements);
        let explorer_view_restore_placements =
            std::mem::take(&mut self.explorer_view_restore_placements);
        let explorer_closed = self.explorer_closed;
        let terminal_default_placement = self.terminal_default_placement;
        *self = Self {
            configured_placements,
            explorer_placement,
            explorer_restore_placement,
            explorer_view_placements,
            explorer_view_restore_placements,
            explorer_closed,
            terminal_default_placement,
            ..Self::default()
        };
        Ok(())
    }
}

#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;
