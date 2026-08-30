use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use eframe::egui::{self, Color32, FontId, RichText, Stroke, TextStyle, Vec2};

use crate::{
    model::ScaleMode,
    session::{ResizeStatus, SessionId, SessionPhase},
    ssh::VmStatus,
};

use super::{
    actions::{selected_tab, DispatchOutcome, UiAction},
    state::{AppState, ClipboardStatus, FramebufferUploadKind, QueueStatus, SetupState},
};

const CANVAS_BLACK: Color32 = Color32::from_rgb(0x0d, 0x11, 0x17);
const CHASSIS_GRAPHITE: Color32 = Color32::from_rgb(0x17, 0x1c, 0x23);
const PANEL_STEEL: Color32 = Color32::from_rgb(0x24, 0x2b, 0x35);
const PRIMARY_TEXT: Color32 = Color32::from_rgb(0xe6, 0xed, 0xf3);
const MUTED_TELEMETRY: Color32 = Color32::from_rgb(0x8b, 0x98, 0xa7);
const PROXMOX_ORANGE: Color32 = Color32::from_rgb(0xe5, 0x70, 0x00);
const READY_CYAN: Color32 = Color32::from_rgb(0x4f, 0xb7, 0xc5);

struct TextureEntry {
    handle: egui::TextureHandle,
    width: u16,
    height: u16,
}

pub(crate) struct ViewResources {
    textures: HashMap<SessionId, TextureEntry>,
    input: InputOwnership,
    app_focused: bool,
}

impl Default for ViewResources {
    fn default() -> Self {
        Self {
            textures: HashMap::new(),
            input: InputOwnership::default(),
            app_focused: true,
        }
    }
}

impl ViewResources {
    pub(crate) fn request_owner_cleanup(&mut self) {
        self.input.schedule_cleanup();
    }

    pub(crate) fn owner_cleanup_pending(&self) -> bool {
        self.input.cleanup != CleanupDispatchState::None
    }

    #[cfg(test)]
    pub(crate) fn owner_cleanup_action(&self) -> Option<UiAction> {
        self.input.pending_cleanup_action()
    }

    pub(crate) fn acknowledge_owner_cleanup(&mut self, outcome: DispatchOutcome) {
        self.input.acknowledge_cleanup(outcome);
    }

    pub(crate) fn begin_pointer_dispatch(
        &mut self,
        session_id: SessionId,
        buttons: u8,
        position: (u16, u16),
    ) {
        self.input
            .begin_pointer_dispatch(session_id, buttons, position);
    }

    pub(crate) fn acknowledge_pointer_dispatch(
        &mut self,
        session_id: SessionId,
        buttons: u8,
        outcome: DispatchOutcome,
    ) {
        self.input
            .acknowledge_pointer_dispatch(session_id, buttons, outcome);
    }

    pub(crate) fn manager_completed(&mut self) {
        self.input.manager_completed();
    }

    #[cfg(test)]
    pub(crate) fn set_test_owner(
        &mut self,
        session_id: SessionId,
        modifier_bits: u8,
        pointer_buttons: u8,
        pointer_position: Option<(u16, u16)>,
    ) {
        self.input = InputOwnership::for_session(session_id);
        self.input
            .set_test_state(modifier_bits, pointer_buttons, pointer_position);
    }
}

#[derive(Default)]
struct InputOwnership {
    session_id: Option<SessionId>,
    keyboard_focused: bool,
    modifier_bits: u8,
    pointer_buttons: u8,
    pointer_position: Option<(u16, u16)>,
    pointer_release_required: bool,
    cleanup: CleanupDispatchState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CleanupDispatchState {
    #[default]
    None,
    Pending,
    Disconnected,
}

impl InputOwnership {
    #[cfg(test)]
    fn for_session(session_id: SessionId) -> Self {
        Self {
            session_id: Some(session_id),
            keyboard_focused: true,
            ..Self::default()
        }
    }

    fn claim(&mut self, session_id: SessionId, keyboard_focused: bool) {
        if self.cleanup != CleanupDispatchState::None {
            return;
        }
        if self.session_id.is_none() {
            self.session_id = Some(session_id);
        }
        if self.session_id == Some(session_id) {
            self.keyboard_focused |= keyboard_focused;
        }
    }

    fn schedule_cleanup(&mut self) {
        if self.session_id.is_some() && self.cleanup == CleanupDispatchState::None {
            self.cleanup = CleanupDispatchState::Pending;
        }
    }

    fn release_if_not(&mut self, session_id: Option<SessionId>) {
        if self.session_id.is_some() && self.session_id != session_id {
            self.schedule_cleanup();
        }
    }

    fn validate_against_state(&mut self, state: &AppState) {
        let Some(session_id) = self.session_id else {
            return;
        };
        let writable = state.tabs().iter().any(|tab| {
            tab.snapshot.session_id == session_id
                && tab.snapshot.phase == SessionPhase::Ready
                && !tab.snapshot.view_only
                && tab.last_error.is_none()
        });
        if !writable {
            self.schedule_cleanup();
        }
    }

    fn pending_cleanup_action(&self) -> Option<UiAction> {
        let session_id = self
            .session_id
            .filter(|_| self.cleanup == CleanupDispatchState::Pending)?;
        Some(UiAction::ReleaseOwnedInput {
            session_id,
            pointer_position: (self.pointer_buttons != 0 || self.pointer_release_required)
                .then_some(self.pointer_position)
                .flatten(),
        })
    }

    fn begin_pointer_dispatch(&mut self, session_id: SessionId, buttons: u8, position: (u16, u16)) {
        if self.session_id != Some(session_id) {
            return;
        }
        self.pointer_position = Some(position);
        if buttons != 0 {
            self.pointer_release_required = true;
        }
    }

    fn acknowledge_pointer_dispatch(
        &mut self,
        session_id: SessionId,
        buttons: u8,
        outcome: DispatchOutcome,
    ) {
        if self.session_id != Some(session_id) || !self.pointer_release_required {
            return;
        }
        if buttons != 0 {
            if outcome == DispatchOutcome::Disconnected {
                self.schedule_cleanup();
                self.acknowledge_cleanup(outcome);
            }
            return;
        }
        match outcome {
            DispatchOutcome::Sent => {
                self.pointer_release_required = false;
                if self.pointer_buttons == 0 {
                    self.pointer_position = None;
                }
            }
            DispatchOutcome::Disconnected => {
                self.schedule_cleanup();
                self.acknowledge_cleanup(outcome);
            }
            _ => self.schedule_cleanup(),
        }
    }

    fn acknowledge_cleanup(&mut self, outcome: DispatchOutcome) {
        if self.cleanup != CleanupDispatchState::Pending {
            return;
        }
        match outcome {
            DispatchOutcome::Sent => *self = Self::default(),
            DispatchOutcome::Disconnected => {
                self.cleanup = CleanupDispatchState::Disconnected;
            }
            _ => {}
        }
    }

    fn manager_completed(&mut self) {
        *self = Self::default();
    }

    fn blocks_fresh_input(&self) -> bool {
        self.cleanup != CleanupDispatchState::None
    }

    fn release_pointer(&mut self, actions: &mut Vec<UiAction>) {
        if self.pointer_buttons != 0 {
            if let (Some(session_id), Some((x, y))) = (self.session_id, self.pointer_position) {
                actions.push(UiAction::Pointer {
                    session_id,
                    buttons: 0,
                    x,
                    y,
                });
            }
        }
        self.pointer_buttons = 0;
        if !self.pointer_release_required {
            self.pointer_position = None;
        }
    }

    #[cfg(test)]
    fn set_test_state(
        &mut self,
        modifier_bits: u8,
        pointer_buttons: u8,
        pointer_position: Option<(u16, u16)>,
    ) {
        self.modifier_bits = modifier_bits;
        self.pointer_buttons = pointer_buttons;
        self.pointer_position = pointer_position;
        self.pointer_release_required = pointer_buttons != 0;
    }

    #[cfg(test)]
    fn is_clear(&self) -> bool {
        self.session_id.is_none()
            && !self.keyboard_focused
            && self.modifier_bits == 0
            && self.pointer_buttons == 0
            && self.pointer_position.is_none()
            && !self.pointer_release_required
            && self.cleanup == CleanupDispatchState::None
    }

    #[cfg(test)]
    fn pointer_buttons(&self) -> u8 {
        self.pointer_buttons
    }
}

pub(crate) fn configure_visuals(ctx: &egui::Context) {
    ctx.style_mut(|style| {
        style.visuals.dark_mode = true;
        style.visuals.panel_fill = CHASSIS_GRAPHITE;
        style.visuals.window_fill = CHASSIS_GRAPHITE;
        style.visuals.extreme_bg_color = CANVAS_BLACK;
        style.visuals.faint_bg_color = PANEL_STEEL;
        style.visuals.selection.bg_fill = PROXMOX_ORANGE;
        style.visuals.selection.stroke = Stroke::new(1.0, PRIMARY_TEXT);
        style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, PRIMARY_TEXT);
        style.visuals.widgets.hovered.bg_fill = PANEL_STEEL;
        style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, PRIMARY_TEXT);
        style.visuals.widgets.active.bg_fill = PROXMOX_ORANGE;
        style.visuals.widgets.active.fg_stroke = Stroke::new(1.0, PRIMARY_TEXT);
        style.spacing.item_spacing = egui::vec2(6.0, 6.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(18.0));
        style
            .text_styles
            .insert(TextStyle::Monospace, FontId::monospace(12.0));
    });
}

pub(crate) fn render(
    ctx: &egui::Context,
    state: &mut AppState,
    resources: &mut ViewResources,
) -> Vec<UiAction> {
    resources.input.validate_against_state(state);
    resources.textures.retain(|session_id, _| {
        state
            .tabs()
            .iter()
            .any(|tab| tab.snapshot.session_id == *session_id)
    });
    let mut actions = Vec::new();
    resources.input.release_if_not(state.selected_session_id());
    render_menu_bar(ctx, state, &mut actions);

    if state.setup_state() != &SetupState::Configured {
        render_setup(ctx, state.setup_state());
        prepare_actions(&resources.input, &mut actions);
        return actions;
    }

    render_status_rail(ctx, state);
    render_sidebar(ctx, state, &mut actions);
    render_workspace(ctx, state, resources, &mut actions);
    render_diagnostics(ctx, state, &mut actions);

    let focused = ctx.input(|input| input.focused);
    if resources.app_focused && !focused {
        resources.input.schedule_cleanup();
    }
    resources.app_focused = focused;
    release_owner_for_control_actions(&mut resources.input, &actions);
    prepare_actions(&resources.input, &mut actions);
    actions
}

fn release_owner_for_control_actions(ownership: &mut InputOwnership, actions: &[UiAction]) {
    if actions.iter().any(|action| {
        !matches!(
            action,
            UiAction::ViewportChanged { .. }
                | UiAction::Key { .. }
                | UiAction::Pointer { .. }
                | UiAction::ReleaseOwnedInput { .. }
        )
    }) {
        ownership.schedule_cleanup();
    }
}

fn prepare_actions(ownership: &InputOwnership, actions: &mut Vec<UiAction>) {
    if let Some(cleanup) = ownership.pending_cleanup_action() {
        actions.retain(|action| {
            !matches!(
                action,
                UiAction::Key { .. }
                    | UiAction::Pointer { .. }
                    | UiAction::ReleaseOwnedInput { .. }
            )
        });
        actions.insert(0, cleanup);
    } else if ownership.blocks_fresh_input() {
        actions.clear();
    }
}

fn render_menu_bar(ctx: &egui::Context, state: &AppState, actions: &mut Vec<UiAction>) {
    let availability = state.action_availability();
    let selected = state.selected_session();
    egui::TopBottomPanel::top("rustedoutclient-menu")
        .exact_height(28.0)
        .frame(
            egui::Frame::new()
                .fill(CHASSIS_GRAPHITE)
                .inner_margin(egui::Margin::symmetric(8, 2))
                .stroke(Stroke::new(1.0, PANEL_STEEL)),
        )
        .show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("RustedOutClient", |ui| {
                    ui.label(RichText::new("Native Proxmox console").color(MUTED_TELEMETRY));
                });
                ui.menu_button("Session", |ui| {
                    menu_action(ui, "Open", availability.open, UiAction::Open, actions);
                    menu_action(
                        ui,
                        "Reconnect",
                        availability.reconnect,
                        UiAction::Reconnect,
                        actions,
                    );
                    menu_action(ui, "Close", availability.close, UiAction::Close, actions);
                    menu_action(
                        ui,
                        "Open in TigerVNC",
                        availability.open_in_tigervnc,
                        UiAction::OpenInTigerVnc,
                        actions,
                    );
                    ui.separator();
                    menu_action(
                        ui,
                        "Ctrl+Alt+Delete",
                        availability.ctrl_alt_delete,
                        UiAction::CtrlAltDelete,
                        actions,
                    );
                    menu_action(
                        ui,
                        "Release All Keys",
                        availability.release_all_keys,
                        UiAction::ReleaseAllKeys,
                        actions,
                    );
                    let view_only = selected.is_some_and(|tab| tab.snapshot.view_only);
                    menu_check(
                        ui,
                        "View Only",
                        view_only,
                        availability.view_only,
                        UiAction::SetViewOnly(!view_only),
                        actions,
                    );
                    menu_action(
                        ui,
                        "Fit to Window",
                        availability.fit_to_window,
                        UiAction::FitToWindow,
                        actions,
                    );
                    menu_action(
                        ui,
                        "1:1",
                        availability.one_to_one,
                        UiAction::OneToOne,
                        actions,
                    );
                    menu_action(
                        ui,
                        "Fullscreen",
                        availability.fullscreen,
                        UiAction::Fullscreen,
                        actions,
                    );
                    ui.separator();
                    menu_action(
                        ui,
                        "Send Clipboard",
                        availability.send_clipboard,
                        UiAction::SendClipboard,
                        actions,
                    );
                    menu_action(
                        ui,
                        "Receive Clipboard",
                        availability.receive_clipboard,
                        UiAction::ReceiveClipboard,
                        actions,
                    );
                    menu_action(
                        ui,
                        "Diagnostics",
                        availability.diagnostics,
                        UiAction::Diagnostics,
                        actions,
                    );
                });
                ui.menu_button("View", |ui| {
                    let dynamic =
                        selected.is_some_and(|tab| tab.snapshot.dynamic_resolution_enabled);
                    menu_check(
                        ui,
                        "Dynamic Resolution",
                        dynamic,
                        availability.dynamic_resolution,
                        UiAction::SetDynamicResolution(!dynamic),
                        actions,
                    );
                    menu_action(
                        ui,
                        "Retry Dynamic Resolution",
                        availability.retry_dynamic_resolution,
                        UiAction::RetryDynamicResolution,
                        actions,
                    );
                });
                ui.menu_button("Help", |ui| {
                    ui.label("Select a running VM, then Open.");
                    ui.label(
                        RichText::new("TigerVNC fallback arrives in Task 12.")
                            .color(MUTED_TELEMETRY),
                    );
                });
            });
        });
}

fn menu_action(
    ui: &mut egui::Ui,
    label: &str,
    enabled: bool,
    action: UiAction,
    actions: &mut Vec<UiAction>,
) {
    if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
        actions.push(action);
        ui.close_menu();
    }
}

fn menu_check(
    ui: &mut egui::Ui,
    label: &str,
    checked: bool,
    enabled: bool,
    action: UiAction,
    actions: &mut Vec<UiAction>,
) {
    if ui
        .add_enabled(enabled, egui::Button::new(label).selected(checked))
        .clicked()
    {
        actions.push(action);
        ui.close_menu();
    }
}

fn render_setup(ctx: &egui::Context, setup: &SetupState) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(CANVAS_BLACK))
        .show(ctx, |ui| {
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    ui.heading(RichText::new("RustedOutClient").color(PRIMARY_TEXT));
                    ui.add_space(8.0);
                    match setup {
                        SetupState::MissingConfiguration => {
                            ui.label("Configuration is required before a console can open.");
                            ui.label(
                                RichText::new(
                                    "Create the private RustedOutClient config, then restart the app.",
                                )
                                .color(MUTED_TELEMETRY),
                            );
                        }
                        SetupState::InvalidConfiguration => {
                            ui.label("The private configuration could not be loaded.");
                            ui.label(
                                RichText::new(
                                    "Check its schema and 0700/0600 permissions, then restart the app.",
                                )
                                .color(MUTED_TELEMETRY),
                            );
                        }
                        SetupState::Configured => {}
                    }
                });
            });
        });
}

fn render_sidebar(ctx: &egui::Context, state: &mut AppState, actions: &mut Vec<UiAction>) {
    egui::SidePanel::left("rack-index")
        .exact_width(264.0)
        .resizable(true)
        .width_range(248.0..=280.0)
        .frame(
            egui::Frame::new()
                .fill(CHASSIS_GRAPHITE)
                .inner_margin(egui::Margin::same(10))
                .stroke(Stroke::new(1.0, PANEL_STEEL)),
        )
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("VM INDEX").monospace().color(MUTED_TELEMETRY));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Refresh").clicked() {
                        actions.push(UiAction::RefreshInventory);
                    }
                });
            });
            let mut search = state.search().to_owned();
            if ui
                .add(
                    egui::TextEdit::singleline(&mut search)
                        .hint_text("Search alias, name, or VMID")
                        .desired_width(f32::INFINITY),
                )
                .changed()
            {
                state.set_search(search);
            }
            ui.add_space(4.0);
            let rows = state.inventory_rows();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for row in rows {
                    let selected = state.selected_inventory() == Some(row.vmid);
                    let state_label = match row.status {
                        VmStatus::Running => "RUNNING",
                        VmStatus::Stopped => "STOPPED",
                    };
                    let title = row.alias.as_deref().unwrap_or(&row.name);
                    let marker = if row.favorite { "◆" } else { "·" };
                    let freshness = if row.stale { "STALE" } else { "LIVE" };
                    let detail = if row.alias.is_some() {
                        format!("{}  {freshness}/{state_label}", row.name)
                    } else {
                        format!("{freshness}/{state_label}")
                    };
                    let response = ui.add(
                        egui::Button::new(
                            egui::RichText::new(format!(
                                "{marker} {:>5}  {title}\n       {detail}",
                                row.vmid
                            ))
                            .monospace()
                            .color(
                                if row.status == VmStatus::Running {
                                    PRIMARY_TEXT
                                } else {
                                    MUTED_TELEMETRY
                                },
                            ),
                        )
                        .selected(selected)
                        .wrap(),
                    );
                    if response.clicked() {
                        state.select_inventory(Some(row.vmid));
                    }
                    if response.double_clicked() && row.can_open() {
                        state.select_inventory(Some(row.vmid));
                        actions.push(UiAction::Open);
                    }
                }
            });
            ui.separator();
            match state.inventory_age_source() {
                Some((observed, true)) => {
                    ui.label(
                        RichText::new(format!("STALE · observed {observed}"))
                            .monospace()
                            .color(PROXMOX_ORANGE),
                    );
                }
                Some((observed, false)) => {
                    ui.label(
                        RichText::new(format!("LIVE · observed {observed}"))
                            .monospace()
                            .color(READY_CYAN),
                    );
                }
                None => {
                    ui.label(
                        RichText::new("WARMING INVENTORY")
                            .monospace()
                            .color(MUTED_TELEMETRY),
                    );
                }
            }
        });
}

fn render_workspace(
    ctx: &egui::Context,
    state: &mut AppState,
    resources: &mut ViewResources,
    actions: &mut Vec<UiAction>,
) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(CANVAS_BLACK)
                .inner_margin(egui::Margin::same(8)),
        )
        .show(ctx, |ui| {
            render_tabs(ui, state);
            render_toolbar(ui, state, actions);
            ui.add_space(4.0);
            render_instrument_bay(ui, state, resources, actions);
        });
}

fn render_tabs(ui: &mut egui::Ui, state: &mut AppState) {
    let tabs = state
        .tabs()
        .iter()
        .map(|tab| {
            (
                tab.snapshot.session_id,
                tab.snapshot.vmid,
                tab.snapshot.phase,
            )
        })
        .collect::<Vec<_>>();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("SESSION BUS")
                .monospace()
                .color(MUTED_TELEMETRY),
        );
        for (session_id, vmid, phase) in tabs {
            let selected = state.selected_session_id() == Some(session_id);
            let label = format!("VM {vmid} · {phase:?}");
            if ui
                .add(egui::Button::new(RichText::new(label).monospace()).selected(selected))
                .clicked()
            {
                state.select_session(Some(session_id));
            }
        }
    });
}

fn render_toolbar(ui: &mut egui::Ui, state: &AppState, actions: &mut Vec<UiAction>) {
    let availability = state.action_availability();
    let selected = state.selected_session();
    ui.horizontal_wrapped(|ui| {
        toolbar_action(
            ui,
            "Reconnect",
            availability.reconnect,
            UiAction::Reconnect,
            actions,
        );
        toolbar_action(
            ui,
            "CAD",
            availability.ctrl_alt_delete,
            UiAction::CtrlAltDelete,
            actions,
        );
        toolbar_action(
            ui,
            "Release Keys",
            availability.release_all_keys,
            UiAction::ReleaseAllKeys,
            actions,
        );
        let view_only = selected.is_some_and(|tab| tab.snapshot.view_only);
        toolbar_check(
            ui,
            "View Only",
            view_only,
            availability.view_only,
            UiAction::SetViewOnly(!view_only),
            actions,
        );
        let dynamic = selected.is_some_and(|tab| tab.snapshot.dynamic_resolution_enabled);
        toolbar_check(
            ui,
            "Dynamic Resolution",
            dynamic,
            availability.dynamic_resolution,
            UiAction::SetDynamicResolution(!dynamic),
            actions,
        );
        let scale = selected.map(|tab| tab.scale_mode);
        toolbar_check(
            ui,
            "Fit",
            scale == Some(ScaleMode::Fit),
            availability.fit_to_window,
            UiAction::FitToWindow,
            actions,
        );
        toolbar_check(
            ui,
            "1:1",
            scale == Some(ScaleMode::OneToOne),
            availability.one_to_one,
            UiAction::OneToOne,
            actions,
        );
        toolbar_action(
            ui,
            "Fullscreen",
            availability.fullscreen,
            UiAction::Fullscreen,
            actions,
        );
        toolbar_action(
            ui,
            "TigerVNC",
            availability.open_in_tigervnc,
            UiAction::OpenInTigerVnc,
            actions,
        );
        if availability.retry_dynamic_resolution {
            toolbar_action(
                ui,
                "Retry resize",
                true,
                UiAction::RetryDynamicResolution,
                actions,
            );
        }
    });
}

fn toolbar_action(
    ui: &mut egui::Ui,
    label: &str,
    enabled: bool,
    action: UiAction,
    actions: &mut Vec<UiAction>,
) {
    if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
        actions.push(action);
    }
}

fn toolbar_check(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    enabled: bool,
    action: UiAction,
    actions: &mut Vec<UiAction>,
) {
    if ui
        .add_enabled(enabled, egui::Button::new(label).selected(selected))
        .clicked()
    {
        actions.push(action);
    }
}

fn render_instrument_bay(
    ui: &mut egui::Ui,
    state: &mut AppState,
    resources: &mut ViewResources,
    actions: &mut Vec<UiAction>,
) {
    let (bay_rect, response) =
        ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
    if response.clicked() {
        response.request_focus();
    }
    let focus_stroke = if response.has_focus() {
        Stroke::new(1.5, READY_CYAN)
    } else {
        Stroke::new(1.0, PANEL_STEEL)
    };
    ui.painter().rect_filled(bay_rect, 2.0, PANEL_STEEL);
    ui.painter()
        .rect_stroke(bay_rect, 2.0, focus_stroke, egui::StrokeKind::Inside);
    paint_corner_brackets(ui.painter(), bay_rect);
    let inner = bay_rect.shrink(14.0);

    let Some(tab) = selected_tab(state) else {
        ui.painter().text(
            inner.center(),
            egui::Align2::CENTER_CENTER,
            "SELECT A RUNNING VM AND OPEN A NATIVE SESSION",
            FontId::monospace(12.0),
            MUTED_TELEMETRY,
        );
        return;
    };
    let session_id = tab.snapshot.session_id;
    resources.input.release_if_not(Some(session_id));
    if response.has_focus() {
        resources.input.claim(session_id, true);
    } else if resources.input.session_id == Some(session_id) && resources.input.keyboard_focused {
        resources.input.schedule_cleanup();
    }
    let backing_width = (inner.width().max(0.0) * ui.ctx().pixels_per_point()) as u32;
    let backing_height = (inner.height().max(0.0) * ui.ctx().pixels_per_point()) as u32;
    if tab
        .viewport
        .map(|viewport| (viewport.width, viewport.height))
        != Some((backing_width, backing_height))
    {
        actions.push(UiAction::ViewportChanged {
            backing_width,
            backing_height,
        });
    }

    let Some(framebuffer) = tab.framebuffer() else {
        ui.painter().text(
            inner.center(),
            egui::Align2::CENTER_CENTER,
            format!(
                "{:?} · waiting for first non-empty frame",
                tab.snapshot.phase
            ),
            FontId::monospace(12.0),
            if tab.snapshot.phase == SessionPhase::Ready {
                READY_CYAN
            } else {
                MUTED_TELEMETRY
            },
        );
        return;
    };
    let framebuffer_size = (framebuffer.width(), framebuffer.height());
    let scale_mode = tab.scale_mode;
    let Some(texture) = texture_for(ui.ctx(), resources, state, session_id) else {
        return;
    };
    let image_size = display_size(
        framebuffer_size,
        inner.size(),
        scale_mode,
        ui.ctx().pixels_per_point(),
    );
    let image_rect = egui::Rect::from_center_size(inner.center(), image_size);
    paint_framebuffer(ui.painter(), texture.id(), image_rect, inner);
    let events = ui.ctx().input(|input| input.events.clone());
    if response.has_focus() && resources.input.session_id == Some(session_id) {
        let modifiers = ui.ctx().input(|input| input.modifiers);
        collect_keyboard_events(
            session_id,
            &events,
            modifiers,
            &mut resources.input,
            actions,
        );
    }
    let interactive_rect = visible_console_rect(image_rect, inner, ui.clip_rect());
    collect_pointer_events(
        session_id,
        &events,
        image_rect,
        interactive_rect,
        framebuffer_size,
        &mut resources.input,
        actions,
    );
}

fn texture_for(
    ctx: &egui::Context,
    resources: &mut ViewResources,
    state: &mut AppState,
    session_id: SessionId,
) -> Option<egui::TextureHandle> {
    let framebuffer = state.framebuffer(session_id)?;
    let width = framebuffer.width();
    let height = framebuffer.height();
    let force_full = resources
        .textures
        .get(&session_id)
        .is_none_or(|entry| entry.width != width || entry.height != height);
    let upload = state.framebuffer_upload_plan(session_id, force_full).ok()?;

    if force_full {
        let upload = upload?;
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [usize::from(upload.width()), usize::from(upload.height())],
            upload.rgba(),
        );
        resources.textures.insert(
            session_id,
            TextureEntry {
                handle: ctx.load_texture(
                    format!("session-{session_id:?}"),
                    image,
                    egui::TextureOptions::NEAREST,
                ),
                width,
                height,
            },
        );
        state
            .acknowledge_framebuffer_upload(session_id, upload.revision())
            .ok()?;
    } else if let Some(upload) = upload {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [usize::from(upload.width()), usize::from(upload.height())],
            upload.rgba(),
        );
        let entry = resources.textures.get_mut(&session_id)?;
        match upload.kind() {
            FramebufferUploadKind::Full => {
                entry.handle.set(image, egui::TextureOptions::NEAREST);
            }
            FramebufferUploadKind::Partial => {
                entry.handle.set_partial(
                    [usize::from(upload.x()), usize::from(upload.y())],
                    image,
                    egui::TextureOptions::NEAREST,
                );
            }
        }
        state
            .acknowledge_framebuffer_upload(session_id, upload.revision())
            .ok()?;
    }
    Some(resources.textures.get(&session_id)?.handle.clone())
}

fn display_size(
    framebuffer_size: (u16, u16),
    available: Vec2,
    scale_mode: ScaleMode,
    pixels_per_point: f32,
) -> Vec2 {
    let (width, height) = framebuffer_size;
    let native = egui::vec2(
        f32::from(width) / pixels_per_point.max(1.0),
        f32::from(height) / pixels_per_point.max(1.0),
    );
    match scale_mode {
        ScaleMode::OneToOne => native,
        ScaleMode::Fit => {
            let scale = (available.x / native.x)
                .min(available.y / native.y)
                .max(0.0);
            native * scale
        }
    }
}

fn collect_keyboard_events(
    session_id: SessionId,
    events: &[egui::Event],
    current_modifiers: egui::Modifiers,
    ownership: &mut InputOwnership,
    actions: &mut Vec<UiAction>,
) {
    if ownership.blocks_fresh_input() {
        return;
    }
    for event in events {
        match event {
            egui::Event::Key {
                key,
                pressed,
                repeat: _,
                modifiers,
                physical_key: _,
            } => {
                sync_modifiers(session_id, *modifiers, ownership, actions);
                if let Some(keysym) = key_to_keysym(*key, modifiers.shift) {
                    actions.push(UiAction::Key {
                        session_id,
                        down: *pressed,
                        keysym,
                    });
                }
            }
            egui::Event::Text(text) => {
                for character in text.chars().filter(|character| !character.is_ascii()) {
                    let keysym = 0x0100_0000 | u32::from(character);
                    actions.push(UiAction::Key {
                        session_id,
                        down: true,
                        keysym,
                    });
                    actions.push(UiAction::Key {
                        session_id,
                        down: false,
                        keysym,
                    });
                }
            }
            _ => {}
        }
    }
    sync_modifiers(session_id, current_modifiers, ownership, actions);
}

fn sync_modifiers(
    session_id: SessionId,
    modifiers: egui::Modifiers,
    ownership: &mut InputOwnership,
    actions: &mut Vec<UiAction>,
) {
    let desired = [
        (modifiers.shift, 0xffe1_u32, 0b0001_u8),
        (modifiers.ctrl, 0xffe3_u32, 0b0010_u8),
        (modifiers.alt, 0xffe9_u32, 0b0100_u8),
        (modifiers.command, 0xffeb_u32, 0b1000_u8),
    ];
    for (enabled, keysym, bit) in desired {
        let was_enabled = ownership.modifier_bits & bit != 0;
        if enabled != was_enabled {
            actions.push(UiAction::Key {
                session_id,
                down: enabled,
                keysym,
            });
            if enabled {
                ownership.modifier_bits |= bit;
            } else {
                ownership.modifier_bits &= !bit;
            }
        }
    }
}

fn visible_console_rect(
    image_rect: egui::Rect,
    inner_rect: egui::Rect,
    clip_rect: egui::Rect,
) -> egui::Rect {
    image_rect.intersect(inner_rect).intersect(clip_rect)
}

fn framebuffer_paint_clip(inner_rect: egui::Rect, clip_rect: egui::Rect) -> egui::Rect {
    inner_rect.intersect(clip_rect)
}

fn paint_framebuffer(
    painter: &egui::Painter,
    texture_id: egui::TextureId,
    image_rect: egui::Rect,
    inner_rect: egui::Rect,
) {
    painter
        .with_clip_rect(framebuffer_paint_clip(inner_rect, painter.clip_rect()))
        .image(
            texture_id,
            image_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
}

fn collect_pointer_events(
    session_id: SessionId,
    events: &[egui::Event],
    image_rect: egui::Rect,
    interactive_rect: egui::Rect,
    framebuffer_size: (u16, u16),
    ownership: &mut InputOwnership,
    actions: &mut Vec<UiAction>,
) {
    if ownership.blocks_fresh_input() {
        return;
    }
    for event in events {
        match event {
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: _,
            } => {
                let mask = pointer_button_mask(*button);
                if *pressed {
                    if !interactive_rect.contains(*pos) {
                        continue;
                    }
                    ownership.claim(session_id, true);
                    if ownership.session_id != Some(session_id) {
                        continue;
                    }
                    ownership.pointer_buttons |= mask;
                } else if ownership.session_id == Some(session_id)
                    && ownership.pointer_buttons & mask != 0
                {
                    if !interactive_rect.contains(*pos) {
                        ownership.release_pointer(actions);
                        continue;
                    }
                    ownership.pointer_buttons &= !mask;
                } else {
                    continue;
                }
                let position = if interactive_rect.contains(*pos) {
                    pointer_coordinates(*pos, image_rect, framebuffer_size)
                } else {
                    ownership.pointer_position
                };
                if let Some((x, y)) = position {
                    ownership.pointer_position = Some((x, y));
                    if ownership.pointer_buttons == 0 {
                        actions.push(UiAction::Pointer {
                            session_id,
                            buttons: 0,
                            x,
                            y,
                        });
                    } else {
                        actions.push(UiAction::Pointer {
                            session_id,
                            buttons: ownership.pointer_buttons,
                            x,
                            y,
                        });
                    }
                }
            }
            egui::Event::PointerMoved(pos) if interactive_rect.contains(*pos) => {
                if let Some((x, y)) = pointer_coordinates(*pos, image_rect, framebuffer_size) {
                    if ownership.session_id == Some(session_id) {
                        ownership.pointer_position = Some((x, y));
                    }
                    actions.push(UiAction::Pointer {
                        session_id,
                        buttons: if ownership.session_id == Some(session_id) {
                            ownership.pointer_buttons
                        } else {
                            0
                        },
                        x,
                        y,
                    });
                }
            }
            egui::Event::PointerGone => {
                if ownership.session_id == Some(session_id) {
                    ownership.release_pointer(actions);
                }
            }
            _ => {}
        }
    }
}

fn pointer_button_mask(button: egui::PointerButton) -> u8 {
    match button {
        egui::PointerButton::Primary => 1,
        egui::PointerButton::Middle => 2,
        egui::PointerButton::Secondary => 4,
        egui::PointerButton::Extra1 => 8,
        egui::PointerButton::Extra2 => 16,
    }
}

fn pointer_coordinates(
    position: egui::Pos2,
    image_rect: egui::Rect,
    framebuffer_size: (u16, u16),
) -> Option<(u16, u16)> {
    if image_rect.width() <= 0.0 || image_rect.height() <= 0.0 {
        return None;
    }
    let relative_x = ((position.x - image_rect.left()) / image_rect.width()).clamp(0.0, 1.0);
    let relative_y = ((position.y - image_rect.top()) / image_rect.height()).clamp(0.0, 1.0);
    let (width, height) = framebuffer_size;
    let x = (relative_x * f32::from(width)).floor() as u16;
    let y = (relative_y * f32::from(height)).floor() as u16;
    Some((
        x.min(width.saturating_sub(1)),
        y.min(height.saturating_sub(1)),
    ))
}

fn key_to_keysym(key: egui::Key, shift: bool) -> Option<u32> {
    use egui::Key;
    let fixed = match key {
        Key::ArrowLeft => 0xff51,
        Key::ArrowUp => 0xff52,
        Key::ArrowRight => 0xff53,
        Key::ArrowDown => 0xff54,
        Key::Escape => 0xff1b,
        Key::Tab => 0xff09,
        Key::Backspace => 0xff08,
        Key::Enter => 0xff0d,
        Key::Insert => 0xff63,
        Key::Delete => 0xffff,
        Key::Home => 0xff50,
        Key::End => 0xff57,
        Key::PageUp => 0xff55,
        Key::PageDown => 0xff56,
        Key::Space => 0x20,
        Key::F1 => 0xffbe,
        Key::F2 => 0xffbf,
        Key::F3 => 0xffc0,
        Key::F4 => 0xffc1,
        Key::F5 => 0xffc2,
        Key::F6 => 0xffc3,
        Key::F7 => 0xffc4,
        Key::F8 => 0xffc5,
        Key::F9 => 0xffc6,
        Key::F10 => 0xffc7,
        Key::F11 => 0xffc8,
        Key::F12 => 0xffc9,
        _ => return character_keysym(key, shift),
    };
    Some(fixed)
}

fn character_keysym(key: egui::Key, shift: bool) -> Option<u32> {
    use egui::Key;
    let character = match key {
        Key::A => 'a',
        Key::B => 'b',
        Key::C => 'c',
        Key::D => 'd',
        Key::E => 'e',
        Key::F => 'f',
        Key::G => 'g',
        Key::H => 'h',
        Key::I => 'i',
        Key::J => 'j',
        Key::K => 'k',
        Key::L => 'l',
        Key::M => 'm',
        Key::N => 'n',
        Key::O => 'o',
        Key::P => 'p',
        Key::Q => 'q',
        Key::R => 'r',
        Key::S => 's',
        Key::T => 't',
        Key::U => 'u',
        Key::V => 'v',
        Key::W => 'w',
        Key::X => 'x',
        Key::Y => 'y',
        Key::Z => 'z',
        Key::Num0 => {
            if shift {
                ')'
            } else {
                '0'
            }
        }
        Key::Num1 => {
            if shift {
                '!'
            } else {
                '1'
            }
        }
        Key::Num2 => {
            if shift {
                '@'
            } else {
                '2'
            }
        }
        Key::Num3 => {
            if shift {
                '#'
            } else {
                '3'
            }
        }
        Key::Num4 => {
            if shift {
                '$'
            } else {
                '4'
            }
        }
        Key::Num5 => {
            if shift {
                '%'
            } else {
                '5'
            }
        }
        Key::Num6 => {
            if shift {
                '^'
            } else {
                '6'
            }
        }
        Key::Num7 => {
            if shift {
                '&'
            } else {
                '7'
            }
        }
        Key::Num8 => {
            if shift {
                '*'
            } else {
                '8'
            }
        }
        Key::Num9 => {
            if shift {
                '('
            } else {
                '9'
            }
        }
        Key::Colon => ':',
        Key::Comma => ',',
        Key::Backslash => '\\',
        Key::Slash => '/',
        Key::Pipe => '|',
        Key::Questionmark => '?',
        Key::Exclamationmark => '!',
        Key::OpenBracket => '[',
        Key::CloseBracket => ']',
        Key::OpenCurlyBracket => '{',
        Key::CloseCurlyBracket => '}',
        Key::Backtick => '`',
        Key::Minus => '-',
        Key::Period => '.',
        Key::Plus => '+',
        Key::Equals => '=',
        Key::Semicolon => ';',
        Key::Quote => '\'',
        _ => return None,
    };
    let character = if shift && character.is_ascii_lowercase() {
        character.to_ascii_uppercase()
    } else {
        character
    };
    Some(u32::from(character))
}

fn paint_corner_brackets(painter: &egui::Painter, rect: egui::Rect) {
    let length = 12.0;
    let stroke = Stroke::new(1.5, PROXMOX_ORANGE);
    for (corner, horizontal, vertical) in [
        (
            rect.left_top(),
            egui::vec2(length, 0.0),
            egui::vec2(0.0, length),
        ),
        (
            rect.right_top(),
            egui::vec2(-length, 0.0),
            egui::vec2(0.0, length),
        ),
        (
            rect.left_bottom(),
            egui::vec2(length, 0.0),
            egui::vec2(0.0, -length),
        ),
        (
            rect.right_bottom(),
            egui::vec2(-length, 0.0),
            egui::vec2(0.0, -length),
        ),
    ] {
        painter.line_segment([corner, corner + horizontal], stroke);
        painter.line_segment([corner, corner + vertical], stroke);
    }
}

fn render_status_rail(ctx: &egui::Context, state: &AppState) {
    egui::TopBottomPanel::bottom("telemetry-rail")
        .exact_height(26.0)
        .frame(
            egui::Frame::new()
                .fill(CHASSIS_GRAPHITE)
                .inner_margin(egui::Margin::symmetric(8, 4))
                .stroke(Stroke::new(1.0, PANEL_STEEL)),
        )
        .show(ctx, |ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal(|ui| {
                    status(ui, state.profile_name(), MUTED_TELEMETRY);
                    status(ui, state.node_name(), MUTED_TELEMETRY);
                    status(ui, &inventory_age_label(state), inventory_age_color(state));
                    if let Some(tab) = state.selected_session() {
                        status(ui, &format!("VM {}", tab.snapshot.vmid), PRIMARY_TEXT);
                        if let Some(name) = state.vm_name(tab.snapshot.vmid) {
                            status(ui, name, PRIMARY_TEXT);
                        }
                        status(
                            ui,
                            &format!("{:?}", tab.snapshot.phase),
                            if tab.snapshot.phase == SessionPhase::Ready {
                                READY_CYAN
                            } else {
                                MUTED_TELEMETRY
                            },
                        );
                        status(ui, &format!("{:?}", tab.scale_mode), MUTED_TELEMETRY);
                        if let Some(size) = tab.snapshot.guest_size {
                            status(
                                ui,
                                &format!("guest {}x{}", size.width, size.height),
                                PRIMARY_TEXT,
                            );
                        }
                        status(
                            ui,
                            &resize_label(tab.snapshot.resize_status),
                            resize_color(tab.snapshot.resize_status),
                        );
                        status(
                            ui,
                            if tab.snapshot.view_only {
                                "view only"
                            } else {
                                "writable"
                            },
                            MUTED_TELEMETRY,
                        );
                        status(ui, clipboard_label(tab.clipboard_status), MUTED_TELEMETRY);
                    } else {
                        status(ui, "no native session", MUTED_TELEMETRY);
                    }
                    match state.queue_status() {
                        QueueStatus::Ready => {}
                        QueueStatus::Busy => {
                            status(ui, "BUSY · command queue full", PROXMOX_ORANGE);
                        }
                        QueueStatus::Disconnected => {
                            status(ui, "command queue disconnected", PROXMOX_ORANGE);
                        }
                    }
                });
            });
        });
}

fn inventory_age_label(state: &AppState) -> String {
    let Some((observed, stale)) = state.inventory_age_source() else {
        return "inventory warming".to_owned();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64);
    let age_seconds = now.saturating_sub(observed) / 1_000;
    format!(
        "inventory {} · {}s",
        if stale { "stale" } else { "live" },
        age_seconds
    )
}

fn inventory_age_color(state: &AppState) -> Color32 {
    match state.inventory_age_source() {
        Some((_, false)) => READY_CYAN,
        Some((_, true)) => PROXMOX_ORANGE,
        None => MUTED_TELEMETRY,
    }
}

fn status(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.label(RichText::new(text).monospace().color(color));
    ui.label(RichText::new("│").color(PANEL_STEEL));
}

fn resize_label(status: ResizeStatus) -> String {
    match status {
        ResizeStatus::Disabled => "resize disabled".to_owned(),
        ResizeStatus::Waiting => "resize waiting".to_owned(),
        ResizeStatus::Requested(size) => format!("resize requested {}x{}", size.width, size.height),
        ResizeStatus::Pending(size) => format!("resize pending {}x{}", size.width, size.height),
        ResizeStatus::Applied(size) => format!("resize applied {}x{}", size.width, size.height),
        ResizeStatus::Rejected => "resize rejected".to_owned(),
        ResizeStatus::Unsupported => "resize unsupported".to_owned(),
        ResizeStatus::TimedOut => "resize timed out".to_owned(),
    }
}

fn resize_color(status: ResizeStatus) -> Color32 {
    match status {
        ResizeStatus::Applied(_) => READY_CYAN,
        ResizeStatus::Rejected | ResizeStatus::Unsupported | ResizeStatus::TimedOut => {
            PROXMOX_ORANGE
        }
        _ => MUTED_TELEMETRY,
    }
}

fn clipboard_label(status: ClipboardStatus) -> &'static str {
    match status {
        ClipboardStatus::Disabled => "clipboard off",
        ClipboardStatus::Ready => "clipboard explicit",
        ClipboardStatus::Sent => "clipboard sent",
        ClipboardStatus::Received => "clipboard received",
        ClipboardStatus::Unavailable => "clipboard unavailable",
    }
}

fn render_diagnostics(ctx: &egui::Context, state: &mut AppState, actions: &mut Vec<UiAction>) {
    if !state.diagnostics_open() {
        return;
    }
    let mut open = true;
    egui::Window::new("Diagnostics")
        .open(&mut open)
        .resizable(true)
        .show(ctx, |ui| {
            let mut diagnostics = state.diagnostics_summary();
            ui.add(
                egui::TextEdit::multiline(&mut diagnostics)
                    .font(TextStyle::Monospace)
                    .desired_rows(12)
                    .interactive(false),
            );
            if ui.button("Copy Diagnostics").clicked() {
                actions.push(UiAction::CopyDiagnostics);
            }
        });
    if !open {
        state.close_diagnostics();
    }
}

#[cfg(test)]
mod input_tests {
    use super::{
        collect_keyboard_events, collect_pointer_events, framebuffer_paint_clip, paint_framebuffer,
        prepare_actions, release_owner_for_control_actions, visible_console_rect, InputOwnership,
    };
    use crate::{
        app::{AppState, DispatchOutcome, UiAction},
        config::AppConfig,
        model::{NodeName, PveProfile, SshTarget, VmId},
        session::{
            AppEvent, PublicError, PublicErrorKind, ResizeStatus, SessionId, SessionPhase,
            SessionSnapshot,
        },
    };

    fn vmid(value: u32) -> VmId {
        VmId::new(value).unwrap()
    }

    fn snapshot(session_id: SessionId, phase: SessionPhase, view_only: bool) -> SessionSnapshot {
        SessionSnapshot {
            session_id,
            profile_name: "Synthetic lab".to_owned(),
            vmid: vmid(107),
            phase,
            view_only,
            clipboard_enabled: false,
            dynamic_resolution_enabled: true,
            guest_size: None,
            resize_status: ResizeStatus::Waiting,
        }
    }

    fn state_with(snapshots: impl IntoIterator<Item = SessionSnapshot>) -> AppState {
        let mut state = AppState::from_config(&AppConfig::new(PveProfile {
            name: "Synthetic lab".to_owned(),
            ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
            node: NodeName::parse("pve2").unwrap(),
        }));
        for snapshot in snapshots {
            state.apply(AppEvent::SessionChanged(snapshot)).unwrap();
        }
        state
    }

    fn modifier(field: &str, enabled: bool) -> egui::Modifiers {
        let mut modifiers = egui::Modifiers::default();
        match field {
            "shift" => modifiers.shift = enabled,
            "ctrl" => modifiers.ctrl = enabled,
            "alt" => modifiers.alt = enabled,
            "command" => modifiers.command = enabled,
            _ => unreachable!(),
        }
        modifiers
    }

    fn key_actions(actions: &[UiAction]) -> Vec<(SessionId, bool, u32)> {
        actions
            .iter()
            .filter_map(|action| match action {
                UiAction::Key {
                    session_id,
                    down,
                    keysym,
                } => Some((*session_id, *down, *keysym)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pure_modifier_presses_and_releases_are_emitted_once_from_aggregate_state() {
        for (name, keysym) in [
            ("shift", 0xffe1),
            ("ctrl", 0xffe3),
            ("alt", 0xffe9),
            ("command", 0xffeb),
        ] {
            let session_id = SessionId::new();
            let mut ownership = InputOwnership::for_session(session_id);
            let mut actions = Vec::new();

            collect_keyboard_events(
                session_id,
                &[],
                modifier(name, true),
                &mut ownership,
                &mut actions,
            );
            collect_keyboard_events(
                session_id,
                &[],
                modifier(name, true),
                &mut ownership,
                &mut actions,
            );
            collect_keyboard_events(
                session_id,
                &[],
                modifier(name, false),
                &mut ownership,
                &mut actions,
            );
            collect_keyboard_events(
                session_id,
                &[],
                modifier(name, false),
                &mut ownership,
                &mut actions,
            );

            assert_eq!(
                key_actions(&actions),
                [(session_id, true, keysym), (session_id, false, keysym)]
            );
        }
    }

    #[test]
    fn key_event_keeps_modifier_before_ordinary_key_then_final_sync_deduplicates() {
        let session_id = SessionId::new();
        let modifiers = modifier("ctrl", true);
        let events = [egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }];
        let mut ownership = InputOwnership::for_session(session_id);
        let mut actions = Vec::new();

        collect_keyboard_events(session_id, &events, modifiers, &mut ownership, &mut actions);

        assert_eq!(
            key_actions(&actions),
            [
                (session_id, true, 0xffe3),
                (session_id, true, u32::from(b'a'))
            ]
        );
    }

    #[test]
    fn owner_cleanup_is_targeted_and_precedes_close_or_reconnect_until_acknowledged() {
        for control in [UiAction::Reconnect, UiAction::Close] {
            let outgoing = SessionId::new();
            let incoming = SessionId::new();
            let mut ownership = InputOwnership::for_session(outgoing);
            ownership.set_test_state(0b1111, 0b1_1111, Some((123, 234)));
            let mut actions = vec![control];

            release_owner_for_control_actions(&mut ownership, &actions);
            prepare_actions(&ownership, &mut actions);

            assert!(matches!(
                actions.first(),
                Some(UiAction::ReleaseOwnedInput {
                    session_id,
                    pointer_position: Some((123, 234)),
                }) if *session_id == outgoing
            ));
            assert!(matches!(
                actions.get(1),
                Some(UiAction::Reconnect | UiAction::Close)
            ));
            assert!(!ownership.is_clear());
            assert!(actions.iter().all(|action| !matches!(
                action,
                UiAction::ReleaseOwnedInput { session_id, .. }
                    if *session_id == incoming
            )));
            ownership.acknowledge_cleanup(DispatchOutcome::Sent);
            assert!(ownership.is_clear());
        }
    }

    #[test]
    fn toolbar_and_menu_actions_release_console_ownership_before_the_action() {
        for control in [
            UiAction::CtrlAltDelete,
            UiAction::FitToWindow,
            UiAction::SetDynamicResolution(false),
            UiAction::Diagnostics,
        ] {
            let outgoing = SessionId::new();
            let mut ownership = InputOwnership::for_session(outgoing);
            ownership.set_test_state(0b0010, 1, Some((10, 20)));
            let mut actions = vec![control];

            release_owner_for_control_actions(&mut ownership, &actions);
            prepare_actions(&ownership, &mut actions);

            assert!(matches!(
                actions.first(),
                Some(UiAction::ReleaseOwnedInput { session_id, .. }) if *session_id == outgoing
            ));
            assert_eq!(actions.get(1), Some(&control));
            assert!(!ownership.is_clear());
        }
    }

    #[test]
    fn tab_widget_and_app_focus_cleanup_release_only_the_prior_session() {
        let outgoing = SessionId::new();
        let incoming = SessionId::new();
        for next_owner in [Some(incoming), None] {
            let mut ownership = InputOwnership::for_session(outgoing);
            ownership.set_test_state(0b0011, 0b1_1111, Some((20, 30)));
            let mut actions = Vec::new();

            ownership.release_if_not(next_owner);
            prepare_actions(&ownership, &mut actions);

            assert_eq!(actions.len(), 1);
            assert!(matches!(
                actions[0],
                UiAction::ReleaseOwnedInput { session_id, .. } if session_id == outgoing
            ));
            assert!(!ownership.is_clear());
        }
    }

    #[test]
    fn visible_console_hit_testing_rejects_hidden_regions_but_releases_outside() {
        let session_id = SessionId::new();
        let image = egui::Rect::from_min_max(egui::pos2(-100.0, -50.0), egui::pos2(300.0, 250.0));
        let inner = egui::Rect::from_min_max(egui::pos2(20.0, 30.0), egui::pos2(280.0, 220.0));
        let clip = egui::Rect::from_min_max(egui::pos2(40.0, 50.0), egui::pos2(260.0, 200.0));
        let visible = visible_console_rect(image, inner, clip);
        assert_eq!(visible, clip);
        let mut ownership = InputOwnership::for_session(session_id);
        let mut actions = Vec::new();
        let events = [
            egui::Event::PointerMoved(egui::pos2(0.0, 0.0)),
            egui::Event::PointerButton {
                pos: egui::pos2(0.0, 0.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: egui::pos2(60.0, 80.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerMoved(egui::pos2(0.0, 0.0)),
            egui::Event::PointerButton {
                pos: egui::pos2(0.0, 0.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ];

        collect_pointer_events(
            session_id,
            &events,
            image,
            visible,
            (400, 300),
            &mut ownership,
            &mut actions,
        );

        assert_eq!(actions.len(), 2, "hidden press/moves must be ignored");
        assert!(matches!(
            actions[0],
            UiAction::Pointer {
                session_id: target,
                buttons: 1,
                x: 160,
                y: 130,
            } if target == session_id
        ));
        assert!(matches!(
            actions[1],
            UiAction::Pointer {
                session_id: target,
                buttons: 0,
                x: 160,
                y: 130,
            } if target == session_id
        ));
        assert_eq!(ownership.pointer_buttons(), 0);
    }

    #[test]
    fn every_supported_pointer_button_releases_on_the_original_session() {
        let outgoing = SessionId::new();
        let incoming = SessionId::new();
        let image = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0));
        let mut ownership = InputOwnership::for_session(outgoing);
        let mut actions = Vec::new();

        for button in [
            egui::PointerButton::Primary,
            egui::PointerButton::Middle,
            egui::PointerButton::Secondary,
            egui::PointerButton::Extra1,
            egui::PointerButton::Extra2,
        ] {
            let events = [
                egui::Event::PointerButton {
                    pos: egui::pos2(50.0, 50.0),
                    button,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
                egui::Event::PointerButton {
                    pos: egui::pos2(150.0, 150.0),
                    button,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                },
            ];
            collect_pointer_events(
                outgoing,
                &events,
                image,
                image,
                (100, 100),
                &mut ownership,
                &mut actions,
            );
        }

        let presses = actions
            .iter()
            .filter_map(|action| match action {
                UiAction::Pointer {
                    session_id,
                    buttons,
                    ..
                } if *buttons != 0 => Some((*session_id, *buttons)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            presses,
            [
                (outgoing, 1),
                (outgoing, 2),
                (outgoing, 4),
                (outgoing, 8),
                (outgoing, 16),
            ]
        );
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(
                    action,
                    UiAction::Pointer {
                        session_id,
                        buttons: 0,
                        ..
                    } if *session_id == outgoing
                ))
                .count(),
            5
        );
        assert!(actions.iter().all(|action| !matches!(
            action,
            UiAction::Pointer { session_id, .. } if *session_id == incoming
        )));
        assert_eq!(ownership.pointer_buttons(), 0);
    }

    #[test]
    fn owner_cleanup_is_one_retryable_semantic_command_and_clears_only_after_sent() {
        let outgoing = SessionId::new();
        let incoming = SessionId::new();
        let mut ownership = InputOwnership::for_session(outgoing);
        ownership.set_test_state(0b1111, 0b1_1111, Some((123, 234)));

        ownership.schedule_cleanup();
        ownership.schedule_cleanup();
        assert_eq!(
            ownership.pending_cleanup_action(),
            Some(UiAction::ReleaseOwnedInput {
                session_id: outgoing,
                pointer_position: Some((123, 234)),
            }),
            "all pointer buttons and modifiers collapse into one bounded cleanup command"
        );
        ownership.claim(incoming, true);
        assert_eq!(ownership.session_id, Some(outgoing));

        ownership.acknowledge_cleanup(DispatchOutcome::Busy);
        assert_eq!(ownership.modifier_bits, 0b1111);
        assert_eq!(ownership.pointer_buttons, 0b1_1111);
        assert_eq!(ownership.pointer_position, Some((123, 234)));
        assert!(ownership.pending_cleanup_action().is_some());

        ownership.acknowledge_cleanup(DispatchOutcome::Disconnected);
        assert_eq!(
            ownership.pending_cleanup_action(),
            None,
            "a disconnected sender retains ownership without spinning retry attempts"
        );
        assert_eq!(ownership.session_id, Some(outgoing));
        assert_eq!(ownership.modifier_bits, 0b1111);
        assert_eq!(ownership.pointer_buttons, 0b1_1111);
        let mut blocked_actions = vec![UiAction::RefreshInventory];
        prepare_actions(&ownership, &mut blocked_actions);
        assert!(
            blocked_actions.is_empty(),
            "later controls remain blocked until manager completion"
        );
        ownership.manager_completed();
        assert!(ownership.is_clear());

        ownership = InputOwnership::for_session(outgoing);
        ownership.set_test_state(0b1111, 0b1_1111, Some((123, 234)));
        ownership.schedule_cleanup();
        ownership.acknowledge_cleanup(DispatchOutcome::Sent);
        assert!(ownership.is_clear());
        assert_eq!(ownership.pending_cleanup_action(), None);

        ownership.claim(incoming, true);
        assert_eq!(ownership.session_id, Some(incoming));

        let mut completion_before_render = InputOwnership::for_session(outgoing);
        completion_before_render.set_test_state(0b0010, 1, Some((50, 60)));
        completion_before_render.manager_completed();
        assert!(
            completion_before_render.is_clear(),
            "worker completion must clear local ownership even before state validation schedules cleanup"
        );
    }

    #[test]
    fn worker_driven_non_writable_transitions_and_tab_changes_keep_cleanup_on_old_owner() {
        let outgoing = SessionId::new();
        let incoming = SessionId::new();

        for mut state in [
            state_with([snapshot(outgoing, SessionPhase::Disconnecting, false)]),
            state_with([snapshot(outgoing, SessionPhase::Disconnected, false)]),
            state_with([snapshot(outgoing, SessionPhase::Ready, true)]),
            {
                let mut state = state_with([snapshot(outgoing, SessionPhase::Ready, false)]);
                state
                    .apply(AppEvent::Error(
                        PublicError::new(PublicErrorKind::RfbProtocol)
                            .with_public_context(outgoing, vmid(107)),
                    ))
                    .unwrap();
                state
            },
            state_with([snapshot(incoming, SessionPhase::Ready, false)]),
        ] {
            let mut ownership = InputOwnership::for_session(outgoing);
            ownership.set_test_state(0b0010, 1, Some((10, 20)));
            ownership.validate_against_state(&state);
            assert_eq!(
                ownership.pending_cleanup_action(),
                Some(UiAction::ReleaseOwnedInput {
                    session_id: outgoing,
                    pointer_position: Some((10, 20)),
                })
            );
            ownership.acknowledge_cleanup(DispatchOutcome::Busy);
            assert_eq!(ownership.session_id, Some(outgoing));
            state.select_session(Some(incoming));
            ownership.claim(incoming, true);
            assert_eq!(ownership.session_id, Some(outgoing));
        }

        let state = state_with([
            snapshot(outgoing, SessionPhase::Ready, false),
            snapshot(incoming, SessionPhase::Ready, false),
        ]);
        let mut ownership = InputOwnership::for_session(outgoing);
        ownership.set_test_state(0b0010, 1, Some((30, 40)));
        ownership.validate_against_state(&state);
        ownership.release_if_not(state.selected_session_id());
        assert_eq!(
            ownership.pending_cleanup_action(),
            Some(UiAction::ReleaseOwnedInput {
                session_id: outgoing,
                pointer_position: Some((30, 40)),
            }),
            "a simultaneous selection change cannot retarget cleanup to the incoming tab"
        );
    }

    #[test]
    fn oversized_one_to_one_paint_uses_bay_clip_while_pointer_mapping_uses_full_image() {
        let image = egui::Rect::from_min_max(egui::pos2(-100.0, -50.0), egui::pos2(500.0, 350.0));
        let inner = egui::Rect::from_min_max(egui::pos2(20.0, 30.0), egui::pos2(280.0, 220.0));
        let ui_clip = egui::Rect::from_min_max(egui::pos2(40.0, 50.0), egui::pos2(260.0, 200.0));

        assert_eq!(
            framebuffer_paint_clip(inner, ui_clip),
            inner.intersect(ui_clip)
        );
        assert_eq!(visible_console_rect(image, inner, ui_clip), ui_clip);
        assert_eq!(
            super::pointer_coordinates(egui::pos2(50.0, 75.0), image, (600, 400)),
            Some((150, 125)),
            "cropped pointer coordinates still map against the original full image"
        );
    }

    #[test]
    fn framebuffer_image_shape_carries_the_bay_clip_and_full_uncropped_geometry() {
        let context = egui::Context::default();
        let texture_id = egui::TextureId::Managed(42);
        let image = egui::Rect::from_min_max(egui::pos2(-100.0, -50.0), egui::pos2(500.0, 350.0));
        let inner = egui::Rect::from_min_max(egui::pos2(20.0, 30.0), egui::pos2(280.0, 220.0));
        let ui_clip = egui::Rect::from_min_max(egui::pos2(40.0, 50.0), egui::pos2(260.0, 200.0));

        let output = context.run(egui::RawInput::default(), |context| {
            let painter = egui::Painter::new(
                context.clone(),
                egui::LayerId::new(egui::Order::Middle, egui::Id::new("paint-clip-test")),
                ui_clip,
            );
            paint_framebuffer(&painter, texture_id, image, inner);
        });
        let clipped = output
            .shapes
            .iter()
            .find(|clipped| {
                matches!(
                    &clipped.shape,
                    egui::Shape::Mesh(mesh) if mesh.texture_id == texture_id
                )
            })
            .expect("framebuffer image shape must be painted");
        assert_eq!(clipped.clip_rect, inner.intersect(ui_clip));
        let egui::Shape::Mesh(mesh) = &clipped.shape else {
            unreachable!();
        };
        let mut painted_bounds = egui::Rect::NOTHING;
        for vertex in &mesh.vertices {
            painted_bounds.extend_with(vertex.pos);
        }
        assert_eq!(painted_bounds, image);
    }
}
