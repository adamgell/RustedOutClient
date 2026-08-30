use std::{cmp::Ordering, fmt};

use thiserror::Error;

use crate::{
    config::AppConfig,
    connection::FbRect,
    model::{ScaleMode, VmId},
    session::{
        AppEvent, DesktopSize, OpenOptions, PublicErrorKind, SessionId, SessionPhase,
        SessionSnapshot,
    },
    ssh::{InventorySnapshot, VmInventoryItem, VmStatus},
    vnc::{normalize_resize_request, ClipboardText, InputError, ProtocolLimits, VncOptions},
};

use super::actions::ActionAvailability;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupState {
    Configured,
    MissingConfiguration,
    InvalidConfiguration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardStatus {
    Disabled,
    Ready,
    Sent,
    Received,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueStatus {
    Ready,
    Busy,
    Disconnected,
}

impl QueueStatus {
    pub fn is_busy(self) -> bool {
        self == Self::Busy
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackingViewport {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Eq, PartialEq)]
pub struct FramebufferImage {
    width: u16,
    height: u16,
    rgba: Vec<u8>,
    revision: u64,
    dirty: Option<DirtyRegion>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirtyRegion {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl DirtyRegion {
    fn full(width: u16, height: u16) -> Self {
        Self {
            left: 0,
            top: 0,
            right: u32::from(width),
            bottom: u32::from(height),
        }
    }

    fn from_rect(rect: &FbRect) -> Self {
        Self {
            left: rect.x,
            top: rect.y,
            right: rect.x + rect.w,
            bottom: rect.y + rect.h,
        }
    }

    fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FramebufferUploadKind {
    Full,
    Partial,
}

/// Ephemeral pixel staging for one texture submission. Pixel bytes intentionally
/// implement neither `Debug` nor `Display`.
pub struct FramebufferUpload {
    kind: FramebufferUploadKind,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    revision: u64,
    rgba: Vec<u8>,
}

impl FramebufferUpload {
    pub fn kind(&self) -> FramebufferUploadKind {
        self.kind
    }

    pub fn x(&self) -> u16 {
        self.x
    }

    pub fn y(&self) -> u16 {
        self.y
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

impl fmt::Debug for FramebufferImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FramebufferImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("byte_count", &self.rgba.len())
            .field("revision", &self.revision)
            .finish()
    }
}

impl FramebufferImage {
    fn new(size: DesktopSize) -> Result<Self, AppStateError> {
        let layout = crate::vnc::validate_framebuffer_layout(
            size.width,
            size.height,
            ProtocolLimits::default(),
        )
        .map_err(|_| AppStateError::InvalidFramebuffer)?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(layout.rgba_bytes)
            .map_err(|_| AppStateError::Allocation)?;
        rgba.resize(layout.rgba_bytes, 0);
        Ok(Self {
            width: size.width,
            height: size.height,
            rgba,
            revision: 0,
            dirty: Some(DirtyRegion::full(size.width, size.height)),
        })
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn validate_rect(&self, rect: &FbRect) -> Result<(), AppStateError> {
        if rect.w == 0 || rect.h == 0 {
            return Err(AppStateError::InvalidFramebuffer);
        }
        let right = rect
            .x
            .checked_add(rect.w)
            .ok_or(AppStateError::InvalidFramebuffer)?;
        let bottom = rect
            .y
            .checked_add(rect.h)
            .ok_or(AppStateError::InvalidFramebuffer)?;
        if right > u32::from(self.width) || bottom > u32::from(self.height) {
            return Err(AppStateError::InvalidFramebuffer);
        }
        let expected = u64::from(rect.w)
            .checked_mul(u64::from(rect.h))
            .and_then(|pixels| pixels.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or(AppStateError::InvalidFramebuffer)?;
        let maximum = usize::try_from(ProtocolLimits::default().max_framebuffer_bytes)
            .map_err(|_| AppStateError::InvalidFramebuffer)?;
        if expected != rect.rgba.len() || expected > maximum {
            return Err(AppStateError::InvalidFramebuffer);
        }
        Ok(())
    }

    fn apply_rect(&mut self, rect: FbRect) -> Result<(), AppStateError> {
        self.validate_rect(&rect)?;
        let source_stride = usize::try_from(rect.w)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(AppStateError::InvalidFramebuffer)?;
        let destination_stride = usize::from(self.width)
            .checked_mul(4)
            .ok_or(AppStateError::InvalidFramebuffer)?;
        let x_offset = usize::try_from(rect.x)
            .ok()
            .and_then(|x| x.checked_mul(4))
            .ok_or(AppStateError::InvalidFramebuffer)?;
        let y = usize::try_from(rect.y).map_err(|_| AppStateError::InvalidFramebuffer)?;
        let height = usize::try_from(rect.h).map_err(|_| AppStateError::InvalidFramebuffer)?;
        for row in 0..height {
            let source_start = row
                .checked_mul(source_stride)
                .ok_or(AppStateError::InvalidFramebuffer)?;
            let source_end = source_start
                .checked_add(source_stride)
                .ok_or(AppStateError::InvalidFramebuffer)?;
            let destination_start = y
                .checked_add(row)
                .and_then(|row| row.checked_mul(destination_stride))
                .and_then(|offset| offset.checked_add(x_offset))
                .ok_or(AppStateError::InvalidFramebuffer)?;
            let destination_end = destination_start
                .checked_add(source_stride)
                .ok_or(AppStateError::InvalidFramebuffer)?;
            self.rgba[destination_start..destination_end]
                .copy_from_slice(&rect.rgba[source_start..source_end]);
        }
        self.revision = self.revision.wrapping_add(1);
        let changed = DirtyRegion::from_rect(&rect);
        self.dirty = Some(
            self.dirty
                .map_or(changed, |existing| existing.union(changed)),
        );
        Ok(())
    }

    fn upload_plan(&self, force_full: bool) -> Result<Option<FramebufferUpload>, AppStateError> {
        let region = if force_full {
            DirtyRegion::full(self.width, self.height)
        } else if let Some(region) = self.dirty {
            region
        } else {
            return Ok(None);
        };
        let region_width = region
            .right
            .checked_sub(region.left)
            .ok_or(AppStateError::InvalidFramebuffer)?;
        let region_height = region
            .bottom
            .checked_sub(region.top)
            .ok_or(AppStateError::InvalidFramebuffer)?;
        let byte_count = u64::from(region_width)
            .checked_mul(u64::from(region_height))
            .and_then(|pixels| pixels.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or(AppStateError::InvalidFramebuffer)?;
        let maximum = usize::try_from(ProtocolLimits::default().max_framebuffer_bytes)
            .map_err(|_| AppStateError::InvalidFramebuffer)?;
        if byte_count > maximum {
            return Err(AppStateError::InvalidFramebuffer);
        }
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(byte_count)
            .map_err(|_| AppStateError::Allocation)?;
        let source_stride = usize::from(self.width)
            .checked_mul(4)
            .ok_or(AppStateError::InvalidFramebuffer)?;
        let row_bytes = usize::try_from(region_width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(AppStateError::InvalidFramebuffer)?;
        let x_bytes = usize::try_from(region.left)
            .ok()
            .and_then(|x| x.checked_mul(4))
            .ok_or(AppStateError::InvalidFramebuffer)?;
        for row in region.top..region.bottom {
            let start = usize::try_from(row)
                .ok()
                .and_then(|row| row.checked_mul(source_stride))
                .and_then(|offset| offset.checked_add(x_bytes))
                .ok_or(AppStateError::InvalidFramebuffer)?;
            let end = start
                .checked_add(row_bytes)
                .ok_or(AppStateError::InvalidFramebuffer)?;
            rgba.extend_from_slice(
                self.rgba
                    .get(start..end)
                    .ok_or(AppStateError::InvalidFramebuffer)?,
            );
        }
        let full = region == DirtyRegion::full(self.width, self.height);
        Ok(Some(FramebufferUpload {
            kind: if full {
                FramebufferUploadKind::Full
            } else {
                FramebufferUploadKind::Partial
            },
            x: u16::try_from(region.left).map_err(|_| AppStateError::InvalidFramebuffer)?,
            y: u16::try_from(region.top).map_err(|_| AppStateError::InvalidFramebuffer)?,
            width: u16::try_from(region_width).map_err(|_| AppStateError::InvalidFramebuffer)?,
            height: u16::try_from(region_height).map_err(|_| AppStateError::InvalidFramebuffer)?,
            revision: self.revision,
            rgba,
        }))
    }

    fn acknowledge_upload(&mut self, revision: u64) {
        if self.revision == revision {
            self.dirty = None;
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTabState {
    pub snapshot: SessionSnapshot,
    pub scale_mode: ScaleMode,
    pub viewport: Option<BackingViewport>,
    pub clipboard_status: ClipboardStatus,
    pub last_error: Option<PublicErrorKind>,
    pub last_input_error: Option<InputError>,
    phase_history: Vec<SessionPhase>,
    framebuffer: Option<FramebufferImage>,
}

impl SessionTabState {
    pub fn framebuffer(&self) -> Option<&FramebufferImage> {
        self.framebuffer.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FavoriteDefinition {
    vmid: VmId,
    alias: Option<String>,
    scale_mode: ScaleMode,
    view_only: bool,
    sort_position: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryRow {
    pub vmid: VmId,
    pub name: String,
    pub alias: Option<String>,
    pub status: VmStatus,
    pub favorite: bool,
    pub stale: bool,
    pub observed_at_unix_ms: u64,
}

impl InventoryRow {
    pub fn can_open(&self) -> bool {
        self.status == VmStatus::Running
    }
}

pub enum UiEffect {
    WriteHostClipboard {
        session_id: SessionId,
        text: ClipboardText,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum AppStateError {
    #[error("session state is unavailable")]
    UnknownSession,
    #[error("framebuffer dimensions are unavailable")]
    MissingFramebufferSize,
    #[error("framebuffer update is invalid")]
    InvalidFramebuffer,
    #[error("framebuffer allocation failed")]
    Allocation,
    #[error("native session render capacity reached")]
    SessionCapacity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppState {
    setup: SetupState,
    profile_name: String,
    node_name: String,
    clipboard_enabled: bool,
    default_scale_mode: ScaleMode,
    default_view_only: bool,
    favorites: Vec<FavoriteDefinition>,
    inventory: Option<InventorySnapshot>,
    search: String,
    selected_inventory: Option<VmId>,
    tabs: Vec<SessionTabState>,
    selected_session: Option<SessionId>,
    queue_status: QueueStatus,
    last_error: Option<PublicErrorKind>,
    fullscreen: bool,
    diagnostics_open: bool,
}

impl AppState {
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            setup: SetupState::Configured,
            profile_name: config.profile.name.clone(),
            node_name: config.profile.node.as_str().to_owned(),
            clipboard_enabled: config.clipboard_enabled,
            default_scale_mode: config.display.scale_mode,
            default_view_only: config.display.view_only,
            favorites: config
                .favorites
                .iter()
                .map(|favorite| FavoriteDefinition {
                    vmid: favorite.vmid,
                    alias: favorite.alias.clone(),
                    scale_mode: favorite.scale_mode,
                    view_only: favorite.view_only,
                    sort_position: favorite.sort_position,
                })
                .collect(),
            inventory: None,
            search: String::new(),
            selected_inventory: None,
            tabs: Vec::new(),
            selected_session: None,
            queue_status: QueueStatus::Ready,
            last_error: None,
            fullscreen: false,
            diagnostics_open: false,
        }
    }

    pub fn missing_configuration() -> Self {
        Self::setup_only(SetupState::MissingConfiguration)
    }

    pub fn invalid_configuration() -> Self {
        Self::setup_only(SetupState::InvalidConfiguration)
    }

    fn setup_only(setup: SetupState) -> Self {
        Self {
            setup,
            profile_name: String::new(),
            node_name: String::new(),
            clipboard_enabled: false,
            default_scale_mode: ScaleMode::Fit,
            default_view_only: false,
            favorites: Vec::new(),
            inventory: None,
            search: String::new(),
            selected_inventory: None,
            tabs: Vec::new(),
            selected_session: None,
            queue_status: QueueStatus::Disconnected,
            last_error: None,
            fullscreen: false,
            diagnostics_open: false,
        }
    }

    pub fn setup_state(&self) -> &SetupState {
        &self.setup
    }

    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    pub fn set_search(&mut self, search: impl Into<String>) {
        self.search = search.into();
    }

    pub fn search(&self) -> &str {
        &self.search
    }

    pub fn select_inventory(&mut self, vmid: Option<VmId>) {
        self.selected_inventory = vmid;
    }

    pub fn selected_inventory(&self) -> Option<VmId> {
        self.selected_inventory
    }

    pub fn select_session(&mut self, session_id: Option<SessionId>) {
        if session_id.is_none()
            || self
                .tabs
                .iter()
                .any(|tab| Some(tab.snapshot.session_id) == session_id)
        {
            self.selected_session = session_id;
        }
    }

    pub fn selected_session_id(&self) -> Option<SessionId> {
        self.selected_session
    }

    pub fn tabs(&self) -> &[SessionTabState] {
        &self.tabs
    }

    pub fn selected_session(&self) -> Option<&SessionTabState> {
        let selected = self.selected_session?;
        self.tabs
            .iter()
            .find(|tab| tab.snapshot.session_id == selected)
    }

    pub(crate) fn selected_session_mut(&mut self) -> Option<&mut SessionTabState> {
        let selected = self.selected_session?;
        self.tabs
            .iter_mut()
            .find(|tab| tab.snapshot.session_id == selected)
    }

    pub fn framebuffer(&self, session_id: SessionId) -> Option<&FramebufferImage> {
        self.tabs
            .iter()
            .find(|tab| tab.snapshot.session_id == session_id)
            .and_then(SessionTabState::framebuffer)
    }

    pub fn framebuffer_upload_plan(
        &self,
        session_id: SessionId,
        force_full: bool,
    ) -> Result<Option<FramebufferUpload>, AppStateError> {
        self.framebuffer(session_id)
            .ok_or(AppStateError::MissingFramebufferSize)?
            .upload_plan(force_full)
    }

    pub fn acknowledge_framebuffer_upload(
        &mut self,
        session_id: SessionId,
        revision: u64,
    ) -> Result<(), AppStateError> {
        let framebuffer = self
            .tabs
            .iter_mut()
            .find(|tab| tab.snapshot.session_id == session_id)
            .ok_or(AppStateError::UnknownSession)?
            .framebuffer
            .as_mut()
            .ok_or(AppStateError::MissingFramebufferSize)?;
        framebuffer.acknowledge_upload(revision);
        Ok(())
    }

    pub fn inventory_rows(&self) -> Vec<InventoryRow> {
        let Some(snapshot) = &self.inventory else {
            return Vec::new();
        };
        let query = self.search.to_ascii_lowercase();
        let mut rows = snapshot
            .vms
            .iter()
            .filter_map(|item| {
                let favorite = self.favorite(item.vmid);
                let vmid_text = item.vmid.to_string();
                let matches = query.is_empty()
                    || vmid_text.to_ascii_lowercase().contains(&query)
                    || item.name.to_ascii_lowercase().contains(&query)
                    || favorite
                        .and_then(|favorite| favorite.alias.as_deref())
                        .is_some_and(|alias| alias.to_ascii_lowercase().contains(&query));
                matches.then(|| InventoryRow {
                    vmid: item.vmid,
                    name: item.name.clone(),
                    alias: favorite.and_then(|favorite| favorite.alias.clone()),
                    status: item.status,
                    favorite: favorite.is_some(),
                    stale: snapshot.stale,
                    observed_at_unix_ms: snapshot.observed_at_unix_ms,
                })
            })
            .collect::<Vec<_>>();
        rows.sort_by(
            |left, right| match (self.favorite(left.vmid), self.favorite(right.vmid)) {
                (Some(left_favorite), Some(right_favorite)) => left_favorite
                    .sort_position
                    .cmp(&right_favorite.sort_position)
                    .then_with(|| left.vmid.cmp(&right.vmid)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => left.vmid.cmp(&right.vmid),
            },
        );
        rows
    }

    pub fn inventory_age_source(&self) -> Option<(u64, bool)> {
        self.inventory
            .as_ref()
            .map(|snapshot| (snapshot.observed_at_unix_ms, snapshot.stale))
    }

    pub fn vm_name(&self, vmid: VmId) -> Option<&str> {
        self.inventory
            .as_ref()?
            .vms
            .iter()
            .find(|item| item.vmid == vmid)
            .map(|item| item.name.as_str())
    }

    pub fn set_viewport(
        &mut self,
        session_id: SessionId,
        width: u32,
        height: u32,
    ) -> Result<(), AppStateError> {
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.snapshot.session_id == session_id)
            .ok_or(AppStateError::UnknownSession)?;
        tab.viewport = Some(BackingViewport { width, height });
        Ok(())
    }

    pub fn action_availability(&self) -> ActionAvailability {
        ActionAvailability::from_state(self)
    }

    pub fn queue_status(&self) -> QueueStatus {
        self.queue_status
    }

    pub(crate) fn set_queue_status(&mut self, status: QueueStatus) {
        self.queue_status = status;
    }

    pub(crate) fn set_clipboard_status(&mut self, session_id: SessionId, status: ClipboardStatus) {
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.snapshot.session_id == session_id)
        {
            tab.clipboard_status = status;
        }
    }

    pub(crate) fn selected_inventory_item(&self) -> Option<&VmInventoryItem> {
        let selected = self.selected_inventory?;
        self.inventory
            .as_ref()?
            .vms
            .iter()
            .find(|item| item.vmid == selected)
    }

    pub(crate) fn selected_resize_is_actionable(&self) -> bool {
        let Some(tab) = self.selected_session() else {
            return false;
        };
        if tab.snapshot.phase != SessionPhase::Ready || tab.last_error.is_some() {
            return false;
        }
        tab.viewport.is_some_and(|viewport| {
            normalize_resize_request(viewport.width, viewport.height, ProtocolLimits::default())
                .is_ok()
        })
    }

    pub(crate) fn open_options(&self, vmid: VmId) -> OpenOptions {
        let favorite = self.favorite(vmid);
        OpenOptions {
            vnc: VncOptions::default(),
            view_only: favorite
                .map(|favorite| favorite.view_only)
                .unwrap_or(self.default_view_only),
            clipboard_enabled: self.clipboard_enabled,
            dynamic_resolution: true,
        }
    }

    pub(crate) fn set_scale_mode(&mut self, mode: ScaleMode) {
        if let Some(tab) = self.selected_session_mut() {
            tab.scale_mode = mode;
        }
    }

    pub(crate) fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
    }

    pub fn fullscreen(&self) -> bool {
        self.fullscreen
    }

    pub(crate) fn toggle_diagnostics(&mut self) {
        self.diagnostics_open = !self.diagnostics_open;
    }

    pub fn diagnostics_open(&self) -> bool {
        self.diagnostics_open
    }

    pub fn close_diagnostics(&mut self) {
        self.diagnostics_open = false;
    }

    pub fn apply(&mut self, event: AppEvent) -> Result<Vec<UiEffect>, AppStateError> {
        let mut effects = Vec::new();
        match event {
            AppEvent::CachedInventory(snapshot) => {
                if self.inventory.as_ref().is_none_or(|current| current.stale) {
                    self.inventory = Some(snapshot);
                }
            }
            AppEvent::LiveInventory(snapshot) => self.inventory = Some(snapshot),
            AppEvent::SessionChanged(snapshot) => self.apply_session_snapshot(snapshot)?,
            AppEvent::FocusExisting { session_id } => {
                if self
                    .tabs
                    .iter()
                    .any(|tab| tab.snapshot.session_id == session_id)
                {
                    self.selected_session = Some(session_id);
                } else {
                    return Err(AppStateError::UnknownSession);
                }
            }
            AppEvent::Framebuffer { session_id, rects } => {
                self.apply_framebuffer(session_id, rects)?;
            }
            AppEvent::ClipboardReceived { session_id, text } => {
                effects.push(UiEffect::WriteHostClipboard { session_id, text });
            }
            AppEvent::InputRejected { session_id, reason } => {
                if let Some(tab) = self
                    .tabs
                    .iter_mut()
                    .find(|tab| tab.snapshot.session_id == session_id)
                {
                    tab.last_input_error = Some(reason);
                }
            }
            AppEvent::Error(error) => {
                self.last_error = Some(error.kind());
                if error.kind() != PublicErrorKind::Queue {
                    if let Some(session_id) = error.session_id() {
                        if let Some(tab) = self
                            .tabs
                            .iter_mut()
                            .find(|tab| tab.snapshot.session_id == session_id)
                        {
                            tab.last_error = Some(error.kind());
                        }
                    }
                }
            }
        }
        Ok(effects)
    }

    fn apply_session_snapshot(&mut self, snapshot: SessionSnapshot) -> Result<(), AppStateError> {
        let session_id = snapshot.session_id;
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.snapshot.session_id == session_id)
        {
            let size_changed = snapshot.guest_size != tab.snapshot.guest_size;
            let replacement = if size_changed {
                snapshot.guest_size.map(FramebufferImage::new).transpose()?
            } else {
                None
            };
            if snapshot.phase != tab.snapshot.phase {
                if tab.phase_history.len() == 16 {
                    tab.phase_history.remove(0);
                }
                tab.phase_history.push(snapshot.phase);
            }
            tab.snapshot = snapshot;
            if size_changed {
                tab.framebuffer = replacement;
            }
            if tab.snapshot.phase == SessionPhase::Ready {
                tab.last_error = None;
            }
        } else {
            self.tabs
                .retain(|tab| tab.snapshot.phase != SessionPhase::Disconnected);
            if self.selected_session.is_some_and(|selected| {
                !self
                    .tabs
                    .iter()
                    .any(|tab| tab.snapshot.session_id == selected)
            }) {
                self.selected_session = None;
            }
            if self.tabs.len() >= 2 {
                return Err(AppStateError::SessionCapacity);
            }
            let scale_mode = self
                .favorite(snapshot.vmid)
                .map(|favorite| favorite.scale_mode)
                .unwrap_or(self.default_scale_mode);
            let clipboard_status = if snapshot.clipboard_enabled {
                ClipboardStatus::Ready
            } else {
                ClipboardStatus::Disabled
            };
            let framebuffer = snapshot.guest_size.map(FramebufferImage::new).transpose()?;
            self.tabs.push(SessionTabState {
                phase_history: vec![snapshot.phase],
                snapshot,
                scale_mode,
                viewport: None,
                clipboard_status,
                last_error: None,
                last_input_error: None,
                framebuffer,
            });
            self.selected_session = Some(session_id);
        }
        if self.selected_session.is_none() {
            self.selected_session = Some(session_id);
        }
        Ok(())
    }

    fn apply_framebuffer(
        &mut self,
        session_id: SessionId,
        rects: Vec<FbRect>,
    ) -> Result<(), AppStateError> {
        if rects.len() > usize::from(ProtocolLimits::default().max_rectangles) {
            return Err(AppStateError::InvalidFramebuffer);
        }
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.snapshot.session_id == session_id)
            .ok_or(AppStateError::UnknownSession)?;
        if tab.framebuffer.is_none() {
            tab.framebuffer = tab
                .snapshot
                .guest_size
                .map(FramebufferImage::new)
                .transpose()?;
        }
        let framebuffer = tab
            .framebuffer
            .as_mut()
            .ok_or(AppStateError::MissingFramebufferSize)?;
        for rect in &rects {
            framebuffer.validate_rect(rect)?;
        }
        for rect in rects {
            framebuffer.apply_rect(rect)?;
        }
        Ok(())
    }

    fn favorite(&self, vmid: VmId) -> Option<&FavoriteDefinition> {
        self.favorites.iter().find(|favorite| favorite.vmid == vmid)
    }

    pub fn diagnostics_summary(&self) -> String {
        let mut lines = vec![
            format!("RustedOutClient {}", env!("CARGO_PKG_VERSION")),
            format!(
                "Upstream base: {}",
                "999e00e3a3672efdbf8e8f307e7bd60875dee67e"
            ),
            format!(
                "Platform: {}/{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
            format!("Profile: {}", self.profile_name),
            format!("Node: {}", self.node_name),
        ];
        if let Some((observed, stale)) = self.inventory_age_source() {
            lines.push(format!(
                "Inventory: {} at {observed}",
                if stale { "stale" } else { "live" }
            ));
        }
        if let Some(tab) = self.selected_session() {
            lines.push(format!("VMID: {}", tab.snapshot.vmid));
            if let Some(name) = self.vm_name(tab.snapshot.vmid) {
                lines.push(format!("VM name: {name}"));
            }
            lines.push(format!("Phase: {:?}", tab.snapshot.phase));
            lines.push(format!("Phase history: {:?}", tab.phase_history));
            lines.push(format!("Scale mode: {:?}", tab.scale_mode));
            lines.push(format!("View only: {}", tab.snapshot.view_only));
            if let Some(size) = tab.snapshot.guest_size {
                lines.push(format!("Guest size: {}x{}", size.width, size.height));
            }
            lines.push(format!("Resize state: {:?}", tab.snapshot.resize_status));
            if let Some(error) = tab.last_error {
                lines.push(format!("Error category: {error:?}"));
            }
        }
        if let Some(error) = self.last_error {
            lines.push(format!("Last app error category: {error:?}"));
        }
        lines.join("\n")
    }
}
