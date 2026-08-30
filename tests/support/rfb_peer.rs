#![allow(dead_code)]

use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use flate2::{write::ZlibEncoder, Compression};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_CAPTURED_MESSAGES: usize = 128;
const MAX_CLIPBOARD_BYTES: u32 = 1_048_576;
const CHALLENGE: [u8; 16] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
];
const PASSWORD_RESPONSE: [u8; 16] = [
    0xb8, 0x66, 0x92, 0x41, 0x25, 0xc8, 0xee, 0xbb, 0x9d, 0xeb, 0xc1, 0xdb, 0x61, 0xc5, 0x38, 0xe2,
];
const CANONICAL_PIXEL_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];
const EXTENDED_DESKTOP_SIZE: i32 = -308;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodingCase {
    Raw,
    CopyRect,
    Hextile,
    Zrle,
    Tight,
}

impl EncodingCase {
    pub const ALL: [Self; 5] = [
        Self::Raw,
        Self::CopyRect,
        Self::Hextile,
        Self::Zrle,
        Self::Tight,
    ];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MalformedCase {
    Banner,
    SecurityTypes,
    Rectangle,
}

impl MalformedCase {
    pub const ALL: [Self; 3] = [Self::Banner, Self::SecurityTypes, Self::Rectangle];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResizeReply {
    Apply,
    Reject,
    Unsupported,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerBehavior {
    Valid {
        encoding: EncodingCase,
        resize_reply: ResizeReply,
    },
    Malformed(MalformedCase),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientMessageFact {
    SetPixelFormat,
    SetEncodings(Vec<i32>),
    UpdateRequest {
        incremental: bool,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Key {
        down: bool,
        keysym: u32,
    },
    Pointer {
        buttons: u8,
        x: u16,
        y: u16,
    },
    ClipboardLength(u32),
    SetDesktopSize {
        message_padding: u8,
        desktop_width: u16,
        desktop_height: u16,
        screen_count: u8,
        screen_padding: u8,
        screen_id: u32,
        screen_x: u16,
        screen_y: u16,
        screen_width: u16,
        screen_height: u16,
        screen_flags: u32,
    },
}

#[derive(Default)]
struct CaptureState {
    auth_valid: bool,
    messages: Vec<ClientMessageFact>,
}

#[derive(Clone, Default)]
pub struct PeerCapture(Arc<Mutex<CaptureState>>);

impl PeerCapture {
    pub fn auth_valid(&self) -> bool {
        self.0.lock().unwrap().auth_valid
    }

    pub fn messages(&self) -> Vec<ClientMessageFact> {
        self.0.lock().unwrap().messages.clone()
    }

    fn set_auth_valid(&self) {
        self.0.lock().unwrap().auth_valid = true;
    }

    fn record(&self, fact: ClientMessageFact) -> io::Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.messages.len() == MAX_CAPTURED_MESSAGES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bounded client-message capture is full",
            ));
        }
        state.messages.push(fact);
        Ok(())
    }
}

pub fn non_black_rgba(encoding: EncodingCase) -> Vec<u8> {
    let rgb = match encoding {
        EncodingCase::Raw => [1, 2, 3],
        EncodingCase::CopyRect => [4, 5, 6],
        EncodingCase::Hextile => [7, 8, 9],
        EncodingCase::Zrle => [10, 11, 12],
        EncodingCase::Tight => [15, 14, 13],
    };
    vec![rgb[0], rgb[1], rgb[2], 255]
}

pub async fn run_peer<S>(
    mut stream: S,
    behavior: PeerBehavior,
    capture: PeerCapture,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match behavior {
        PeerBehavior::Malformed(MalformedCase::Banner) => {
            stream.write_all(b"RFB 003").await?;
            stream.shutdown().await?;
            return Ok(());
        }
        PeerBehavior::Malformed(MalformedCase::SecurityTypes) => {
            exchange_version(&mut stream).await?;
            stream.write_all(&[1]).await?;
            stream.shutdown().await?;
            return Ok(());
        }
        PeerBehavior::Valid { .. } | PeerBehavior::Malformed(MalformedCase::Rectangle) => {}
    }

    handshake(&mut stream, &capture).await?;
    read_configuration(&mut stream, &capture).await?;

    match behavior {
        PeerBehavior::Malformed(MalformedCase::Rectangle) => {
            stream.write_all(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 1]).await?;
            stream.shutdown().await?;
            Ok(())
        }
        PeerBehavior::Valid {
            encoding,
            resize_reply,
        } => {
            stream.write_all(&frame_update(encoding)?).await?;
            capture_client_messages(&mut stream, &capture, resize_reply).await
        }
        PeerBehavior::Malformed(_) => unreachable!("early malformed cases returned above"),
    }
}

async fn exchange_version<S>(stream: &mut S) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(b"RFB 003.008\n").await?;
    let mut version = [0_u8; 12];
    stream.read_exact(&mut version).await?;
    if version != *b"RFB 003.008\n" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "client selected an unexpected RFB version",
        ));
    }
    Ok(())
}

async fn handshake<S>(stream: &mut S, capture: &PeerCapture) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    exchange_version(stream).await?;
    stream.write_all(&[1, 2]).await?;
    if stream.read_u8().await? != 2 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "client did not select VNC authentication",
        ));
    }
    stream.write_all(&CHALLENGE).await?;
    let mut response = [0_u8; 16];
    stream.read_exact(&mut response).await?;
    if response != PASSWORD_RESPONSE {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "synthetic VNC response mismatch",
        ));
    }
    capture.set_auth_valid();
    stream.write_all(&0_u32.to_be_bytes()).await?;
    if stream.read_u8().await? != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "client did not request a shared session",
        ));
    }

    let mut init = Vec::new();
    init.extend_from_slice(&64_u16.to_be_bytes());
    init.extend_from_slice(&64_u16.to_be_bytes());
    init.extend_from_slice(&CANONICAL_PIXEL_FORMAT);
    init.extend_from_slice(&9_u32.to_be_bytes());
    init.extend_from_slice(b"synthetic");
    stream.write_all(&init).await
}

async fn read_configuration<S>(stream: &mut S, capture: &PeerCapture) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut pixel_format = [0_u8; 20];
    stream.read_exact(&mut pixel_format).await?;
    if pixel_format[0] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected SetPixelFormat",
        ));
    }
    capture.record(ClientMessageFact::SetPixelFormat)?;

    let mut encodings_header = [0_u8; 4];
    stream.read_exact(&mut encodings_header).await?;
    if encodings_header[0] != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected SetEncodings",
        ));
    }
    let count = u16::from_be_bytes([encodings_header[2], encodings_header[3]]);
    if count > 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SetEncodings exceeded the fixture bound",
        ));
    }
    let mut encodings = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        encodings.push(stream.read_i32().await?);
    }
    capture.record(ClientMessageFact::SetEncodings(encodings))?;

    read_one_message(stream, capture, ResizeReply::None).await
}

fn push_rect_header(bytes: &mut Vec<u8>, x: u16, y: u16, width: u16, height: u16, encoding: i32) {
    bytes.extend_from_slice(&x.to_be_bytes());
    bytes.extend_from_slice(&y.to_be_bytes());
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&encoding.to_be_bytes());
}

fn frame_update(encoding: EncodingCase) -> io::Result<Vec<u8>> {
    let mut bytes = vec![
        0,
        0,
        0,
        if encoding == EncodingCase::CopyRect {
            2
        } else {
            1
        },
    ];
    match encoding {
        EncodingCase::Raw => {
            push_rect_header(&mut bytes, 0, 0, 1, 1, 0);
            bytes.extend_from_slice(&[3, 2, 1, 0]);
        }
        EncodingCase::CopyRect => {
            push_rect_header(&mut bytes, 0, 0, 1, 1, 0);
            bytes.extend_from_slice(&[6, 5, 4, 0]);
            push_rect_header(&mut bytes, 1, 0, 1, 1, 1);
            bytes.extend_from_slice(&0_u16.to_be_bytes());
            bytes.extend_from_slice(&0_u16.to_be_bytes());
        }
        EncodingCase::Hextile => {
            push_rect_header(&mut bytes, 0, 0, 1, 1, 5);
            bytes.push(1);
            bytes.extend_from_slice(&[9, 8, 7, 0]);
        }
        EncodingCase::Zrle => {
            push_rect_header(&mut bytes, 0, 0, 1, 1, 16);
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&[0, 12, 11, 10])?;
            let compressed = encoder.finish()?;
            bytes.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&compressed);
        }
        EncodingCase::Tight => {
            push_rect_header(&mut bytes, 0, 0, 1, 1, 7);
            bytes.extend_from_slice(&[0x80, 15, 14, 13]);
        }
    }
    Ok(bytes)
}

async fn capture_client_messages<S>(
    stream: &mut S,
    capture: &PeerCapture,
    resize_reply: ResizeReply,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        match read_one_message(stream, capture, resize_reply).await {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

async fn read_one_message<S>(
    stream: &mut S,
    capture: &PeerCapture,
    resize_reply: ResizeReply,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match stream.read_u8().await? {
        0 => {
            let mut rest = [0_u8; 19];
            stream.read_exact(&mut rest).await?;
            capture.record(ClientMessageFact::SetPixelFormat)
        }
        2 => {
            let _padding = stream.read_u8().await?;
            let count = stream.read_u16().await?;
            if count > 32 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SetEncodings exceeded the fixture bound",
                ));
            }
            let mut encodings = Vec::with_capacity(usize::from(count));
            for _ in 0..count {
                encodings.push(stream.read_i32().await?);
            }
            capture.record(ClientMessageFact::SetEncodings(encodings))
        }
        3 => {
            let incremental = stream.read_u8().await? != 0;
            let x = stream.read_u16().await?;
            let y = stream.read_u16().await?;
            let width = stream.read_u16().await?;
            let height = stream.read_u16().await?;
            capture.record(ClientMessageFact::UpdateRequest {
                incremental,
                x,
                y,
                width,
                height,
            })
        }
        4 => {
            let down = stream.read_u8().await? != 0;
            let mut padding = [0_u8; 2];
            stream.read_exact(&mut padding).await?;
            let keysym = stream.read_u32().await?;
            capture.record(ClientMessageFact::Key { down, keysym })
        }
        5 => {
            let buttons = stream.read_u8().await?;
            let x = stream.read_u16().await?;
            let y = stream.read_u16().await?;
            capture.record(ClientMessageFact::Pointer { buttons, x, y })
        }
        6 => {
            let mut padding = [0_u8; 3];
            stream.read_exact(&mut padding).await?;
            let length = stream.read_u32().await?;
            if length > MAX_CLIPBOARD_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "client clipboard exceeded the fixture bound",
                ));
            }
            let mut remaining = length;
            let mut discard = [0_u8; 4096];
            while remaining > 0 {
                let take = usize::try_from(remaining.min(discard.len() as u32)).unwrap();
                stream.read_exact(&mut discard[..take]).await?;
                remaining -= take as u32;
            }
            capture.record(ClientMessageFact::ClipboardLength(length))
        }
        251 => {
            let message_padding = stream.read_u8().await?;
            let width = stream.read_u16().await?;
            let height = stream.read_u16().await?;
            let screens = stream.read_u8().await?;
            let screen_padding = stream.read_u8().await?;
            if screens != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fixture accepts one SetDesktopSize screen",
                ));
            }
            let mut screen = [0_u8; 16];
            stream.read_exact(&mut screen).await?;
            let screen_id = u32::from_be_bytes([screen[0], screen[1], screen[2], screen[3]]);
            let screen_x = u16::from_be_bytes([screen[4], screen[5]]);
            let screen_y = u16::from_be_bytes([screen[6], screen[7]]);
            let screen_width = u16::from_be_bytes([screen[8], screen[9]]);
            let screen_height = u16::from_be_bytes([screen[10], screen[11]]);
            let screen_flags = u32::from_be_bytes([screen[12], screen[13], screen[14], screen[15]]);
            if screen_width != width || screen_height != height {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SetDesktopSize screen geometry mismatch",
                ));
            }
            capture.record(ClientMessageFact::SetDesktopSize {
                message_padding,
                desktop_width: width,
                desktop_height: height,
                screen_count: screens,
                screen_padding,
                screen_id,
                screen_x,
                screen_y,
                screen_width,
                screen_height,
                screen_flags,
            })?;
            match resize_reply {
                ResizeReply::Apply => {
                    write_extended_desktop_size(stream, 1, 0, width, height).await?;
                    write_extended_desktop_size(stream, 0, 0, width, height).await
                }
                ResizeReply::Reject => {
                    write_extended_desktop_size(stream, 1, 1, width, height).await
                }
                ResizeReply::Unsupported => {
                    write_extended_desktop_size(stream, 1, 3, width, height).await
                }
                ResizeReply::None => Ok(()),
            }
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown client message type",
        )),
    }
}

async fn write_extended_desktop_size<S>(
    stream: &mut S,
    reason: u16,
    result: u16,
    width: u16,
    height: u16,
) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut bytes = vec![0, 0, 0, 1];
    push_rect_header(
        &mut bytes,
        reason,
        result,
        width,
        height,
        EXTENDED_DESKTOP_SIZE,
    );
    bytes.extend_from_slice(&[1, 0, 0, 0]);
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.extend_from_slice(&0_u16.to_be_bytes());
    bytes.extend_from_slice(&0_u16.to_be_bytes());
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    stream.write_all(&bytes).await
}
