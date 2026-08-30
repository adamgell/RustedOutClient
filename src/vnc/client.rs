use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;
use tracing::debug;

use crate::{
    connection::{
        ClipboardSlot, DesktopSize, FbRect, ResizeProtocolOutcome, VncCommand, VncEvent,
        VncSessionChannels, VNC_QUEUE_CAPACITY,
    },
    ssh::{ProxyTicket, TrustedSshProxy},
};

use tight::TightState;
use zrle::ZrleState;

use super::{
    encoding::{self, copyrect, hextile, raw, tight, zrle},
    framebuffer::{CheckedRect, Framebuffer},
    limits::validate_framebuffer_layout_for_phase,
    messages::{client_msg, encoding as enc, server_msg, PixelFormat},
    negotiate_version,
    security::negotiate_security,
    wire::{allocate_zeroed, sanitize_remote_text},
    ClipboardText, ProtocolLimits, RfbError, RfbErrorKind, RfbPhase, RfbReader,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VncOptions {
    pub limits: ProtocolLimits,
    pub shared: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtendedDesktopSize {
    ServerSize(DesktopSize),
    ServerUnsupported(DesktopSize),
    Pending(DesktopSize),
    Rejected,
    Unsupported,
}

impl Default for VncOptions {
    fn default() -> Self {
        Self {
            limits: ProtocolLimits::default(),
            shared: true,
        }
    }
}

pub struct ServerInit {
    pub width: u16,
    pub height: u16,
    pub pixel_format: PixelFormat,
    pub desktop_name: String,
    pub framebuffer: Vec<u8>,
}

const CANONICAL_DECODER_FORMAT: PixelFormat = PixelFormat {
    bits_per_pixel: 32,
    depth: 24,
    big_endian: false,
    true_colour: true,
    red_max: 255,
    green_max: 255,
    blue_max: 255,
    red_shift: 16,
    green_shift: 8,
    blue_shift: 0,
};

pub async fn read_server_init<S>(reader: &mut RfbReader<S>) -> Result<ServerInit, RfbError>
where
    S: AsyncRead + Unpin,
{
    let width = reader
        .read_u16()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let height = reader
        .read_u16()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let layout = validate_framebuffer_layout_for_phase(
        width,
        height,
        reader.limits(),
        RfbPhase::ServerInit,
    )?;

    let bits_per_pixel = reader
        .read_u8()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let depth = reader
        .read_u8()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let big_endian = reader
        .read_u8()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?
        != 0;
    let true_colour = reader
        .read_u8()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?
        != 0;
    let red_max = reader
        .read_u16()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let green_max = reader
        .read_u16()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let blue_max = reader
        .read_u16()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let red_shift = reader
        .read_u8()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let green_shift = reader
        .read_u8()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let blue_shift = reader
        .read_u8()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let mut padding = [0_u8; 3];
    reader
        .read_exact(&mut padding)
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;

    let pixel_format = PixelFormat {
        bits_per_pixel,
        depth,
        big_endian,
        true_colour,
        red_max,
        green_max,
        blue_max,
        red_shift,
        green_shift,
        blue_shift,
    };
    pixel_format.validate_for_phase(RfbPhase::ServerInit)?;

    let name_length = reader
        .read_u32()
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let name_bytes = reader
        .read_bounded_bytes(
            u64::from(name_length),
            u64::from(reader.limits().max_text_bytes),
            "desktop name",
            RfbPhase::ServerInit,
        )
        .await?;
    let desktop_name = sanitize_remote_text(&name_bytes, RfbPhase::ServerInit, "desktop name")?;
    let framebuffer = allocate_zeroed(
        layout.rgba_bytes,
        RfbPhase::ServerInit,
        "startup framebuffer",
    )?;

    Ok(ServerInit {
        width,
        height,
        pixel_format,
        desktop_name,
        framebuffer,
    })
}

fn decoder_pixel_format() -> &'static PixelFormat {
    &CANONICAL_DECODER_FORMAT
}

async fn configure_server<S>(reader: &mut RfbReader<S>) -> Result<ServerInit, RfbError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let init = read_server_init(reader).await?;
    send_set_pixel_format(reader).await?;
    send_set_encodings(reader).await?;
    Ok(init)
}

pub fn normalize_resize_request(
    backing_width: u32,
    backing_height: u32,
    limits: ProtocolLimits,
) -> Result<DesktopSize, RfbError> {
    limits.validate_for_phase(RfbPhase::Session)?;
    if backing_width < 640
        || backing_height < 480
        || backing_width > u32::from(limits.max_dimension)
        || backing_height > u32::from(limits.max_dimension)
    {
        return Err(RfbError::limit(
            RfbPhase::Session,
            "dynamic resize dimensions",
        ));
    }
    let backing_pixels = u64::from(backing_width)
        .checked_mul(u64::from(backing_height))
        .ok_or_else(|| RfbError::limit(RfbPhase::Session, "dynamic resize pixels"))?;
    if backing_pixels > limits.max_pixels {
        return Err(RfbError::limit(RfbPhase::Session, "dynamic resize pixels"));
    }

    let width = backing_width - backing_width % 8;
    let height = backing_height - backing_height % 8;
    let width = u16::try_from(width)
        .map_err(|_| RfbError::limit(RfbPhase::Session, "dynamic resize width"))?;
    let height = u16::try_from(height)
        .map_err(|_| RfbError::limit(RfbPhase::Session, "dynamic resize height"))?;
    if width < 640 || height < 480 {
        return Err(RfbError::limit(
            RfbPhase::Session,
            "dynamic resize dimensions",
        ));
    }
    validate_framebuffer_layout_for_phase(width, height, limits, RfbPhase::Session)?;
    Ok(DesktopSize::new(width, height))
}

pub fn encode_set_encodings() -> [u8; 32] {
    let encodings = [
        enc::ZRLE,
        enc::HEXTILE,
        enc::COPY_RECT,
        enc::RAW,
        enc::DESKTOP_SIZE,
        enc::EXTENDED_DESKTOP_SIZE,
        enc::TIGHT,
    ];
    let mut message = [0_u8; 32];
    message[0] = client_msg::SET_ENCODINGS;
    message[2..4].copy_from_slice(&(encodings.len() as u16).to_be_bytes());
    for (index, encoding) in encodings.iter().enumerate() {
        let start = 4 + index * 4;
        message[start..start + 4].copy_from_slice(&encoding.to_be_bytes());
    }
    message
}

pub fn encode_set_desktop_size(
    size: DesktopSize,
    limits: ProtocolLimits,
) -> Result<[u8; 24], RfbError> {
    if size.width < 640 || size.height < 480 {
        return Err(RfbError::limit(
            RfbPhase::Session,
            "dynamic resize dimensions",
        ));
    }
    validate_framebuffer_layout_for_phase(size.width, size.height, limits, RfbPhase::Session)?;

    let mut message = [0_u8; 24];
    message[0] = client_msg::SET_DESKTOP_SIZE;
    message[2..4].copy_from_slice(&size.width.to_be_bytes());
    message[4..6].copy_from_slice(&size.height.to_be_bytes());
    message[6] = 1;
    message[16..18].copy_from_slice(&size.width.to_be_bytes());
    message[18..20].copy_from_slice(&size.height.to_be_bytes());
    Ok(message)
}

pub fn parse_extended_desktop_size(
    reason: u16,
    result: u16,
    size: DesktopSize,
    payload: &[u8],
    limits: ProtocolLimits,
) -> Result<ExtendedDesktopSize, RfbError> {
    validate_framebuffer_layout_for_phase(size.width, size.height, limits, RfbPhase::Session)?;
    let header = payload.get(..4).ok_or_else(|| {
        RfbError::new(
            RfbPhase::Session,
            RfbErrorKind::Protocol,
            "extended desktop size header",
        )
    })?;
    if header[1..] != [0, 0, 0] {
        return Err(RfbError::new(
            RfbPhase::Session,
            RfbErrorKind::Protocol,
            "extended desktop size padding",
        ));
    }
    let screen_count = usize::from(header[0]);
    if screen_count == 0 {
        return Err(RfbError::new(
            RfbPhase::Session,
            RfbErrorKind::Protocol,
            "extended desktop size screens",
        ));
    }
    let expected = screen_count
        .checked_mul(16)
        .and_then(|bytes| bytes.checked_add(4))
        .ok_or_else(|| RfbError::limit(RfbPhase::Session, "extended desktop size screens"))?;
    if payload.len() != expected {
        return Err(RfbError::new(
            RfbPhase::Session,
            RfbErrorKind::Protocol,
            "extended desktop size payload",
        ));
    }
    if !matches!(reason, 0..=2)
        || (reason == 1 && !matches!(result, 0..=3))
        || (reason != 1 && result != 0)
    {
        return Err(RfbError::new(
            RfbPhase::Session,
            RfbErrorKind::Protocol,
            "extended desktop size reason/result",
        ));
    }

    let mut ids = Vec::new();
    ids.try_reserve_exact(screen_count)
        .map_err(|_| RfbError::allocation(RfbPhase::Session, "extended desktop screens"))?;
    let mut exact_single = false;
    for (index, screen) in payload[4..].chunks_exact(16).enumerate() {
        let id = u32::from_be_bytes(screen[0..4].try_into().map_err(|_| {
            RfbError::new(
                RfbPhase::Session,
                RfbErrorKind::Protocol,
                "extended desktop screen id",
            )
        })?);
        if ids.contains(&id) {
            return Err(RfbError::new(
                RfbPhase::Session,
                RfbErrorKind::Protocol,
                "extended desktop duplicate screen id",
            ));
        }
        ids.push(id);
        let x = u16::from_be_bytes(screen[4..6].try_into().map_err(|_| {
            RfbError::new(
                RfbPhase::Session,
                RfbErrorKind::Protocol,
                "extended desktop screen x",
            )
        })?);
        let y = u16::from_be_bytes(screen[6..8].try_into().map_err(|_| {
            RfbError::new(
                RfbPhase::Session,
                RfbErrorKind::Protocol,
                "extended desktop screen y",
            )
        })?);
        let width = u16::from_be_bytes(screen[8..10].try_into().map_err(|_| {
            RfbError::new(
                RfbPhase::Session,
                RfbErrorKind::Protocol,
                "extended desktop screen width",
            )
        })?);
        let height = u16::from_be_bytes(screen[10..12].try_into().map_err(|_| {
            RfbError::new(
                RfbPhase::Session,
                RfbErrorKind::Protocol,
                "extended desktop screen height",
            )
        })?);
        let flags = u32::from_be_bytes(screen[12..16].try_into().map_err(|_| {
            RfbError::new(
                RfbPhase::Session,
                RfbErrorKind::Protocol,
                "extended desktop screen flags",
            )
        })?);
        let right = x
            .checked_add(width)
            .ok_or_else(|| RfbError::limit(RfbPhase::Session, "extended desktop screen bounds"))?;
        let bottom = y
            .checked_add(height)
            .ok_or_else(|| RfbError::limit(RfbPhase::Session, "extended desktop screen bounds"))?;
        if width == 0 || height == 0 || right > size.width || bottom > size.height {
            return Err(RfbError::new(
                RfbPhase::Session,
                RfbErrorKind::Protocol,
                "extended desktop screen bounds",
            ));
        }
        if screen_count == 1 && (width != size.width || height != size.height) {
            return Err(RfbError::new(
                RfbPhase::Session,
                RfbErrorKind::Protocol,
                "extended desktop size contradiction",
            ));
        }
        exact_single = index == 0
            && screen_count == 1
            && id == 0
            && x == 0
            && y == 0
            && width == size.width
            && height == size.height
            && flags == 0;
    }

    if result == 1 || result == 2 {
        return Ok(ExtendedDesktopSize::Rejected);
    }
    if result == 3 {
        return Ok(ExtendedDesktopSize::Unsupported);
    }
    if !exact_single {
        return if reason == 1 {
            Ok(ExtendedDesktopSize::Unsupported)
        } else {
            Ok(ExtendedDesktopSize::ServerUnsupported(size))
        };
    }
    if reason == 1 {
        Ok(ExtendedDesktopSize::Pending(size))
    } else {
        Ok(ExtendedDesktopSize::ServerSize(size))
    }
}

#[derive(Clone, Copy)]
struct DirtyRegion {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl DirtyRegion {
    fn from_rect(rect: &FbRect, framebuffer: &Framebuffer) -> Result<Self, RfbError> {
        let region = Self::from_bounds(rect.x, rect.y, rect.w, rect.h, framebuffer)?;
        let expected = u64::from(rect.w)
            .checked_mul(u64::from(rect.h))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| RfbError::limit(RfbPhase::EventQueue, "dirty rectangle bytes"))?;
        if usize::try_from(expected).ok() != Some(rect.rgba.len()) {
            return Err(RfbError::new(
                RfbPhase::EventQueue,
                RfbErrorKind::Protocol,
                "dirty rectangle bytes",
            ));
        }
        Ok(region)
    }

    fn from_bounds(
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        framebuffer: &Framebuffer,
    ) -> Result<Self, RfbError> {
        let right = x
            .checked_add(width)
            .ok_or_else(|| RfbError::limit(RfbPhase::EventQueue, "dirty rectangle"))?;
        let bottom = y
            .checked_add(height)
            .ok_or_else(|| RfbError::limit(RfbPhase::EventQueue, "dirty rectangle"))?;
        if width == 0
            || height == 0
            || right > u32::from(framebuffer.width())
            || bottom > u32::from(framebuffer.height())
        {
            return Err(RfbError::new(
                RfbPhase::EventQueue,
                RfbErrorKind::Protocol,
                "dirty rectangle",
            ));
        }
        Ok(Self {
            left: x,
            top: y,
            right,
            bottom,
        })
    }

    fn include(&mut self, other: Self) {
        self.left = self.left.min(other.left);
        self.top = self.top.min(other.top);
        self.right = self.right.max(other.right);
        self.bottom = self.bottom.max(other.bottom);
    }
}

struct EventQueue {
    sender: Sender<VncEvent>,
    pending_dirty: Option<DirtyRegion>,
    limits: ProtocolLimits,
}

impl EventQueue {
    fn new(sender: Sender<VncEvent>, limits: ProtocolLimits) -> Self {
        Self {
            sender,
            pending_dirty: None,
            limits,
        }
    }

    fn queue_error(field: &'static str) -> RfbError {
        RfbError::new(RfbPhase::EventQueue, RfbErrorKind::Queue, field)
    }

    fn send_lossless(&self, event: VncEvent) -> Result<(), RfbError> {
        self.sender.try_send(event).map_err(|error| match error {
            TrySendError::Full(_) => Self::queue_error("event queue full"),
            TrySendError::Disconnected(_) => Self::queue_error("event queue disconnected"),
        })
    }

    fn coalesce(&mut self, framebuffer: &Framebuffer, rects: &[FbRect]) -> Result<(), RfbError> {
        for rect in rects {
            let next = DirtyRegion::from_rect(rect, framebuffer)?;
            if let Some(pending) = &mut self.pending_dirty {
                pending.include(next);
            } else {
                self.pending_dirty = Some(next);
            }
        }
        Ok(())
    }

    fn flush_pending(&mut self, framebuffer: &Framebuffer) -> Result<(), RfbError> {
        let Some(region) = self.pending_dirty else {
            return Ok(());
        };
        if self.sender.is_full() {
            return Ok(());
        }
        let rect = snapshot_rect(
            framebuffer,
            region.left,
            region.top,
            region.right - region.left,
            region.bottom - region.top,
            self.limits,
        )?;
        match self
            .sender
            .try_send(VncEvent::FramebufferRects(framebuffer_rects(rect)?))
        {
            Ok(()) => {
                self.pending_dirty = None;
                Ok(())
            }
            Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                Err(Self::queue_error("event queue disconnected"))
            }
        }
    }

    fn send_framebuffer(
        &mut self,
        framebuffer: &Framebuffer,
        rects: Vec<FbRect>,
    ) -> Result<(), RfbError> {
        self.flush_pending(framebuffer)?;
        if self.pending_dirty.is_some() {
            return self.coalesce(framebuffer, &rects);
        }
        match self.sender.try_send(VncEvent::FramebufferRects(rects)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(VncEvent::FramebufferRects(rects))) => {
                self.coalesce(framebuffer, &rects)
            }
            Err(TrySendError::Disconnected(_)) => {
                Err(Self::queue_error("event queue disconnected"))
            }
            Err(TrySendError::Full(_)) => Err(Self::queue_error("invalid framebuffer event")),
        }
    }
}

pub struct VncClient;

impl VncClient {
    pub async fn run(
        proxy: TrustedSshProxy,
        options: VncOptions,
        event_tx: Sender<VncEvent>,
        command_rx: Receiver<VncCommand>,
    ) -> Result<(), RfbError> {
        Self::run_inner(
            proxy,
            options,
            VncSessionChannels {
                event_tx,
                command_rx,
                clipboard: ClipboardSlot::default(),
            },
            false,
            std::future::pending(),
        )
        .await
    }

    pub(crate) async fn run_cancellable(
        proxy: TrustedSshProxy,
        options: VncOptions,
        channels: VncSessionChannels,
        clipboard_enabled: bool,
        cancelled: oneshot::Receiver<()>,
    ) -> Result<(), RfbError> {
        Self::run_inner(proxy, options, channels, clipboard_enabled, async move {
            let _ = cancelled.await;
        })
        .await
    }

    async fn run_inner<C>(
        proxy: TrustedSshProxy,
        options: VncOptions,
        channels: VncSessionChannels,
        clipboard_enabled: bool,
        cancelled: C,
    ) -> Result<(), RfbError>
    where
        C: std::future::Future<Output = ()>,
    {
        let (stream, ticket) = proxy.into_parts();
        let limits = options.limits;
        let VncSessionChannels {
            event_tx,
            command_rx,
            clipboard,
        } = channels;
        let mut reader = RfbReader::new(stream, limits);
        tokio::pin!(cancelled);
        let result = {
            let connected = run_connected(
                &mut reader,
                ticket,
                options,
                event_tx,
                command_rx,
                clipboard,
                clipboard_enabled,
            );
            tokio::pin!(connected);
            tokio::select! {
                biased;
                result = &mut connected => result,
                () = &mut cancelled => Ok(()),
            }
        };
        finish_session(reader, result).await
    }

    #[cfg(test)]
    async fn run_test_stream<S>(
        stream: S,
        ticket: ProxyTicket,
        options: VncOptions,
        channels: VncSessionChannels,
        clipboard_enabled: bool,
        cancelled: oneshot::Receiver<()>,
    ) -> Result<(), RfbError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let limits = options.limits;
        let VncSessionChannels {
            event_tx,
            command_rx,
            clipboard,
        } = channels;
        let mut reader = RfbReader::new(stream, limits);
        let cancelled = async move {
            let _ = cancelled.await;
        };
        tokio::pin!(cancelled);
        let result = {
            let connected = run_connected(
                &mut reader,
                ticket,
                options,
                event_tx,
                command_rx,
                clipboard,
                clipboard_enabled,
            );
            tokio::pin!(connected);
            tokio::select! {
                biased;
                result = &mut connected => result,
                () = &mut cancelled => Ok(()),
            }
        };
        finish_session(reader, result).await
    }
}

async fn run_connected<S>(
    reader: &mut RfbReader<S>,
    ticket: ProxyTicket,
    options: VncOptions,
    event_tx: Sender<VncEvent>,
    command_rx: Receiver<VncCommand>,
    clipboard: ClipboardSlot,
    clipboard_enabled: bool,
) -> Result<(), RfbError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    options.limits.validate_for_phase(RfbPhase::ServerInit)?;
    if event_tx.capacity() != Some(VNC_QUEUE_CAPACITY)
        || command_rx.capacity() != Some(VNC_QUEUE_CAPACITY)
    {
        return Err(RfbError::new(
            RfbPhase::EventQueue,
            RfbErrorKind::Queue,
            "queue capacity",
        ));
    }

    let limits = options.limits;
    let version = negotiate_version(reader).await?;
    negotiate_security(reader, ticket, version).await?;

    reader
        .write_u8(u8::from(options.shared))
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
    let init = configure_server(reader).await?;
    debug!(
        width = init.width,
        height = init.height,
        "RFB ServerInit accepted"
    );

    let ServerInit {
        width,
        height,
        pixel_format: _,
        desktop_name,
        framebuffer: pixels,
    } = init;
    let mut framebuffer = Framebuffer::from_pixels(width, height, limits, pixels)
        .map_err(encoding::map_framebuffer_error)?;
    let mut events = EventQueue::new(event_tx, limits);
    events.send_lossless(VncEvent::DesktopSize(DesktopSize::new(
        framebuffer.width(),
        framebuffer.height(),
    )))?;
    events.send_lossless(VncEvent::DesktopName(desktop_name))?;

    send_fb_update_request(reader, false, 0, 0, width, height).await?;

    run_session(
        reader,
        &mut framebuffer,
        &mut events,
        &command_rx,
        limits,
        &clipboard,
        clipboard_enabled,
    )
    .await
}

async fn finish_session<S>(
    reader: RfbReader<S>,
    result: Result<(), RfbError>,
) -> Result<(), RfbError>
where
    S: AsyncWrite + Unpin,
{
    let mut stream = reader.into_inner();
    match stream.shutdown().await {
        Ok(()) => result,
        Err(source) => match result {
            Ok(()) => Err(RfbError::io(RfbPhase::Cleanup, source)),
            Err(primary) => Err(primary.with_cleanup_failure(source)),
        },
    }
}

async fn run_session<S>(
    reader: &mut RfbReader<S>,
    framebuffer: &mut Framebuffer,
    events: &mut EventQueue,
    command_rx: &Receiver<VncCommand>,
    limits: ProtocolLimits,
    clipboard: &ClipboardSlot,
    clipboard_enabled: bool,
) -> Result<(), RfbError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let pixel_format = decoder_pixel_format();
    let mut zrle_state = ZrleState::new();
    let mut tight_state = TightState::new();

    loop {
        events.flush_pending(framebuffer)?;
        loop {
            match command_rx.try_recv() {
                Ok(VncCommand::Disconnect) => {
                    events.flush_pending(framebuffer)?;
                    events.send_lossless(VncEvent::Disconnected)?;
                    return Ok(());
                }
                Ok(VncCommand::KeyEvent { down, keysym }) => {
                    send_key_event(reader, down, keysym).await?;
                }
                Ok(VncCommand::PointerEvent { buttons, x, y }) => {
                    send_pointer_event(reader, buttons, x, y).await?;
                }
                Ok(VncCommand::SetClipboard(text)) => {
                    send_client_cut_text(reader, &text, limits).await?;
                }
                Ok(VncCommand::SetDesktopSize(size)) => {
                    send_set_desktop_size(reader, size, limits).await?;
                }
                Ok(VncCommand::GracefulDisconnect(barrier)) => {
                    reader
                        .flush()
                        .await
                        .map_err(|source| RfbError::io(RfbPhase::Cleanup, source))?;
                    barrier.acknowledge();
                    return Ok(());
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(EventQueue::queue_error("command queue disconnected"));
                }
            }
        }

        let message_type =
            match tokio::time::timeout(Duration::from_millis(5), reader.read_u8()).await {
                Ok(result) => result.map_err(|source| RfbError::io(RfbPhase::Session, source))?,
                Err(_) => continue,
            };

        match message_type {
            server_msg::FB_UPDATE => {
                reader
                    .read_u8()
                    .await
                    .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                let rectangle_count = reader
                    .read_u16()
                    .await
                    .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                if rectangle_count > limits.max_rectangles {
                    return Err(RfbError::limit(
                        RfbPhase::Session,
                        "framebuffer rectangle count",
                    ));
                }
                let mut dirty = None;

                for _ in 0..rectangle_count {
                    let x = reader
                        .read_u16()
                        .await
                        .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                    let y = reader
                        .read_u16()
                        .await
                        .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                    let width = reader
                        .read_u16()
                        .await
                        .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                    let height = reader
                        .read_u16()
                        .await
                        .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                    let encoding = reader
                        .read_i32()
                        .await
                        .map_err(|source| RfbError::io(RfbPhase::Session, source))?;

                    match encoding {
                        enc::RAW => {
                            raw::decode(reader, framebuffer, pixel_format, x, y, width, height)
                                .await?;
                            include_dirty(&mut dirty, framebuffer, x, y, width, height)?;
                        }
                        enc::COPY_RECT => {
                            copyrect::decode(reader, framebuffer, x, y, width, height).await?;
                            include_dirty(&mut dirty, framebuffer, x, y, width, height)?;
                        }
                        enc::ZRLE => {
                            zrle::decode(
                                reader,
                                framebuffer,
                                pixel_format,
                                x,
                                y,
                                width,
                                height,
                                &mut zrle_state,
                            )
                            .await?;
                            include_dirty(&mut dirty, framebuffer, x, y, width, height)?;
                        }
                        enc::HEXTILE => {
                            hextile::decode(reader, framebuffer, pixel_format, x, y, width, height)
                                .await?;
                            include_dirty(&mut dirty, framebuffer, x, y, width, height)?;
                        }
                        enc::TIGHT => {
                            tight::decode(
                                reader,
                                framebuffer,
                                pixel_format,
                                x,
                                y,
                                width,
                                height,
                                &mut tight_state,
                            )
                            .await?;
                            include_dirty(&mut dirty, framebuffer, x, y, width, height)?;
                        }
                        enc::DESKTOP_SIZE => {
                            if let Some(region) = dirty.take() {
                                let rect = snapshot_rect(
                                    framebuffer,
                                    region.left,
                                    region.top,
                                    region.right - region.left,
                                    region.bottom - region.top,
                                    limits,
                                )?;
                                events.send_framebuffer(framebuffer, framebuffer_rects(rect)?)?;
                            }
                            events.flush_pending(framebuffer)?;
                            encoding::decode_desktop_size(framebuffer, width, height)?;
                            events.send_lossless(VncEvent::DesktopSize(DesktopSize::new(
                                framebuffer.width(),
                                framebuffer.height(),
                            )))?;
                            send_fb_update_request(reader, false, 0, 0, width, height).await?;
                        }
                        enc::EXTENDED_DESKTOP_SIZE => {
                            let mut payload_header = [0_u8; 4];
                            reader
                                .read_exact(&mut payload_header)
                                .await
                                .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                            let screen_bytes = u64::from(payload_header[0])
                                .checked_mul(16)
                                .ok_or_else(|| {
                                    RfbError::limit(RfbPhase::Session, "extended desktop screens")
                                })?;
                            let screens = reader
                                .read_bounded_bytes(
                                    screen_bytes,
                                    u64::from(u8::MAX) * 16,
                                    "extended desktop screens",
                                    RfbPhase::Session,
                                )
                                .await?;
                            let payload_len =
                                4_usize.checked_add(screens.len()).ok_or_else(|| {
                                    RfbError::limit(RfbPhase::Session, "extended desktop payload")
                                })?;
                            let mut payload = Vec::new();
                            payload.try_reserve_exact(payload_len).map_err(|_| {
                                RfbError::allocation(RfbPhase::Session, "extended desktop payload")
                            })?;
                            payload.extend_from_slice(&payload_header);
                            payload.extend_from_slice(&screens);
                            let size = DesktopSize::new(width, height);
                            match parse_extended_desktop_size(x, y, size, &payload, limits)? {
                                ExtendedDesktopSize::ServerSize(size) => {
                                    if let Some(region) = dirty.take() {
                                        let rect = snapshot_rect(
                                            framebuffer,
                                            region.left,
                                            region.top,
                                            region.right - region.left,
                                            region.bottom - region.top,
                                            limits,
                                        )?;
                                        events.send_framebuffer(
                                            framebuffer,
                                            framebuffer_rects(rect)?,
                                        )?;
                                    }
                                    events.flush_pending(framebuffer)?;
                                    encoding::decode_desktop_size(
                                        framebuffer,
                                        size.width,
                                        size.height,
                                    )?;
                                    events.send_lossless(VncEvent::DesktopSize(size))?;
                                    send_fb_update_request(
                                        reader,
                                        false,
                                        0,
                                        0,
                                        size.width,
                                        size.height,
                                    )
                                    .await?;
                                }
                                ExtendedDesktopSize::ServerUnsupported(size) => {
                                    if let Some(region) = dirty.take() {
                                        let rect = snapshot_rect(
                                            framebuffer,
                                            region.left,
                                            region.top,
                                            region.right - region.left,
                                            region.bottom - region.top,
                                            limits,
                                        )?;
                                        events.send_framebuffer(
                                            framebuffer,
                                            framebuffer_rects(rect)?,
                                        )?;
                                    }
                                    events.flush_pending(framebuffer)?;
                                    encoding::decode_desktop_size(
                                        framebuffer,
                                        size.width,
                                        size.height,
                                    )?;
                                    events.send_lossless(VncEvent::DesktopSize(size))?;
                                    events.send_lossless(VncEvent::ResizeOutcome(
                                        ResizeProtocolOutcome::ServerUnsupported,
                                    ))?;
                                    send_fb_update_request(
                                        reader,
                                        false,
                                        0,
                                        0,
                                        size.width,
                                        size.height,
                                    )
                                    .await?;
                                }
                                ExtendedDesktopSize::Pending(size) => {
                                    events.send_lossless(VncEvent::ResizeOutcome(
                                        ResizeProtocolOutcome::Forwarded(size),
                                    ))?;
                                }
                                ExtendedDesktopSize::Rejected => {
                                    events.send_lossless(VncEvent::ResizeOutcome(
                                        ResizeProtocolOutcome::Rejected,
                                    ))?;
                                }
                                ExtendedDesktopSize::Unsupported => {
                                    events.send_lossless(VncEvent::ResizeOutcome(
                                        ResizeProtocolOutcome::Unsupported,
                                    ))?;
                                }
                            }
                        }
                        enc::CURSOR => {
                            encoding::decode_cursor(reader, pixel_format, x, y, width, height)
                                .await?;
                        }
                        _ => {
                            return Err(RfbError::new(
                                RfbPhase::Session,
                                RfbErrorKind::Protocol,
                                "unsupported encoding",
                            ));
                        }
                    }
                }

                if let Some(region) = dirty {
                    let rect = snapshot_rect(
                        framebuffer,
                        region.left,
                        region.top,
                        region.right - region.left,
                        region.bottom - region.top,
                        limits,
                    )?;
                    events.send_framebuffer(framebuffer, framebuffer_rects(rect)?)?;
                }

                let width = framebuffer.width();
                let height = framebuffer.height();
                send_fb_update_request(reader, true, 0, 0, width, height).await?;
            }
            server_msg::SET_COLOUR_MAP_ENTRIES => {
                reader
                    .read_u8()
                    .await
                    .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                reader
                    .read_u16()
                    .await
                    .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                let count = reader
                    .read_u16()
                    .await
                    .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                let declared = u64::from(count)
                    .checked_mul(6)
                    .ok_or_else(|| RfbError::limit(RfbPhase::Session, "colour map"))?;
                let maximum = u64::from(u16::MAX) * 6;
                let bytes = reader
                    .read_bounded_bytes(declared, maximum, "colour map", RfbPhase::Session)
                    .await?;
                drop(bytes);
            }
            server_msg::BELL => debug!("RFB bell"),
            server_msg::SERVER_CUT_TEXT => {
                let mut padding = [0_u8; 3];
                reader
                    .read_exact(&mut padding)
                    .await
                    .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                let declared = reader
                    .read_u32()
                    .await
                    .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
                let text = reader
                    .read_bounded_bytes(
                        u64::from(declared),
                        u64::from(limits.max_clipboard_bytes),
                        "server clipboard",
                        RfbPhase::Session,
                    )
                    .await?;
                let text = ClipboardText::try_from(text).map_err(|_| {
                    RfbError::new(
                        RfbPhase::Session,
                        RfbErrorKind::Protocol,
                        "server clipboard UTF-8",
                    )
                })?;
                if clipboard_enabled {
                    clipboard.replace(text);
                }
            }
            _ => {
                return Err(RfbError::new(
                    RfbPhase::Session,
                    RfbErrorKind::Protocol,
                    "server message type",
                ));
            }
        }
    }
}

fn include_dirty(
    dirty: &mut Option<DirtyRegion>,
    framebuffer: &Framebuffer,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> Result<(), RfbError> {
    let next = DirtyRegion::from_bounds(
        u32::from(x),
        u32::from(y),
        u32::from(width),
        u32::from(height),
        framebuffer,
    )?;
    if let Some(region) = dirty {
        region.include(next);
    } else {
        *dirty = Some(next);
    }
    Ok(())
}

fn framebuffer_rects(rect: FbRect) -> Result<Vec<FbRect>, RfbError> {
    let mut rects = Vec::new();
    rects
        .try_reserve_exact(1)
        .map_err(|_| RfbError::allocation(RfbPhase::EventQueue, "framebuffer event"))?;
    rects.push(rect);
    Ok(rects)
}

fn snapshot_rect(
    framebuffer: &Framebuffer,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    limits: ProtocolLimits,
) -> Result<FbRect, RfbError> {
    let right = x
        .checked_add(width)
        .ok_or_else(|| RfbError::limit(RfbPhase::EventQueue, "framebuffer event bounds"))?;
    let bottom = y
        .checked_add(height)
        .ok_or_else(|| RfbError::limit(RfbPhase::EventQueue, "framebuffer event bounds"))?;
    if width == 0
        || height == 0
        || right > u32::from(framebuffer.width())
        || bottom > u32::from(framebuffer.height())
    {
        return Err(RfbError::new(
            RfbPhase::EventQueue,
            RfbErrorKind::Protocol,
            "framebuffer event bounds",
        ));
    }

    let byte_count = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| RfbError::limit(RfbPhase::EventQueue, "framebuffer event bytes"))?;
    if byte_count > limits.max_framebuffer_bytes {
        return Err(RfbError::limit(
            RfbPhase::EventQueue,
            "framebuffer event bytes",
        ));
    }
    let x = u16::try_from(x)
        .map_err(|_| RfbError::limit(RfbPhase::EventQueue, "framebuffer event x"))?;
    let y = u16::try_from(y)
        .map_err(|_| RfbError::limit(RfbPhase::EventQueue, "framebuffer event y"))?;
    let width_u16 = u16::try_from(width)
        .map_err(|_| RfbError::limit(RfbPhase::EventQueue, "framebuffer event width"))?;
    let height_u16 = u16::try_from(height)
        .map_err(|_| RfbError::limit(RfbPhase::EventQueue, "framebuffer event height"))?;
    let rectangle = CheckedRect::new(
        x,
        y,
        width_u16,
        height_u16,
        framebuffer.width(),
        framebuffer.height(),
    )
    .map_err(|_| {
        RfbError::new(
            RfbPhase::EventQueue,
            RfbErrorKind::Protocol,
            "framebuffer event bounds",
        )
    })?;
    let rgba = framebuffer
        .snapshot(rectangle)
        .map_err(|error| match error.kind() {
            RfbErrorKind::Allocation => {
                RfbError::allocation(RfbPhase::EventQueue, "framebuffer event")
            }
            RfbErrorKind::Limit => RfbError::limit(RfbPhase::EventQueue, "framebuffer event bytes"),
            _ => RfbError::new(
                RfbPhase::EventQueue,
                RfbErrorKind::Protocol,
                "framebuffer event bytes",
            ),
        })?;

    Ok(FbRect {
        x: u32::from(x),
        y: u32::from(y),
        w: width,
        h: height,
        rgba,
    })
}

async fn send_set_pixel_format<S>(reader: &mut RfbReader<S>) -> Result<(), RfbError>
where
    S: AsyncWrite + Unpin,
{
    let pixel_format = decoder_pixel_format();
    let mut message = [0_u8; 20];
    message[0] = client_msg::SET_PIXEL_FORMAT;
    message[4] = pixel_format.bits_per_pixel;
    message[5] = pixel_format.depth;
    message[6] = u8::from(pixel_format.big_endian);
    message[7] = u8::from(pixel_format.true_colour);
    message[8..10].copy_from_slice(&pixel_format.red_max.to_be_bytes());
    message[10..12].copy_from_slice(&pixel_format.green_max.to_be_bytes());
    message[12..14].copy_from_slice(&pixel_format.blue_max.to_be_bytes());
    message[14] = pixel_format.red_shift;
    message[15] = pixel_format.green_shift;
    message[16] = pixel_format.blue_shift;
    reader
        .write_all(&message)
        .await
        .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))
}

async fn send_set_encodings<S>(reader: &mut RfbReader<S>) -> Result<(), RfbError>
where
    S: AsyncWrite + Unpin,
{
    let message = encode_set_encodings();
    reader
        .write_all(&message)
        .await
        .map_err(|source| RfbError::io(RfbPhase::Session, source))
}

async fn send_set_desktop_size<S>(
    reader: &mut RfbReader<S>,
    size: DesktopSize,
    limits: ProtocolLimits,
) -> Result<(), RfbError>
where
    S: AsyncWrite + Unpin,
{
    let message = encode_set_desktop_size(size, limits)?;
    reader
        .write_all(&message)
        .await
        .map_err(|source| RfbError::io(RfbPhase::Session, source))
}

async fn send_fb_update_request<S>(
    reader: &mut RfbReader<S>,
    incremental: bool,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> Result<(), RfbError>
where
    S: AsyncWrite + Unpin,
{
    let mut message = [0_u8; 10];
    message[0] = client_msg::FB_UPDATE_REQUEST;
    message[1] = u8::from(incremental);
    message[2..4].copy_from_slice(&x.to_be_bytes());
    message[4..6].copy_from_slice(&y.to_be_bytes());
    message[6..8].copy_from_slice(&width.to_be_bytes());
    message[8..10].copy_from_slice(&height.to_be_bytes());
    reader
        .write_all(&message)
        .await
        .map_err(|source| RfbError::io(RfbPhase::Session, source))
}

async fn send_key_event<S>(
    reader: &mut RfbReader<S>,
    down: bool,
    keysym: u32,
) -> Result<(), RfbError>
where
    S: AsyncWrite + Unpin,
{
    let mut message = [0_u8; 8];
    message[0] = client_msg::KEY_EVENT;
    message[1] = u8::from(down);
    message[4..8].copy_from_slice(&keysym.to_be_bytes());
    reader
        .write_all(&message)
        .await
        .map_err(|source| RfbError::io(RfbPhase::Session, source))
}

async fn send_pointer_event<S>(
    reader: &mut RfbReader<S>,
    buttons: u8,
    x: u16,
    y: u16,
) -> Result<(), RfbError>
where
    S: AsyncWrite + Unpin,
{
    let mut message = [0_u8; 6];
    message[0] = client_msg::POINTER_EVENT;
    message[1] = buttons;
    message[2..4].copy_from_slice(&x.to_be_bytes());
    message[4..6].copy_from_slice(&y.to_be_bytes());
    reader
        .write_all(&message)
        .await
        .map_err(|source| RfbError::io(RfbPhase::Session, source))
}

async fn send_client_cut_text<S>(
    reader: &mut RfbReader<S>,
    text: &str,
    limits: ProtocolLimits,
) -> Result<(), RfbError>
where
    S: AsyncWrite + Unpin,
{
    let length = u32::try_from(text.len())
        .map_err(|_| RfbError::limit(RfbPhase::Session, "client clipboard"))?;
    if length > limits.max_clipboard_bytes {
        return Err(RfbError::limit(RfbPhase::Session, "client clipboard"));
    }
    let mut header = [0_u8; 8];
    header[0] = client_msg::CLIENT_CUT_TEXT;
    header[4..8].copy_from_slice(&length.to_be_bytes());
    reader
        .write_all(&header)
        .await
        .map_err(|source| RfbError::io(RfbPhase::Session, source))?;
    reader
        .write_all(text.as_bytes())
        .await
        .map_err(|source| RfbError::io(RfbPhase::Session, source))
}

#[cfg(test)]
#[path = "../../tests/support/rfb_peer.rs"]
mod test_rfb_peer;

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
    };

    use crossbeam_channel::bounded;
    use tokio::{
        io::{duplex, AsyncReadExt, AsyncWrite, AsyncWriteExt},
        sync::oneshot,
        time::{sleep, timeout, Duration},
    };

    use super::{
        configure_server, decoder_pixel_format, finish_session, framebuffer_rects, run_session,
        test_rfb_peer as rfb_peer, tight, EventQueue, TightState,
    };
    use crate::{
        connection::{
            bounded_vnc_channels, ClipboardSlot, DesktopSize, FbRect, ResizeProtocolOutcome,
            VncCommand, VncEvent,
        },
        ssh::ProxyTicket,
        vnc::{
            messages::{encoding as enc, server_msg},
            CheckedRect, Framebuffer, InputController, ProtocolLimits, RfbError, RfbErrorKind,
            RfbPhase, RfbReader, VncClient, VncOptions, CLIPBOARD_TEXT_LIMIT,
        },
    };

    const CANONICAL_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];
    const RGB565_16_FORMAT: [u8; 16] = [16, 16, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0];
    const RGB565_32_FORMAT: [u8; 16] = [32, 16, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0];
    const CANONICAL_SET_PIXEL_FORMAT: [u8; 20] = [
        0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0,
    ];
    const SET_ENCODINGS: [u8; 32] = [
        2, 0, 0, 7, 0, 0, 0, 16, 0, 0, 0, 5, 0, 0, 0, 1, 0, 0, 0, 0, 255, 255, 255, 33, 255, 255,
        254, 204, 0, 0, 0, 7,
    ];

    fn server_init_bytes(pixel_format: [u8; 16]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&pixel_format);
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes
    }

    async fn configure_fixture(pixel_format: [u8; 16]) -> ([u8; 52], super::ServerInit) {
        let (client, mut peer) = duplex(256);
        peer.write_all(&server_init_bytes(pixel_format))
            .await
            .unwrap();
        let mut reader = RfbReader::new(client, ProtocolLimits::default());
        let init = configure_server(&mut reader).await.unwrap();
        let mut outbound = [0_u8; 52];
        peer.read_exact(&mut outbound).await.unwrap();
        (outbound, init)
    }

    fn one_pixel_rect(x: u32, y: u32, rgba: [u8; 4]) -> FbRect {
        FbRect {
            x,
            y,
            w: 1,
            h: 1,
            rgba: rgba.to_vec(),
        }
    }

    fn push_rectangle_header(
        wire: &mut Vec<u8>,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        encoding: i32,
    ) {
        wire.extend_from_slice(&x.to_be_bytes());
        wire.extend_from_slice(&y.to_be_bytes());
        wire.extend_from_slice(&width.to_be_bytes());
        wire.extend_from_slice(&height.to_be_bytes());
        wire.extend_from_slice(&encoding.to_be_bytes());
    }

    fn push_one_screen_extended_size(
        wire: &mut Vec<u8>,
        reason: u16,
        result: u16,
        width: u16,
        height: u16,
    ) {
        push_extended_size(
            wire,
            reason,
            result,
            width,
            height,
            &[(0, 0, 0, width, height, 0)],
        );
    }

    fn push_extended_size(
        wire: &mut Vec<u8>,
        reason: u16,
        result: u16,
        width: u16,
        height: u16,
        screens: &[(u32, u16, u16, u16, u16, u32)],
    ) {
        push_rectangle_header(
            wire,
            reason,
            result,
            width,
            height,
            enc::EXTENDED_DESKTOP_SIZE,
        );
        wire.extend_from_slice(&[screens.len() as u8, 0, 0, 0]);
        for (id, x, y, screen_width, screen_height, flags) in screens {
            wire.extend_from_slice(&id.to_be_bytes());
            wire.extend_from_slice(&x.to_be_bytes());
            wire.extend_from_slice(&y.to_be_bytes());
            wire.extend_from_slice(&screen_width.to_be_bytes());
            wire.extend_from_slice(&screen_height.to_be_bytes());
            wire.extend_from_slice(&flags.to_be_bytes());
        }
    }

    #[tokio::test]
    async fn terminal_protocol_error_still_shuts_down_the_owned_transport() {
        let (client, mut peer) = duplex(16);
        let reader = RfbReader::new(client, ProtocolLimits::default());
        let primary = RfbError::new(
            RfbPhase::Authentication,
            RfbErrorKind::SecurityFailure,
            "synthetic authentication",
        );

        let returned = finish_session(reader, Err(primary.clone()))
            .await
            .unwrap_err();
        assert_eq!(returned, primary);

        let mut byte = [0_u8; 1];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
    }

    struct ShutdownFailure;

    impl AsyncWrite for ShutdownFailure {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "synthetic shutdown failure",
            )))
        }
    }

    #[tokio::test]
    async fn cleanup_failure_preserves_the_primary_protocol_classification() {
        let reader = RfbReader::new(ShutdownFailure, ProtocolLimits::default());
        let primary = RfbError::new(
            RfbPhase::Authentication,
            RfbErrorKind::SecurityFailure,
            "synthetic authentication",
        );

        let returned = finish_session(reader, Err(primary)).await.unwrap_err();
        assert_eq!(returned.phase(), RfbPhase::Authentication);
        assert_eq!(returned.kind(), RfbErrorKind::SecurityFailure);
        assert!(returned.has_cleanup_failure());
    }

    #[tokio::test]
    async fn malformed_remote_clipboard_utf8_fails_closed_before_event_delivery() {
        let mut message = vec![server_msg::SERVER_CUT_TEXT, 0, 0, 0];
        message.extend_from_slice(&2_u32.to_be_bytes());
        message.extend_from_slice(&[0x66, 0x80]);

        let (client, mut peer) = duplex(64);
        peer.write_all(&message).await.unwrap();
        peer.shutdown().await.unwrap();
        let limits = ProtocolLimits::default();
        let mut reader = RfbReader::new(client, limits);
        let mut framebuffer = Framebuffer::new(1, 1, limits).unwrap();
        let (event_tx, event_rx) = bounded(8);
        let mut events = EventQueue::new(event_tx, limits);
        let (_command_tx, command_rx) = bounded::<VncCommand>(1);
        let clipboard = ClipboardSlot::default();

        let error = run_session(
            &mut reader,
            &mut framebuffer,
            &mut events,
            &command_rx,
            limits,
            &clipboard,
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(error.phase(), RfbPhase::Session);
        assert_eq!(error.kind(), RfbErrorKind::Protocol);
        assert!(event_rx.try_recv().is_err());
        assert_eq!(clipboard.retained_count(), 0);
    }

    #[tokio::test]
    async fn clipboard_pressure_replaces_one_private_slot_without_event_payloads() {
        const MESSAGE_COUNT: usize = 257;
        const MESSAGE_BYTES: usize = 1_048_576;
        const MARKER_BYTES: usize = 8;

        let (client, mut peer) = duplex(64 * 1024);
        let producer = tokio::spawn(async move {
            for index in 0..MESSAGE_COUNT {
                let mut text = vec![b'x'; MESSAGE_BYTES];
                let marker = format!("{index:0MARKER_BYTES$}");
                text[MESSAGE_BYTES - MARKER_BYTES..].copy_from_slice(marker.as_bytes());
                assert_eq!(text.len(), MESSAGE_BYTES);
                peer.write_all(&[server_msg::SERVER_CUT_TEXT, 0, 0, 0])
                    .await
                    .unwrap();
                peer.write_all(&(MESSAGE_BYTES as u32).to_be_bytes())
                    .await
                    .unwrap();
                peer.write_all(&text).await.unwrap();
            }
            peer.shutdown().await.unwrap();
        });
        let limits = ProtocolLimits::default();
        let mut reader = RfbReader::new(client, limits);
        let mut framebuffer = Framebuffer::new(1, 1, limits).unwrap();
        let (connection, channels) = bounded_vnc_channels();
        let mut events = EventQueue::new(channels.event_tx, limits);

        let error = run_session(
            &mut reader,
            &mut framebuffer,
            &mut events,
            &channels.command_rx,
            limits,
            &channels.clipboard,
            true,
        )
        .await
        .unwrap_err();
        producer.await.unwrap();

        assert_eq!(error.kind(), RfbErrorKind::Io);
        assert_eq!(error.io_kind(), Some(io::ErrorKind::UnexpectedEof));
        assert!(connection.event_rx.try_recv().is_err());
        assert_eq!(channels.clipboard.retained_count(), 1);
        let latest = channels.clipboard.take().unwrap();
        assert_eq!(latest.as_str().len(), MESSAGE_BYTES);
        assert_eq!(
            &latest.as_str().as_bytes()[MESSAGE_BYTES - MARKER_BYTES..],
            b"00000256"
        );
        assert_eq!(channels.clipboard.retained_count(), 0);
    }

    #[tokio::test]
    async fn disabled_clipboard_pressure_validates_and_retains_nothing() {
        const MESSAGE_COUNT: usize = 257;
        const MESSAGE_BYTES: usize = 8;

        let (client, mut peer) = duplex(128);
        let producer = tokio::spawn(async move {
            for index in 0..MESSAGE_COUNT {
                let text = format!("{index:0MESSAGE_BYTES$}");
                peer.write_all(&[server_msg::SERVER_CUT_TEXT, 0, 0, 0])
                    .await
                    .unwrap();
                peer.write_all(&(MESSAGE_BYTES as u32).to_be_bytes())
                    .await
                    .unwrap();
                peer.write_all(text.as_bytes()).await.unwrap();
            }
            peer.shutdown().await.unwrap();
        });
        let limits = ProtocolLimits {
            max_clipboard_bytes: MESSAGE_BYTES as u32,
            ..ProtocolLimits::default()
        };
        let mut reader = RfbReader::new(client, limits);
        let mut framebuffer = Framebuffer::new(1, 1, limits).unwrap();
        let (connection, channels) = bounded_vnc_channels();
        let mut events = EventQueue::new(channels.event_tx, limits);

        let error = run_session(
            &mut reader,
            &mut framebuffer,
            &mut events,
            &channels.command_rx,
            limits,
            &channels.clipboard,
            false,
        )
        .await
        .unwrap_err();
        producer.await.unwrap();

        assert_eq!(error.kind(), RfbErrorKind::Io);
        assert_eq!(error.io_kind(), Some(io::ErrorKind::UnexpectedEof));
        assert!(connection.event_rx.try_recv().is_err());
        assert_eq!(channels.clipboard.retained_count(), 0);
    }

    #[tokio::test]
    async fn graceful_close_acknowledges_only_after_ordered_key_bytes_are_written() {
        let (client, mut peer) = duplex(64);
        let limits = ProtocolLimits::default();
        let mut reader = RfbReader::new(client, limits);
        let mut framebuffer = Framebuffer::new(1, 1, limits).unwrap();
        let (connection, channels) = bounded_vnc_channels();
        let mut events = EventQueue::new(channels.event_tx, limits);

        connection.send_key(true, 0xffe3).unwrap();
        connection.send_key(false, 0xffe3).unwrap();
        let acknowledged = connection.begin_graceful_close().unwrap();

        let session = run_session(
            &mut reader,
            &mut framebuffer,
            &mut events,
            &channels.command_rx,
            limits,
            &channels.clipboard,
            false,
        );
        let (result, wire) = tokio::join!(session, async {
            let mut wire = [0_u8; 16];
            peer.read_exact(&mut wire).await.unwrap();
            wire
        });

        result.unwrap();
        acknowledged.await.unwrap();
        assert_eq!(
            wire,
            [
                4, 1, 0, 0, 0, 0, 0xff, 0xe3, // Control_L down
                4, 0, 0, 0, 0, 0, 0xff, 0xe3, // Control_L up
            ]
        );
    }

    #[tokio::test]
    async fn valid_native_formats_emit_canonical_pixel_format_before_encodings() {
        for format in [CANONICAL_FORMAT, RGB565_16_FORMAT, RGB565_32_FORMAT] {
            let (outbound, init) = configure_fixture(format).await;
            assert_eq!(&outbound[..20], &CANONICAL_SET_PIXEL_FORMAT);
            assert_eq!(&outbound[20..], &SET_ENCODINGS);
            assert_eq!(init.pixel_format.bits_per_pixel, format[0]);
            assert_eq!(init.pixel_format.depth, format[1]);
        }
    }

    #[tokio::test]
    async fn rgb565_native_format_cannot_select_four_byte_tight_framing() {
        let (_outbound, init) = configure_fixture(RGB565_32_FORMAT).await;
        assert_eq!(init.pixel_format.depth, 16);

        let (mut peer, client) = duplex(16);
        peer.write_all(&[0x80, 0x11, 0x22, 0x33]).await.unwrap();
        peer.shutdown().await.unwrap();
        let mut framebuffer = Framebuffer::new(1, 1, ProtocolLimits::default()).unwrap();
        let mut reader = RfbReader::new(client, ProtocolLimits::default());
        tight::decode(
            &mut reader,
            &mut framebuffer,
            decoder_pixel_format(),
            0,
            0,
            1,
            1,
            &mut TightState::new(),
        )
        .await
        .unwrap();
        assert_eq!(framebuffer.pixels(), [0x11, 0x22, 0x33, 0xff]);
    }

    #[tokio::test]
    async fn framebuffer_update_orders_old_dirty_resize_new_dirty_and_refresh_requests() {
        let mut update = vec![server_msg::FB_UPDATE, 0];
        update.extend_from_slice(&3_u16.to_be_bytes());
        push_rectangle_header(&mut update, 0, 0, 2, 1, enc::RAW);
        update.extend_from_slice(&[0x03, 0x02, 0x01, 0, 0x06, 0x05, 0x04, 0]);
        push_rectangle_header(&mut update, 0, 0, 3, 1, enc::DESKTOP_SIZE);
        push_rectangle_header(&mut update, 2, 0, 1, 1, enc::RAW);
        update.extend_from_slice(&[0x09, 0x08, 0x07, 0]);

        let (client, mut peer) = duplex(512);
        peer.write_all(&update).await.unwrap();
        peer.shutdown().await.unwrap();
        let limits = ProtocolLimits::default();
        let mut reader = RfbReader::new(client, limits);
        let mut framebuffer = Framebuffer::new(2, 1, limits).unwrap();
        let (event_tx, event_rx) = bounded(8);
        let mut events = EventQueue::new(event_tx, limits);
        let (_command_tx, command_rx) = bounded::<VncCommand>(1);
        let clipboard = ClipboardSlot::default();

        let error = run_session(
            &mut reader,
            &mut framebuffer,
            &mut events,
            &command_rx,
            limits,
            &clipboard,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.phase(), RfbPhase::Session);
        assert_eq!(error.kind(), RfbErrorKind::Io);
        assert_eq!(error.io_kind(), Some(io::ErrorKind::UnexpectedEof));

        drop(reader);
        let mut outbound = Vec::new();
        peer.read_to_end(&mut outbound).await.unwrap();
        assert_eq!(
            outbound,
            [
                3, 0, 0, 0, 0, 0, 0, 3, 0, 1, // resize refresh: nonincremental, 3x1
                3, 1, 0, 0, 0, 0, 0, 3, 0, 1, // normal post-update incremental, 3x1
            ]
        );

        let VncEvent::FramebufferRects(old_rects) = event_rx.try_recv().unwrap() else {
            panic!("expected old-dimension framebuffer event first");
        };
        assert_eq!(old_rects.len(), 1);
        assert_eq!(
            (
                old_rects[0].x,
                old_rects[0].y,
                old_rects[0].w,
                old_rects[0].h,
            ),
            (0, 0, 2, 1)
        );
        assert_eq!(
            old_rects[0].rgba,
            [0x01, 0x02, 0x03, 0xff, 0x04, 0x05, 0x06, 0xff,]
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(VncEvent::DesktopSize(DesktopSize {
                width: 3,
                height: 1
            }))
        ));
        let VncEvent::FramebufferRects(new_rects) = event_rx.try_recv().unwrap() else {
            panic!("expected new-dimension framebuffer event after DesktopSize");
        };
        assert_eq!(new_rects.len(), 1);
        assert_eq!(
            (
                new_rects[0].x,
                new_rects[0].y,
                new_rects[0].w,
                new_rects[0].h,
            ),
            (2, 0, 1, 1)
        );
        assert_eq!(new_rects[0].rgba, [0x07, 0x08, 0x09, 0xff]);
        assert!(event_rx.try_recv().is_err());
        assert_eq!(framebuffer.dimensions(), (3, 1));
        assert_eq!(
            framebuffer.pixels(),
            [0, 0, 0, 0, 0, 0, 0, 0, 0x07, 0x08, 0x09, 0xff]
        );
    }

    #[tokio::test]
    async fn production_parser_keeps_forwarded_resize_pending_until_later_server_size() {
        let mut updates = vec![server_msg::FB_UPDATE, 0];
        updates.extend_from_slice(&1_u16.to_be_bytes());
        push_one_screen_extended_size(&mut updates, 1, 0, 1_600, 900);
        updates.extend_from_slice(&[server_msg::FB_UPDATE, 0]);
        updates.extend_from_slice(&1_u16.to_be_bytes());
        push_one_screen_extended_size(&mut updates, 0, 0, 1_600, 900);

        let (client, mut peer) = duplex(512);
        peer.write_all(&updates).await.unwrap();
        peer.shutdown().await.unwrap();
        let limits = ProtocolLimits::default();
        let mut reader = RfbReader::new(client, limits);
        let mut framebuffer = Framebuffer::new(640, 480, limits).unwrap();
        let (event_tx, event_rx) = bounded(8);
        let mut events = EventQueue::new(event_tx, limits);
        let (_command_tx, command_rx) = bounded::<VncCommand>(1);
        let clipboard = ClipboardSlot::default();

        let error = run_session(
            &mut reader,
            &mut framebuffer,
            &mut events,
            &command_rx,
            limits,
            &clipboard,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), RfbErrorKind::Io);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(VncEvent::ResizeOutcome(
                crate::connection::ResizeProtocolOutcome::Forwarded(DesktopSize {
                    width: 1_600,
                    height: 900
                })
            ))
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(VncEvent::DesktopSize(DesktopSize {
                width: 1_600,
                height: 900
            }))
        ));
        assert_eq!(framebuffer.dimensions(), (1_600, 900));
    }

    #[tokio::test]
    async fn server_unsupported_multiscreen_grow_advances_geometry_before_following_rect() {
        let mut update = vec![server_msg::FB_UPDATE, 0];
        update.extend_from_slice(&2_u16.to_be_bytes());
        push_extended_size(
            &mut update,
            0,
            0,
            4,
            1,
            &[(0, 0, 0, 2, 1, 0), (1, 2, 0, 2, 1, 0)],
        );
        push_rectangle_header(&mut update, 3, 0, 1, 1, enc::RAW);
        update.extend_from_slice(&[0x09, 0x08, 0x07, 0]);

        let (client, mut peer) = duplex(512);
        peer.write_all(&update).await.unwrap();
        peer.shutdown().await.unwrap();
        let limits = ProtocolLimits::default();
        let mut reader = RfbReader::new(client, limits);
        let mut framebuffer = Framebuffer::new(2, 1, limits).unwrap();
        let (event_tx, event_rx) = bounded(8);
        let mut events = EventQueue::new(event_tx, limits);
        let (_command_tx, command_rx) = bounded::<VncCommand>(1);
        let clipboard = ClipboardSlot::default();

        let error = run_session(
            &mut reader,
            &mut framebuffer,
            &mut events,
            &command_rx,
            limits,
            &clipboard,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.io_kind(), Some(io::ErrorKind::UnexpectedEof));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(VncEvent::DesktopSize(DesktopSize {
                width: 4,
                height: 1
            }))
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(VncEvent::ResizeOutcome(
                crate::connection::ResizeProtocolOutcome::ServerUnsupported
            ))
        ));
        let VncEvent::FramebufferRects(rects) = event_rx.try_recv().unwrap() else {
            panic!("following rectangle should use the grown geometry");
        };
        assert_eq!(
            (rects[0].x, rects[0].y, rects[0].w, rects[0].h),
            (3, 0, 1, 1)
        );
        assert_eq!(rects[0].rgba, [0x07, 0x08, 0x09, 0xff]);
        assert_eq!(framebuffer.dimensions(), (4, 1));

        drop(reader);
        let mut outbound = Vec::new();
        peer.read_to_end(&mut outbound).await.unwrap();
        assert_eq!(
            outbound,
            [3, 0, 0, 0, 0, 0, 0, 4, 0, 1, 3, 1, 0, 0, 0, 0, 0, 4, 0, 1,]
        );
    }

    #[tokio::test]
    async fn server_unsupported_flagged_reason_two_shrinks_and_requests_new_dimensions() {
        let mut update = vec![server_msg::FB_UPDATE, 0];
        update.extend_from_slice(&1_u16.to_be_bytes());
        push_extended_size(&mut update, 2, 0, 2, 1, &[(0, 0, 0, 2, 1, 1)]);

        let (client, mut peer) = duplex(256);
        peer.write_all(&update).await.unwrap();
        peer.shutdown().await.unwrap();
        let limits = ProtocolLimits::default();
        let mut reader = RfbReader::new(client, limits);
        let mut framebuffer = Framebuffer::new(4, 2, limits).unwrap();
        let (event_tx, event_rx) = bounded(8);
        let mut events = EventQueue::new(event_tx, limits);
        let (_command_tx, command_rx) = bounded::<VncCommand>(1);
        let clipboard = ClipboardSlot::default();

        run_session(
            &mut reader,
            &mut framebuffer,
            &mut events,
            &command_rx,
            limits,
            &clipboard,
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            event_rx.try_recv(),
            Ok(VncEvent::DesktopSize(DesktopSize {
                width: 2,
                height: 1
            }))
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(VncEvent::ResizeOutcome(
                crate::connection::ResizeProtocolOutcome::ServerUnsupported
            ))
        ));
        assert_eq!(framebuffer.dimensions(), (2, 1));

        drop(reader);
        let mut outbound = Vec::new();
        peer.read_to_end(&mut outbound).await.unwrap();
        assert_eq!(
            outbound,
            [3, 0, 0, 0, 0, 0, 0, 2, 0, 1, 3, 1, 0, 0, 0, 0, 0, 2, 0, 1,]
        );
    }

    #[tokio::test]
    async fn client_rejection_and_unsupported_outcomes_do_not_mutate_geometry() {
        let mut updates = vec![server_msg::FB_UPDATE, 0];
        updates.extend_from_slice(&1_u16.to_be_bytes());
        push_one_screen_extended_size(&mut updates, 1, 3, 4, 1);
        updates.extend_from_slice(&[server_msg::FB_UPDATE, 0]);
        updates.extend_from_slice(&1_u16.to_be_bytes());
        push_one_screen_extended_size(&mut updates, 1, 1, 5, 1);

        let (client, mut peer) = duplex(512);
        peer.write_all(&updates).await.unwrap();
        peer.shutdown().await.unwrap();
        let limits = ProtocolLimits::default();
        let mut reader = RfbReader::new(client, limits);
        let mut framebuffer = Framebuffer::new(2, 1, limits).unwrap();
        let (event_tx, event_rx) = bounded(8);
        let mut events = EventQueue::new(event_tx, limits);
        let (_command_tx, command_rx) = bounded::<VncCommand>(1);
        let clipboard = ClipboardSlot::default();

        run_session(
            &mut reader,
            &mut framebuffer,
            &mut events,
            &command_rx,
            limits,
            &clipboard,
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            event_rx.try_recv(),
            Ok(VncEvent::ResizeOutcome(
                crate::connection::ResizeProtocolOutcome::Unsupported
            ))
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(VncEvent::ResizeOutcome(
                crate::connection::ResizeProtocolOutcome::Rejected
            ))
        ));
        assert!(event_rx.try_recv().is_err());
        assert_eq!(framebuffer.dimensions(), (2, 1));

        drop(reader);
        let mut outbound = Vec::new();
        peer.read_to_end(&mut outbound).await.unwrap();
        assert_eq!(
            outbound,
            [3, 1, 0, 0, 0, 0, 0, 2, 0, 1, 3, 1, 0, 0, 0, 0, 0, 2, 0, 1,]
        );
    }

    #[test]
    fn framebuffer_pressure_coalesces_into_one_fixed_dirty_region() {
        let (sender, receiver) = bounded(1);
        let mut events = EventQueue::new(sender, ProtocolLimits::default());
        let mut framebuffer = Framebuffer::new(2, 2, ProtocolLimits::default()).unwrap();
        let whole = CheckedRect::new(0, 0, 2, 2, 2, 2).unwrap();
        framebuffer
            .write_rgba(whole, &(0_u8..16).collect::<Vec<_>>())
            .unwrap();

        events.send_lossless(VncEvent::Disconnected).unwrap();
        events
            .send_framebuffer(
                &framebuffer,
                framebuffer_rects(one_pixel_rect(0, 0, [0, 1, 2, 3])).unwrap(),
            )
            .unwrap();
        events
            .send_framebuffer(
                &framebuffer,
                framebuffer_rects(one_pixel_rect(1, 1, [12, 13, 14, 15])).unwrap(),
            )
            .unwrap();

        let pending = events.pending_dirty.expect("one pending region");
        assert_eq!(
            (pending.left, pending.top, pending.right, pending.bottom),
            (0, 0, 2, 2)
        );
        assert!(matches!(receiver.try_recv(), Ok(VncEvent::Disconnected)));

        events.flush_pending(&framebuffer).unwrap();
        assert!(events.pending_dirty.is_none());
        let VncEvent::FramebufferRects(rects) = receiver.try_recv().unwrap() else {
            panic!("expected one coalesced framebuffer event");
        };
        assert_eq!(rects.len(), 1);
        let rect = &rects[0];
        assert_eq!((rect.x, rect.y, rect.w, rect.h), (0, 0, 2, 2));
        assert_eq!(rect.rgba, framebuffer.pixels());
    }

    #[test]
    fn full_state_queue_returns_a_typed_error() {
        let (sender, _receiver) = bounded(1);
        let events = EventQueue::new(sender, ProtocolLimits::default());
        events.send_lossless(VncEvent::Disconnected).unwrap();

        let error_event = VncEvent::Error(RfbError::new(
            RfbPhase::Session,
            RfbErrorKind::Protocol,
            "synthetic error",
        ));
        let error = events.send_lossless(error_event).unwrap_err();
        assert_eq!(error.phase(), RfbPhase::EventQueue);
        assert_eq!(error.kind(), RfbErrorKind::Queue);
    }

    async fn next_event(
        connection: &crate::connection::VncConnection,
    ) -> crate::connection::VncEvent {
        timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(event) = connection.event_rx.try_recv() {
                    return event;
                }
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("synthetic peer did not produce a client event")
    }

    async fn wait_for_non_black_frame(
        connection: &crate::connection::VncConnection,
    ) -> Vec<FbRect> {
        loop {
            if let VncEvent::FramebufferRects(rects) = next_event(connection).await {
                assert!(rects.iter().any(|rect| {
                    rect.rgba
                        .chunks_exact(4)
                        .any(|pixel| pixel[..3] != [0, 0, 0])
                }));
                return rects;
            }
        }
    }

    fn fixed_test_ticket() -> ProxyTicket {
        ProxyTicket::for_auth_test_with_drop_signal("password").0
    }

    #[tokio::test]
    async fn deterministic_rfb_peer_authenticates_and_decodes_every_supported_image_encoding() {
        for encoding in rfb_peer::EncodingCase::ALL {
            let (client_stream, peer_stream) = duplex(2 * 1024 * 1024);
            let (connection, channels) = bounded_vnc_channels();
            let capture = rfb_peer::PeerCapture::default();
            let peer_capture = capture.clone();
            let peer = tokio::spawn(rfb_peer::run_peer(
                peer_stream,
                rfb_peer::PeerBehavior::Valid {
                    encoding,
                    resize_reply: rfb_peer::ResizeReply::None,
                },
                peer_capture,
            ));
            let (cancel, cancelled) = oneshot::channel();
            let client = tokio::spawn(VncClient::run_test_stream(
                client_stream,
                fixed_test_ticket(),
                VncOptions::default(),
                channels,
                false,
                cancelled,
            ));

            let rects = wait_for_non_black_frame(&connection).await;
            assert!(
                !rects.is_empty(),
                "{encoding:?} produced no dirty rectangle"
            );
            assert!(
                capture.auth_valid(),
                "{encoding:?} did not validate DES auth"
            );
            let facts = capture.messages();
            assert!(facts.iter().any(|fact| matches!(
                fact,
                rfb_peer::ClientMessageFact::SetEncodings(encodings)
                    if encodings == &[16, 5, 1, 0, -223, -308, 7]
            )));

            let _ = cancel.send(());
            timeout(Duration::from_secs(2), client)
                .await
                .expect("client task retained after cancellation")
                .unwrap()
                .unwrap();
            timeout(Duration::from_secs(2), peer)
                .await
                .expect("peer task retained after cancellation")
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn peer_records_bounded_semantic_input_update_clipboard_and_resize_facts_without_payloads(
    ) {
        use rfb_peer::ClientMessageFact as Fact;

        let (client_stream, peer_stream) = duplex(2 * 1024 * 1024);
        let (connection, channels) = bounded_vnc_channels();
        let capture = rfb_peer::PeerCapture::default();
        let peer = tokio::spawn(rfb_peer::run_peer(
            peer_stream,
            rfb_peer::PeerBehavior::Valid {
                encoding: rfb_peer::EncodingCase::Raw,
                resize_reply: rfb_peer::ResizeReply::Apply,
            },
            capture.clone(),
        ));
        let (cancel, cancelled) = oneshot::channel();
        let client = tokio::spawn(VncClient::run_test_stream(
            client_stream,
            fixed_test_ticket(),
            VncOptions::default(),
            channels,
            true,
            cancelled,
        ));
        wait_for_non_black_frame(&connection).await;

        let mut input =
            InputController::for_connection(connection, false, true, CLIPBOARD_TEXT_LIMIT).unwrap();
        input.mark_ready();
        input.pointer(1, 10, 20).unwrap();
        input.key(true, 0x41).unwrap();
        input.ctrl_alt_delete().unwrap();
        input.release_owned_input(Some((11, 21))).unwrap();
        input.key(true, 0x42).unwrap();
        input.release_all_keys().unwrap();
        input
            .send_clipboard("bounded synthetic clipboard".to_owned())
            .unwrap();

        for requested in [DesktopSize::new(1600, 896), DesktopSize::new(1920, 1080)] {
            input.connection().request_desktop_size(requested).unwrap();
            let mut forwarded = false;
            let mut applied = false;
            timeout(Duration::from_secs(2), async {
                while !(forwarded && applied) {
                    match next_event(input.connection()).await {
                        VncEvent::ResizeOutcome(ResizeProtocolOutcome::Forwarded(size))
                            if size == requested =>
                        {
                            forwarded = true;
                        }
                        VncEvent::DesktopSize(size) if size == requested => applied = true,
                        _ => {}
                    }
                }
            })
            .await
            .expect("resize lifecycle did not reach forwarded and applied");
        }

        timeout(Duration::from_secs(2), async {
            loop {
                let facts = capture.messages();
                if facts
                    .iter()
                    .filter(|fact| matches!(fact, Fact::SetDesktopSize { .. }))
                    .count()
                    == 2
                    && facts
                        .iter()
                        .any(|fact| matches!(fact, Fact::ClipboardLength(27)))
                {
                    break;
                }
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("bounded peer capture did not observe all client messages");

        let facts = capture.messages();
        let cad = [
            Fact::Key {
                down: true,
                keysym: 0xffe3,
            },
            Fact::Key {
                down: true,
                keysym: 0xffe9,
            },
            Fact::Key {
                down: true,
                keysym: 0xffff,
            },
            Fact::Key {
                down: false,
                keysym: 0xffff,
            },
            Fact::Key {
                down: false,
                keysym: 0xffe9,
            },
            Fact::Key {
                down: false,
                keysym: 0xffe3,
            },
        ];
        assert!(facts.windows(cad.len()).any(|window| window == cad));
        assert!(facts.contains(&Fact::Pointer {
            buttons: 1,
            x: 10,
            y: 20,
        }));
        assert!(facts.contains(&Fact::Pointer {
            buttons: 0,
            x: 11,
            y: 21,
        }));
        assert!(facts.contains(&Fact::Key {
            down: false,
            keysym: 0x41,
        }));
        assert!(facts.contains(&Fact::Key {
            down: false,
            keysym: 0x42,
        }));
        assert!(facts.contains(&Fact::ClipboardLength(27)));
        assert!(facts.iter().any(|fact| matches!(
            fact,
            Fact::UpdateRequest {
                width: 64,
                height: 64,
                ..
            }
        )));
        assert!(facts.contains(&Fact::SetDesktopSize {
            width: 1600,
            height: 896,
            screens: 1,
        }));
        assert!(facts.contains(&Fact::SetDesktopSize {
            width: 1920,
            height: 1080,
            screens: 1,
        }));
        assert!(facts.len() <= 128);
        assert!(!format!("{facts:?}").contains("bounded synthetic clipboard"));

        let _ = cancel.send(());
        timeout(Duration::from_secs(2), client)
            .await
            .expect("input client task retained after cancellation")
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(2), peer)
            .await
            .expect("input peer task retained after cancellation")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn closed_malformed_peer_cases_fail_finitely_with_typed_errors_and_no_retained_task() {
        for malformed in rfb_peer::MalformedCase::ALL {
            let (client_stream, peer_stream) = duplex(2 * 1024 * 1024);
            let (_connection, channels) = bounded_vnc_channels();
            let peer = tokio::spawn(rfb_peer::run_peer(
                peer_stream,
                rfb_peer::PeerBehavior::Malformed(malformed),
                rfb_peer::PeerCapture::default(),
            ));
            let (_cancel, cancelled) = oneshot::channel::<()>();
            let client = tokio::spawn(VncClient::run_test_stream(
                client_stream,
                fixed_test_ticket(),
                VncOptions::default(),
                channels,
                false,
                cancelled,
            ));

            let error = timeout(Duration::from_secs(2), client)
                .await
                .expect("malformed client case did not terminate")
                .unwrap()
                .unwrap_err();
            assert!(matches!(
                error.kind(),
                RfbErrorKind::ProtocolBanner
                    | RfbErrorKind::Protocol
                    | RfbErrorKind::SecurityFailure
                    | RfbErrorKind::SecurityAllowlist
                    | RfbErrorKind::Decoder
                    | RfbErrorKind::Io
            ));
            timeout(Duration::from_secs(2), peer)
                .await
                .expect("malformed peer task did not terminate")
                .unwrap()
                .unwrap();
        }
    }
}
