use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use flate2::Decompress;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::debug;

use crate::{
    connection::{FbRect, VncCommand, VncEvent, VNC_QUEUE_CAPACITY},
    framebuffer::Framebuffer,
    protocol::encoding::{copyrect, hextile, raw, tight, zrle},
    ssh::TrustedSshProxy,
};

use tight::TightState;

use super::{
    limits::validate_framebuffer_layout_for_phase,
    messages::{client_msg, encoding as enc, server_msg, PixelFormat},
    negotiate_version,
    security::negotiate_security,
    wire::{allocate_zeroed, sanitize_remote_text},
    ProtocolLimits, RfbError, RfbErrorKind, RfbPhase, RfbReader,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VncOptions {
    pub limits: ProtocolLimits,
    pub shared: bool,
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

fn validate_pixel_format(pixel_format: &PixelFormat) -> Result<(), RfbError> {
    let bits = pixel_format.bits_per_pixel;
    if !matches!(bits, 8 | 16 | 32)
        || pixel_format.depth == 0
        || pixel_format.depth > bits
        || !pixel_format.true_colour
        || pixel_format.red_max == 0
        || pixel_format.green_max == 0
        || pixel_format.blue_max == 0
        || pixel_format.red_shift >= bits
        || pixel_format.green_shift >= bits
        || pixel_format.blue_shift >= bits
    {
        return Err(RfbError::new(
            RfbPhase::ServerInit,
            RfbErrorKind::ServerInit,
            "pixel format",
        ));
    }
    Ok(())
}

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
    validate_pixel_format(&pixel_format)?;

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
        if width == 0 || height == 0 || right > framebuffer.width || bottom > framebuffer.height {
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
        let (stream, ticket) = proxy.into_parts();
        let mut reader = RfbReader::new(stream, limits);
        let version = negotiate_version(&mut reader).await?;
        negotiate_security(&mut reader, ticket, version).await?;

        reader
            .write_u8(u8::from(options.shared))
            .await
            .map_err(|source| RfbError::io(RfbPhase::ServerInit, source))?;
        let init = read_server_init(&mut reader).await?;
        debug!(
            width = init.width,
            height = init.height,
            "RFB ServerInit accepted"
        );

        let mut framebuffer = Framebuffer {
            width: u32::from(init.width),
            height: u32::from(init.height),
            pixels: init.framebuffer,
        };
        let pixel_format = init.pixel_format;
        let mut events = EventQueue::new(event_tx, limits);
        events.send_lossless(VncEvent::DesktopSize(framebuffer.width, framebuffer.height))?;
        events.send_lossless(VncEvent::DesktopName(init.desktop_name))?;

        send_set_encodings(&mut reader).await?;
        send_fb_update_request(&mut reader, false, 0, 0, init.width, init.height).await?;

        run_session(
            &mut reader,
            &mut framebuffer,
            &pixel_format,
            &mut events,
            &command_rx,
            limits,
        )
        .await
    }
}

async fn run_session<S>(
    reader: &mut RfbReader<S>,
    framebuffer: &mut Framebuffer,
    pixel_format: &PixelFormat,
    events: &mut EventQueue,
    command_rx: &Receiver<VncCommand>,
    limits: ProtocolLimits,
) -> Result<(), RfbError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut zrle_decompressor = Decompress::new(true);
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
                                .await
                                .map_err(|_| decoder_error())?;
                            include_dirty(&mut dirty, framebuffer, x, y, width, height)?;
                        }
                        enc::COPY_RECT => {
                            copyrect::decode(reader, framebuffer, x, y, width, height)
                                .await
                                .map_err(|_| decoder_error())?;
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
                                &mut zrle_decompressor,
                            )
                            .await
                            .map_err(|_| decoder_error())?;
                            include_dirty(&mut dirty, framebuffer, x, y, width, height)?;
                        }
                        enc::HEXTILE => {
                            hextile::decode(reader, framebuffer, pixel_format, x, y, width, height)
                                .await
                                .map_err(|_| decoder_error())?;
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
                            .await
                            .map_err(|_| decoder_error())?;
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
                            let layout = validate_framebuffer_layout_for_phase(
                                width,
                                height,
                                limits,
                                RfbPhase::Session,
                            )?;
                            let pixels = allocate_zeroed(
                                layout.rgba_bytes,
                                RfbPhase::Session,
                                "resized framebuffer",
                            )?;
                            framebuffer.width = u32::from(width);
                            framebuffer.height = u32::from(height);
                            framebuffer.pixels = pixels;
                            events.send_lossless(VncEvent::DesktopSize(
                                framebuffer.width,
                                framebuffer.height,
                            ))?;
                            send_fb_update_request(reader, false, 0, 0, width, height).await?;
                        }
                        enc::CURSOR => {
                            discard_cursor(reader, pixel_format, width, height, limits).await?;
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

                let width = u16::try_from(framebuffer.width)
                    .map_err(|_| RfbError::limit(RfbPhase::Session, "framebuffer update width"))?;
                let height = u16::try_from(framebuffer.height)
                    .map_err(|_| RfbError::limit(RfbPhase::Session, "framebuffer update height"))?;
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
                let text = sanitize_remote_text(&text, RfbPhase::Session, "server clipboard")?;
                events.send_lossless(VncEvent::ClipboardText(text))?;
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

fn decoder_error() -> RfbError {
    RfbError::new(
        RfbPhase::Session,
        RfbErrorKind::Protocol,
        "framebuffer decoder",
    )
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
    if width == 0 || height == 0 || right > framebuffer.width || bottom > framebuffer.height {
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
    let byte_count = usize::try_from(byte_count)
        .map_err(|_| RfbError::limit(RfbPhase::EventQueue, "framebuffer event bytes"))?;
    let mut rgba = allocate_zeroed(byte_count, RfbPhase::EventQueue, "framebuffer event")?;
    let row_bytes_u64 = u64::from(width)
        .checked_mul(4)
        .ok_or_else(|| RfbError::limit(RfbPhase::EventQueue, "framebuffer event row"))?;
    let row_bytes = usize::try_from(row_bytes_u64)
        .map_err(|_| RfbError::limit(RfbPhase::EventQueue, "framebuffer event row"))?;

    for row in 0..height {
        let source_pixel = u64::from(y + row)
            .checked_mul(u64::from(framebuffer.width))
            .and_then(|offset| offset.checked_add(u64::from(x)))
            .ok_or_else(|| RfbError::limit(RfbPhase::EventQueue, "framebuffer event source"))?;
        let source_start = source_pixel
            .checked_mul(4)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| RfbError::limit(RfbPhase::EventQueue, "framebuffer event source"))?;
        let source_end = source_start
            .checked_add(row_bytes)
            .ok_or_else(|| RfbError::limit(RfbPhase::EventQueue, "framebuffer event source"))?;
        let destination_start = usize::try_from(u64::from(row) * row_bytes_u64)
            .map_err(|_| RfbError::limit(RfbPhase::EventQueue, "framebuffer event target"))?;
        let destination_end = destination_start
            .checked_add(row_bytes)
            .ok_or_else(|| RfbError::limit(RfbPhase::EventQueue, "framebuffer event target"))?;
        let source = framebuffer
            .pixels
            .get(source_start..source_end)
            .ok_or_else(|| {
                RfbError::new(
                    RfbPhase::EventQueue,
                    RfbErrorKind::Protocol,
                    "framebuffer event source",
                )
            })?;
        let destination = rgba
            .get_mut(destination_start..destination_end)
            .ok_or_else(|| {
                RfbError::new(
                    RfbPhase::EventQueue,
                    RfbErrorKind::Protocol,
                    "framebuffer event target",
                )
            })?;
        destination.copy_from_slice(source);
    }

    Ok(FbRect {
        x,
        y,
        w: width,
        h: height,
        rgba,
    })
}

async fn discard_cursor<S>(
    reader: &mut RfbReader<S>,
    pixel_format: &PixelFormat,
    width: u16,
    height: u16,
    limits: ProtocolLimits,
) -> Result<(), RfbError>
where
    S: AsyncRead + Unpin,
{
    let bytes_per_pixel = u64::from(pixel_format.bits_per_pixel / 8);
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| RfbError::limit(RfbPhase::Session, "cursor pixels"))?;
    let image_bytes = pixels
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| RfbError::limit(RfbPhase::Session, "cursor image"))?;
    let mask_row_bytes = u64::from(width)
        .checked_add(7)
        .ok_or_else(|| RfbError::limit(RfbPhase::Session, "cursor mask"))?
        / 8;
    let mask_bytes = mask_row_bytes
        .checked_mul(u64::from(height))
        .ok_or_else(|| RfbError::limit(RfbPhase::Session, "cursor mask"))?;
    let declared = image_bytes
        .checked_add(mask_bytes)
        .ok_or_else(|| RfbError::limit(RfbPhase::Session, "cursor payload"))?;
    let bytes = reader
        .read_bounded_bytes(
            declared,
            limits.max_framebuffer_bytes,
            "cursor payload",
            RfbPhase::Session,
        )
        .await?;
    drop(bytes);
    Ok(())
}

async fn send_set_encodings<S>(reader: &mut RfbReader<S>) -> Result<(), RfbError>
where
    S: AsyncWrite + Unpin,
{
    let encodings = [
        enc::ZRLE,
        enc::HEXTILE,
        enc::COPY_RECT,
        enc::RAW,
        enc::DESKTOP_SIZE,
        enc::TIGHT,
    ];
    let mut message = [0_u8; 28];
    message[0] = client_msg::SET_ENCODINGS;
    message[2..4].copy_from_slice(&(encodings.len() as u16).to_be_bytes());
    for (index, encoding) in encodings.iter().enumerate() {
        let start = 4 + index * 4;
        message[start..start + 4].copy_from_slice(&encoding.to_be_bytes());
    }
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
mod tests {
    use crossbeam_channel::bounded;

    use super::{framebuffer_rects, EventQueue};
    use crate::{
        connection::{FbRect, VncEvent},
        framebuffer::Framebuffer,
        vnc::{ProtocolLimits, RfbError, RfbErrorKind, RfbPhase},
    };

    fn one_pixel_rect(x: u32, y: u32, rgba: [u8; 4]) -> FbRect {
        FbRect {
            x,
            y,
            w: 1,
            h: 1,
            rgba: rgba.to_vec(),
        }
    }

    #[test]
    fn framebuffer_pressure_coalesces_into_one_fixed_dirty_region() {
        let (sender, receiver) = bounded(1);
        let mut events = EventQueue::new(sender, ProtocolLimits::default());
        let framebuffer = Framebuffer {
            width: 2,
            height: 2,
            pixels: (0_u8..16).collect(),
        };

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
        assert_eq!(rect.rgba, framebuffer.pixels);
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
}
