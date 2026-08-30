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
    actions::{selected_tab, UiAction},
    state::{AppState, ClipboardStatus, FramebufferImage, QueueStatus, SetupState},
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
    revision: u64,
}

pub(crate) struct ViewResources {
    textures: HashMap<SessionId, TextureEntry>,
    pointer_buttons: u8,
    pointer_position: Option<(u16, u16)>,
    modifier_bits: u8,
    app_focused: bool,
}

impl Default for ViewResources {
    fn default() -> Self {
        Self {
            textures: HashMap::new(),
            pointer_buttons: 0,
            pointer_position: None,
            modifier_bits: 0,
            app_focused: true,
        }
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
    resources.textures.retain(|session_id, _| {
        state
            .tabs()
            .iter()
            .any(|tab| tab.snapshot.session_id == *session_id)
    });
    let mut actions = Vec::new();
    render_menu_bar(ctx, state, &mut actions);

    if state.setup_state() != &SetupState::Configured {
        render_setup(ctx, state.setup_state());
        return actions;
    }

    render_status_rail(ctx, state);
    render_sidebar(ctx, state, &mut actions);
    render_workspace(ctx, state, resources, &mut actions);
    render_diagnostics(ctx, state, &mut actions);

    let focused = ctx.input(|input| input.focused);
    if resources.app_focused && !focused && state.selected_session_id().is_some() {
        if resources.pointer_buttons != 0 {
            if let Some((x, y)) = resources.pointer_position {
                actions.push(UiAction::Pointer { buttons: 0, x, y });
            }
        }
        resources.pointer_buttons = 0;
        resources.pointer_position = None;
        resources.modifier_bits = 0;
        actions.push(UiAction::FocusLost);
    }
    resources.app_focused = focused;
    actions
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
    state: &AppState,
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
    let Some(texture) = texture_for(ui.ctx(), resources, tab.snapshot.session_id, framebuffer)
    else {
        return;
    };
    let image_size = display_size(
        framebuffer,
        inner.size(),
        tab.scale_mode,
        ui.ctx().pixels_per_point(),
    );
    let image_rect = egui::Rect::from_center_size(inner.center(), image_size);
    ui.painter().image(
        texture.id(),
        image_rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    if response.has_focus() {
        collect_keyboard_actions(ui.ctx(), resources, actions);
    }
    collect_pointer_actions(ui.ctx(), image_rect, framebuffer, resources, actions);
}

fn texture_for(
    ctx: &egui::Context,
    resources: &mut ViewResources,
    session_id: SessionId,
    framebuffer: &FramebufferImage,
) -> Option<egui::TextureHandle> {
    let image = || {
        egui::ColorImage::from_rgba_unmultiplied(
            [
                usize::from(framebuffer.width()),
                usize::from(framebuffer.height()),
            ],
            framebuffer.rgba(),
        )
    };
    let entry = resources
        .textures
        .entry(session_id)
        .or_insert_with(|| TextureEntry {
            handle: ctx.load_texture(
                format!("session-{session_id:?}"),
                image(),
                egui::TextureOptions::NEAREST,
            ),
            width: framebuffer.width(),
            height: framebuffer.height(),
            revision: framebuffer.revision(),
        });
    if entry.width != framebuffer.width() || entry.height != framebuffer.height() {
        *entry = TextureEntry {
            handle: ctx.load_texture(
                format!("session-{session_id:?}"),
                image(),
                egui::TextureOptions::NEAREST,
            ),
            width: framebuffer.width(),
            height: framebuffer.height(),
            revision: framebuffer.revision(),
        };
    } else if entry.revision != framebuffer.revision() {
        entry.handle.set(image(), egui::TextureOptions::NEAREST);
        entry.revision = framebuffer.revision();
    }
    Some(entry.handle.clone())
}

fn display_size(
    framebuffer: &FramebufferImage,
    available: Vec2,
    scale_mode: ScaleMode,
    pixels_per_point: f32,
) -> Vec2 {
    let native = egui::vec2(
        f32::from(framebuffer.width()) / pixels_per_point.max(1.0),
        f32::from(framebuffer.height()) / pixels_per_point.max(1.0),
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

fn collect_keyboard_actions(
    ctx: &egui::Context,
    resources: &mut ViewResources,
    actions: &mut Vec<UiAction>,
) {
    let events = ctx.input(|input| input.events.clone());
    for event in events {
        match event {
            egui::Event::Key {
                key,
                pressed,
                repeat: _,
                modifiers,
                physical_key: _,
            } => {
                sync_modifiers(modifiers, resources, actions);
                if let Some(keysym) = key_to_keysym(key, modifiers.shift) {
                    actions.push(UiAction::Key {
                        down: pressed,
                        keysym,
                    });
                }
            }
            egui::Event::Text(text) => {
                for character in text.chars().filter(|character| !character.is_ascii()) {
                    let keysym = 0x0100_0000 | u32::from(character);
                    actions.push(UiAction::Key { down: true, keysym });
                    actions.push(UiAction::Key {
                        down: false,
                        keysym,
                    });
                }
            }
            _ => {}
        }
    }
}

fn sync_modifiers(
    modifiers: egui::Modifiers,
    resources: &mut ViewResources,
    actions: &mut Vec<UiAction>,
) {
    let desired = [
        (modifiers.shift, 0xffe1_u32, 0b0001_u8),
        (modifiers.ctrl, 0xffe3_u32, 0b0010_u8),
        (modifiers.alt, 0xffe9_u32, 0b0100_u8),
        (modifiers.command, 0xffeb_u32, 0b1000_u8),
    ];
    for (enabled, keysym, bit) in desired {
        let was_enabled = resources.modifier_bits & bit != 0;
        if enabled != was_enabled {
            actions.push(UiAction::Key {
                down: enabled,
                keysym,
            });
            if enabled {
                resources.modifier_bits |= bit;
            } else {
                resources.modifier_bits &= !bit;
            }
        }
    }
}

fn collect_pointer_actions(
    ctx: &egui::Context,
    image_rect: egui::Rect,
    framebuffer: &FramebufferImage,
    resources: &mut ViewResources,
    actions: &mut Vec<UiAction>,
) {
    let events = ctx.input(|input| input.events.clone());
    for event in events {
        match event {
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: _,
            } => {
                let mask = pointer_button_mask(button);
                if pressed {
                    if !image_rect.contains(pos) {
                        continue;
                    }
                    resources.pointer_buttons |= mask;
                } else if resources.pointer_buttons & mask != 0 {
                    resources.pointer_buttons &= !mask;
                } else {
                    continue;
                }
                let position = if image_rect.contains(pos) {
                    pointer_coordinates(pos, image_rect, framebuffer)
                } else {
                    resources.pointer_position
                };
                if let Some((x, y)) = position {
                    resources.pointer_position = Some((x, y));
                    actions.push(UiAction::Pointer {
                        buttons: resources.pointer_buttons,
                        x,
                        y,
                    });
                }
            }
            egui::Event::PointerMoved(pos) if image_rect.contains(pos) => {
                if let Some((x, y)) = pointer_coordinates(pos, image_rect, framebuffer) {
                    resources.pointer_position = Some((x, y));
                    actions.push(UiAction::Pointer {
                        buttons: resources.pointer_buttons,
                        x,
                        y,
                    });
                }
            }
            egui::Event::PointerGone => {
                if resources.pointer_buttons != 0 {
                    if let Some((x, y)) = resources.pointer_position {
                        actions.push(UiAction::Pointer { buttons: 0, x, y });
                    }
                }
                resources.pointer_buttons = 0;
                resources.pointer_position = None;
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
    framebuffer: &FramebufferImage,
) -> Option<(u16, u16)> {
    if image_rect.width() <= 0.0 || image_rect.height() <= 0.0 {
        return None;
    }
    let relative_x = ((position.x - image_rect.left()) / image_rect.width()).clamp(0.0, 1.0);
    let relative_y = ((position.y - image_rect.top()) / image_rect.height()).clamp(0.0, 1.0);
    let x = (relative_x * f32::from(framebuffer.width())).floor() as u16;
    let y = (relative_y * f32::from(framebuffer.height())).floor() as u16;
    Some((
        x.min(framebuffer.width().saturating_sub(1)),
        y.min(framebuffer.height().saturating_sub(1)),
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
