//! Shared RFB fuzz harness. Session execution is gated on `cfg(fuzzing)`.

use std::{
    collections::HashSet,
    fs,
    future::Future,
    io::{Cursor, ErrorKind, Write},
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::LazyLock,
    task::{Context, Poll},
    time::Duration,
};

use flate2::{write::ZlibEncoder, Compression};
use image::{codecs::jpeg::JpegEncoder, ExtendedColorType};
use rustedoutclient::vnc::{
    encoding::{hextile, tight, zrle},
    messages::{self, PixelFormat},
    negotiate_security_type, negotiate_version, read_security_result, read_server_init,
    Framebuffer, ProtocolLimits, RfbError, RfbErrorKind, RfbReader,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(fuzzing)]
use rustedoutclient::vnc::{fuzz_run_session, FuzzSessionObservation};

pub const CANONICAL_TARGETS: [&str; 5] = [
    "rfb_handshake",
    "rfb_session",
    "rfb_zrle",
    "rfb_tight",
    "rfb_hextile",
];

const HARNESS_TIMEOUT: Duration = Duration::from_millis(250);
const SYNTHETIC: &[u8] = b"synthetic";
const DENY: &[u8] = b"deny";
const BANNER_38: &[u8] = b"RFB 003.008\n";
const CANONICAL_PF: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Category {
    Ok,
    IoEof,
    IoOther,
    ProtocolBanner,
    SecurityAllowlist,
    SecurityFailure,
    ServerInit,
    Limit,
    Allocation,
    Decoder,
    Protocol,
    Queue,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "Ok",
            Self::IoEof => "IoEof",
            Self::IoOther => "IoOther",
            Self::ProtocolBanner => "ProtocolBanner",
            Self::SecurityAllowlist => "SecurityAllowlist",
            Self::SecurityFailure => "SecurityFailure",
            Self::ServerInit => "ServerInit",
            Self::Limit => "Limit",
            Self::Allocation => "Allocation",
            Self::Decoder => "Decoder",
            Self::Protocol => "Protocol",
            Self::Queue => "Queue",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "Ok" => Self::Ok,
            "IoEof" => Self::IoEof,
            "IoOther" => Self::IoOther,
            "ProtocolBanner" => Self::ProtocolBanner,
            "SecurityAllowlist" => Self::SecurityAllowlist,
            "SecurityFailure" => Self::SecurityFailure,
            "ServerInit" => Self::ServerInit,
            "Limit" => Self::Limit,
            "Allocation" => Self::Allocation,
            "Decoder" => Self::Decoder,
            "Protocol" => Self::Protocol,
            "Queue" => Self::Queue,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CorpusManifest {
    pub version: u32,
    pub targets: Vec<String>,
    pub seeds: Vec<SeedRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SeedRecord {
    pub target: String,
    pub file: String,
    pub sha256: String,
    pub length: u64,
    pub category: Category,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixture: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub behavior: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Execution {
    pub category: Category,
    pub transitions: Vec<String>,
}

pub fn classify(result: Result<(), RfbError>) -> Category {
    match result {
        Ok(()) => Category::Ok,
        Err(error) => match error.kind() {
            RfbErrorKind::ProtocolBanner => Category::ProtocolBanner,
            RfbErrorKind::SecurityAllowlist => Category::SecurityAllowlist,
            RfbErrorKind::SecurityFailure => Category::SecurityFailure,
            RfbErrorKind::ServerInit => Category::ServerInit,
            RfbErrorKind::Limit => Category::Limit,
            RfbErrorKind::Allocation => Category::Allocation,
            RfbErrorKind::Decoder => Category::Decoder,
            RfbErrorKind::Protocol => Category::Protocol,
            RfbErrorKind::Queue => Category::Queue,
            RfbErrorKind::Io => {
                if error.io_kind() == Some(ErrorKind::UnexpectedEof) {
                    Category::IoEof
                } else {
                    Category::IoOther
                }
            }
        },
    }
}

pub fn tight_geometry(header: u8) -> (u16, u16) {
    match header {
        0 => (1, 1),
        1 => (2, 2),
        2 => (4, 4),
        3 => (8, 8),
        4 => (16, 16),
        _ => (16, 16),
    }
}

pub fn fuzz_limits() -> ProtocolLimits {
    ProtocolLimits {
        max_dimension: 64,
        max_pixels: 4_096,
        max_framebuffer_bytes: 16_384,
        max_text_bytes: 4_096,
        max_rectangles: 64,
        max_encoded_rect_bytes: 65_536,
        max_clipboard_bytes: 4_096,
    }
}

pub fn session_limits() -> ProtocolLimits {
    ProtocolLimits {
        max_encoded_rect_bytes: 4_096,
        ..fuzz_limits()
    }
}

pub fn hextile_limits() -> ProtocolLimits {
    ProtocolLimits {
        max_encoded_rect_bytes: 2_048,
        ..fuzz_limits()
    }
}

pub fn canonical_format() -> PixelFormat {
    PixelFormat {
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
    }
}

pub struct SliceSink {
    input: Cursor<Vec<u8>>,
}

impl SliceSink {
    pub fn new(input: &[u8]) -> Self {
        Self {
            input: Cursor::new(input.to_vec()),
        }
    }
}

impl AsyncRead for SliceSink {
    fn poll_read(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let position = usize::try_from(this.input.position()).unwrap_or(usize::MAX);
        let remaining = this.input.get_ref().len().saturating_sub(position);
        let length = remaining.min(buffer.remaining());
        buffer.put_slice(&this.input.get_ref()[position..position + length]);
        this.input.set_position((position + length) as u64);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for SliceSink {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("current-thread runtime")
    });
    &RUNTIME
}

fn block_on_timeout<F, T>(future: F) -> T
where
    F: Future<Output = T>,
{
    runtime()
        .block_on(async { tokio::time::timeout(HARNESS_TIMEOUT, future).await })
        .expect("harness timeout")
}

fn session_framebuffer(limits: ProtocolLimits) -> Framebuffer {
    Framebuffer::new(64, 64, limits).expect("64x64 harness framebuffer")
}

fn assert_unchanged_on_error(
    result: &Result<(), RfbError>,
    before: &[u8],
    framebuffer: &Framebuffer,
) {
    if result.is_err() && framebuffer.pixels() != before {
        panic!("framebuffer mutated on decoder error");
    }
}

pub fn execute_rfb_handshake(data: &[u8]) -> Execution {
    let category = block_on_timeout(async {
        let mut reader = RfbReader::new(SliceSink::new(data), fuzz_limits());
        let version = match negotiate_version(&mut reader).await {
            Ok(version) => version,
            Err(error) => return classify(Err(error)),
        };
        if let Err(error) = negotiate_security_type(&mut reader, version).await {
            return classify(Err(error));
        }
        if let Err(error) = read_security_result(&mut reader, version).await {
            return classify(Err(error));
        }
        classify(read_server_init(&mut reader).await.map(|_| ()))
    });
    Execution {
        category,
        transitions: Vec::new(),
    }
}

pub fn execute_rfb_zrle(data: &[u8]) -> Execution {
    let category = block_on_timeout(async {
        let limits = fuzz_limits();
        let mut framebuffer = session_framebuffer(limits);
        let before = framebuffer.pixels().to_vec();
        let mut reader = RfbReader::new(data, limits);
        let mut state = zrle::ZrleState::new();
        let result = zrle::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            16,
            16,
            &mut state,
        )
        .await;
        assert_unchanged_on_error(&result, &before, &framebuffer);
        classify(result)
    });
    Execution {
        category,
        transitions: Vec::new(),
    }
}

pub fn execute_rfb_tight(data: &[u8]) -> Execution {
    let category = block_on_timeout(async {
        let (width, height, payload) = match data.split_first() {
            Some((header, rest)) => {
                let (width, height) = tight_geometry(*header);
                (width, height, rest)
            }
            None => (16, 16, &[][..]),
        };
        let limits = fuzz_limits();
        let mut framebuffer = session_framebuffer(limits);
        let before = framebuffer.pixels().to_vec();
        let mut reader = RfbReader::new(payload, limits);
        let mut state = tight::TightState::new();
        let result = tight::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            width,
            height,
            &mut state,
        )
        .await;
        assert_unchanged_on_error(&result, &before, &framebuffer);
        classify(result)
    });
    Execution {
        category,
        transitions: Vec::new(),
    }
}

pub fn execute_rfb_hextile(data: &[u8]) -> Execution {
    let category = block_on_timeout(async {
        let limits = hextile_limits();
        let mut framebuffer = session_framebuffer(limits);
        let before = framebuffer.pixels().to_vec();
        let mut reader = RfbReader::new(data, limits);
        let result = hextile::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            32,
            16,
        )
        .await;
        assert_unchanged_on_error(&result, &before, &framebuffer);
        classify(result)
    });
    Execution {
        category,
        transitions: Vec::new(),
    }
}

#[cfg(fuzzing)]
fn session_transitions(observation: &FuzzSessionObservation) -> Vec<String> {
    let mut transitions = Vec::new();
    if observation.framebuffer_events > 0 {
        transitions.push("framebuffer_event".to_string());
    }
    if observation.desktop_size_events > 0 {
        transitions.push("desktop_size_event".to_string());
    }
    if observation.resize_outcome_events > 0 {
        transitions.push("resize_outcome_event".to_string());
    }
    if observation.final_width != 64 || observation.final_height != 64 {
        transitions.push("framebuffer_resized".to_string());
    }
    transitions
}

#[cfg(fuzzing)]
pub fn execute_rfb_session(data: &[u8]) -> Execution {
    block_on_timeout(async {
        let limits = session_limits();
        let mut reader = RfbReader::new(SliceSink::new(data), limits);
        let mut framebuffer = session_framebuffer(limits);
        let observation = fuzz_run_session(&mut reader, &mut framebuffer).await;
        let transitions = session_transitions(&observation);
        Execution {
            category: classify(observation.result),
            transitions,
        }
    })
}

pub fn run_rfb_handshake(data: &[u8]) {
    let _ = execute_rfb_handshake(data);
}

#[cfg(fuzzing)]
pub fn run_rfb_session(data: &[u8]) {
    let _ = execute_rfb_session(data);
}

pub fn run_rfb_zrle(data: &[u8]) {
    let _ = execute_rfb_zrle(data);
}

pub fn run_rfb_tight(data: &[u8]) {
    let _ = execute_rfb_tight(data);
}

pub fn run_rfb_hextile(data: &[u8]) {
    let _ = execute_rfb_hextile(data);
}

pub fn execute_target(target: &str, data: &[u8]) -> Result<Execution, String> {
    match target {
        "rfb_handshake" => Ok(execute_rfb_handshake(data)),
        "rfb_zrle" => Ok(execute_rfb_zrle(data)),
        "rfb_tight" => Ok(execute_rfb_tight(data)),
        "rfb_hextile" => Ok(execute_rfb_hextile(data)),
        "rfb_session" => {
            #[cfg(fuzzing)]
            {
                Ok(execute_rfb_session(data))
            }
            #[cfg(not(fuzzing))]
            {
                let _ = data;
                Err("fuzzing".to_string())
            }
        }
        _ => Err("unknown".to_string()),
    }
}

pub struct Candidate {
    pub target: &'static str,
    pub name: &'static str,
    pub category: Category,
    pub transition: Option<&'static str>,
    pub behavior: &'static str,
    pub bytes: Vec<u8>,
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_i32(bytes: &mut Vec<u8>, value: i32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn bounded_text(text: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_u32(&mut bytes, text.len() as u32);
    bytes.extend_from_slice(text);
    bytes
}

fn handshake_ok_prefix() -> Vec<u8> {
    let mut bytes = BANNER_38.to_vec();
    bytes.extend_from_slice(&[1, 2]);
    push_u32(&mut bytes, 0);
    bytes
}

fn server_init(width: u16, height: u16, pixel_format: [u8; 16], name: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_u16(&mut bytes, width);
    push_u16(&mut bytes, height);
    bytes.extend_from_slice(&pixel_format);
    bytes.extend_from_slice(&bounded_text(name));
    bytes
}

fn overlapping_pixel_format() -> [u8; 16] {
    let mut pixel_format = CANONICAL_PF;
    pixel_format[10] = 16;
    pixel_format[11] = 16;
    pixel_format
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("zlib compress");
    encoder.finish().expect("zlib finish")
}

fn zrle_wire(decoded: &[u8]) -> Vec<u8> {
    let compressed = zlib(decoded);
    let mut wire = Vec::new();
    push_u32(&mut wire, compressed.len() as u32);
    wire.extend_from_slice(&compressed);
    wire
}

fn compact_length(value: u32) -> Vec<u8> {
    assert!(value <= 4_194_303);
    if value <= 127 {
        vec![value as u8]
    } else if value <= 16_383 {
        vec![((value & 0x7f) as u8) | 0x80, (value >> 7) as u8]
    } else {
        vec![
            ((value & 0x7f) as u8) | 0x80,
            (((value >> 7) & 0x7f) as u8) | 0x80,
            (value >> 14) as u8,
        ]
    }
}

fn jpeg_rgb(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let mut pixels = vec![0; (width * height * 3) as usize];
    for pixel in pixels.as_chunks_mut::<3>().0 {
        pixel.copy_from_slice(&rgb);
    }
    let mut encoded = Vec::new();
    JpegEncoder::new(&mut encoded)
        .encode(&pixels, width, height, ExtendedColorType::Rgb8)
        .expect("jpeg encode");
    encoded
}

fn tpixel(red: u8, green: u8, blue: u8) -> [u8; 3] {
    [red, green, blue]
}

fn cpixel(red: u8, green: u8, blue: u8) -> [u8; 3] {
    [blue, green, red]
}

fn pixel32(red: u8, green: u8, blue: u8) -> [u8; 4] {
    [blue, green, red, 0]
}

fn tight_seed(geometry: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![geometry];
    bytes.extend_from_slice(payload);
    bytes
}

fn fb_update(count: u16) -> Vec<u8> {
    let mut bytes = vec![messages::server_msg::FB_UPDATE, 0];
    push_u16(&mut bytes, count);
    bytes
}

fn rect_header(x: u16, y: u16, width: u16, height: u16, encoding: i32) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_u16(&mut bytes, x);
    push_u16(&mut bytes, y);
    push_u16(&mut bytes, width);
    push_u16(&mut bytes, height);
    push_i32(&mut bytes, encoding);
    bytes
}

fn candidate(
    target: &'static str,
    name: &'static str,
    category: Category,
    transition: Option<&'static str>,
    behavior: &'static str,
    bytes: Vec<u8>,
) -> Candidate {
    Candidate {
        target,
        name,
        category,
        transition,
        behavior,
        bytes,
    }
}

fn handshake_candidates() -> Vec<Candidate> {
    let mut oversized_name = handshake_ok_prefix();
    push_u16(&mut oversized_name, 64);
    push_u16(&mut oversized_name, 64);
    oversized_name.extend_from_slice(&CANONICAL_PF);
    push_u32(&mut oversized_name, 65_537);

    let mut truncated_name = handshake_ok_prefix();
    push_u16(&mut truncated_name, 64);
    push_u16(&mut truncated_name, 64);
    truncated_name.extend_from_slice(&CANONICAL_PF);
    push_u32(&mut truncated_name, 8);
    truncated_name.extend_from_slice(b"abc");

    let mut zero_reason = BANNER_38.to_vec();
    zero_reason.push(0);
    zero_reason.extend_from_slice(&bounded_text(DENY));

    let mut result_failure = BANNER_38.to_vec();
    result_failure.extend_from_slice(&[1, 2]);
    push_u32(&mut result_failure, 1);
    result_failure.extend_from_slice(&bounded_text(DENY));

    let mut no_vncauth = BANNER_38.to_vec();
    no_vncauth.extend_from_slice(&[1, 5, 6]);

    vec![
        candidate(
            "rfb_handshake",
            "valid-3.8-vncauth-init.bin",
            Category::Ok,
            None,
            "valid 3.8 type-2 handshake and 64x64 ServerInit",
            {
                let mut bytes = handshake_ok_prefix();
                bytes.extend_from_slice(&server_init(64, 64, CANONICAL_PF, SYNTHETIC));
                bytes
            },
        ),
        candidate(
            "rfb_handshake",
            "banner-malformed.bin",
            Category::ProtocolBanner,
            None,
            "wrong banner prefix",
            b"NOT RFB DATA".to_vec(),
        ),
        candidate(
            "rfb_handshake",
            "banner-unsupported-version.bin",
            Category::ProtocolBanner,
            None,
            "unsupported RFB 3.1 banner",
            b"RFB 003.001\n".to_vec(),
        ),
        candidate(
            "rfb_handshake",
            "security-no-vncauth.bin",
            Category::SecurityAllowlist,
            None,
            "security types without type 2",
            no_vncauth,
        ),
        candidate(
            "rfb_handshake",
            "security-zero-reason.bin",
            Category::SecurityFailure,
            None,
            "security count 0 with deny reason",
            zero_reason,
        ),
        candidate(
            "rfb_handshake",
            "security-result-failure-reason.bin",
            Category::SecurityFailure,
            None,
            "security result failure with deny reason",
            result_failure,
        ),
        candidate(
            "rfb_handshake",
            "init-oversized-dimensions.bin",
            Category::Limit,
            None,
            "ServerInit 8193x1 exceeds dimension limit",
            {
                let mut bytes = handshake_ok_prefix();
                bytes.extend_from_slice(&server_init(8193, 1, CANONICAL_PF, SYNTHETIC));
                bytes
            },
        ),
        candidate(
            "rfb_handshake",
            "init-invalid-pixel-format.bin",
            Category::ServerInit,
            None,
            "overlapping red and green masks",
            {
                let mut bytes = handshake_ok_prefix();
                bytes.extend_from_slice(&server_init(
                    64,
                    64,
                    overlapping_pixel_format(),
                    SYNTHETIC,
                ));
                bytes
            },
        ),
        candidate(
            "rfb_handshake",
            "init-oversized-name.bin",
            Category::Limit,
            None,
            "desktop name length 65537",
            oversized_name,
        ),
        candidate(
            "rfb_handshake",
            "init-truncated-name.bin",
            Category::IoEof,
            None,
            "declared name length 8 with 3 bytes",
            truncated_name,
        ),
    ]
}

fn mixed_encodings() -> Vec<u8> {
    let mut bytes = fb_update(5);
    bytes.extend_from_slice(&rect_header(0, 0, 1, 1, messages::encoding::RAW));
    bytes.extend_from_slice(&pixel32(3, 2, 1));
    bytes.extend_from_slice(&rect_header(1, 0, 1, 1, messages::encoding::COPY_RECT));
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    bytes.extend_from_slice(&rect_header(2, 0, 1, 1, messages::encoding::HEXTILE));
    bytes.push(1);
    bytes.extend_from_slice(&pixel32(9, 8, 7));
    bytes.extend_from_slice(&rect_header(3, 0, 1, 1, messages::encoding::ZRLE));
    let mut zrle = vec![0];
    zrle.extend_from_slice(&cpixel(12, 11, 10));
    bytes.extend_from_slice(&zrle_wire(&zrle));
    bytes.extend_from_slice(&rect_header(4, 0, 1, 1, messages::encoding::TIGHT));
    bytes.push(0x80);
    bytes.extend_from_slice(&tpixel(15, 14, 13));
    bytes
}

fn session_candidates() -> Vec<Candidate> {
    let mut raw = fb_update(1);
    raw.extend_from_slice(&rect_header(0, 0, 1, 1, messages::encoding::RAW));
    raw.extend_from_slice(&pixel32(0x11, 0x22, 0x33));

    let over_count = fb_update(4_097);

    let mut out_of_bounds = fb_update(1);
    out_of_bounds.extend_from_slice(&rect_header(63, 0, 2, 1, messages::encoding::RAW));
    out_of_bounds.extend_from_slice(&pixel32(1, 2, 3));

    let mut unknown_encoding = fb_update(1);
    unknown_encoding.extend_from_slice(&rect_header(0, 0, 1, 1, 99));

    let mut desktop = fb_update(1);
    desktop.extend_from_slice(&rect_header(0, 0, 32, 32, messages::encoding::DESKTOP_SIZE));

    let mut extended = fb_update(1);
    extended.extend_from_slice(&rect_header(
        0,
        0,
        32,
        32,
        messages::encoding::EXTENDED_DESKTOP_SIZE,
    ));
    extended.extend_from_slice(&[1, 0, 0, 0]);
    push_u32(&mut extended, 0);
    push_u16(&mut extended, 0);
    push_u16(&mut extended, 0);
    push_u16(&mut extended, 32);
    push_u16(&mut extended, 32);
    push_u32(&mut extended, 0);

    let mut extended_malformed = fb_update(1);
    extended_malformed.extend_from_slice(&rect_header(
        0,
        0,
        32,
        32,
        messages::encoding::EXTENDED_DESKTOP_SIZE,
    ));
    extended_malformed.extend_from_slice(&[0, 0, 0, 0]);

    let mut clipboard = vec![messages::server_msg::SERVER_CUT_TEXT, 0, 0, 0];
    clipboard.extend_from_slice(&bounded_text(SYNTHETIC));

    let mut clipboard_invalid = vec![messages::server_msg::SERVER_CUT_TEXT, 0, 0, 0];
    clipboard_invalid.extend_from_slice(&bounded_text(&[0x66, 0x80]));

    let mut clipboard_over = vec![messages::server_msg::SERVER_CUT_TEXT, 0, 0, 0];
    push_u32(&mut clipboard_over, 1_048_577);

    let mut colour_map = vec![messages::server_msg::SET_COLOUR_MAP_ENTRIES, 0];
    push_u16(&mut colour_map, 0);
    push_u16(&mut colour_map, 1);
    colour_map.extend_from_slice(&[0, 1, 0, 2, 0, 3]);

    let mut truncated = fb_update(1);
    truncated.extend_from_slice(&[0, 0, 0, 0, 0]);

    vec![
        candidate(
            "rfb_session",
            "valid-raw-rect.bin",
            Category::IoEof,
            Some("framebuffer_event"),
            "one RAW 1x1 rectangle",
            raw,
        ),
        candidate(
            "rfb_session",
            "valid-mixed-encodings.bin",
            Category::IoEof,
            Some("framebuffer_event"),
            "RAW CopyRect Hextile ZRLE Tight 1x1",
            mixed_encodings(),
        ),
        candidate(
            "rfb_session",
            "rect-count-over-limit.bin",
            Category::Limit,
            None,
            "rectangle count 4097",
            over_count,
        ),
        candidate(
            "rfb_session",
            "rect-out-of-bounds.bin",
            Category::Protocol,
            None,
            "rectangle 63,0,2,1 exceeds 64x64",
            out_of_bounds,
        ),
        candidate(
            "rfb_session",
            "unknown-encoding.bin",
            Category::Protocol,
            None,
            "encoding 99",
            unknown_encoding,
        ),
        candidate(
            "rfb_session",
            "desktop-size-resize.bin",
            Category::IoEof,
            Some("desktop_size_event,framebuffer_resized"),
            "DesktopSize 32x32",
            desktop,
        ),
        candidate(
            "rfb_session",
            "extended-desktop-size-valid.bin",
            Category::IoEof,
            Some("desktop_size_event,framebuffer_resized"),
            "ExtendedDesktopSize one 32x32 screen",
            extended,
        ),
        candidate(
            "rfb_session",
            "extended-desktop-size-malformed.bin",
            Category::Protocol,
            None,
            "ExtendedDesktopSize with zero screens",
            extended_malformed,
        ),
        candidate(
            "rfb_session",
            "clipboard-valid-utf8.bin",
            Category::IoEof,
            None,
            "SERVER_CUT_TEXT synthetic UTF-8",
            clipboard,
        ),
        candidate(
            "rfb_session",
            "clipboard-invalid-utf8.bin",
            Category::Protocol,
            None,
            "SERVER_CUT_TEXT invalid UTF-8",
            clipboard_invalid,
        ),
        candidate(
            "rfb_session",
            "clipboard-over-limit.bin",
            Category::Limit,
            None,
            "SERVER_CUT_TEXT declared 1048577",
            clipboard_over,
        ),
        candidate(
            "rfb_session",
            "colour-map-entries.bin",
            Category::IoEof,
            None,
            "SET_COLOUR_MAP_ENTRIES count 1",
            colour_map,
        ),
        candidate(
            "rfb_session",
            "unknown-message-type.bin",
            Category::Protocol,
            None,
            "unknown server message 0x7f",
            vec![0x7f],
        ),
        candidate(
            "rfb_session",
            "bell.bin",
            Category::IoEof,
            None,
            "Bell message",
            vec![messages::server_msg::BELL],
        ),
        candidate(
            "rfb_session",
            "truncated-rect-header.bin",
            Category::IoEof,
            None,
            "FB_UPDATE count 1 with 5 header bytes",
            truncated,
        ),
    ]
}

fn zrle_candidates() -> Vec<Candidate> {
    let mut raw_tile = vec![0];
    for _ in 0..256 {
        raw_tile.extend_from_slice(&cpixel(1, 2, 3));
    }
    let mut packed = vec![2];
    packed.extend_from_slice(&cpixel(1, 2, 3));
    packed.extend_from_slice(&cpixel(4, 5, 6));
    packed.extend_from_slice(&[0_u8; 32]);
    let mut palette_index = vec![3];
    palette_index.extend_from_slice(&cpixel(1, 2, 3));
    palette_index.extend_from_slice(&cpixel(4, 5, 6));
    palette_index.extend_from_slice(&cpixel(7, 8, 9));
    palette_index.extend_from_slice(&[0xc0, 0, 0, 0]);
    let mut palette_rle = vec![130];
    palette_rle.extend_from_slice(&cpixel(1, 2, 3));
    palette_rle.extend_from_slice(&cpixel(4, 5, 6));
    palette_rle.extend_from_slice(&[0x80, 0xff, 0x00]);
    let mut plain_rle = vec![128];
    plain_rle.extend_from_slice(&cpixel(1, 2, 3));
    plain_rle.extend_from_slice(&[0xff, 0x00]);
    let mut run_beyond = vec![128];
    run_beyond.extend_from_slice(&cpixel(1, 2, 3));
    run_beyond.extend_from_slice(&[0xff, 0x01]);
    let mut trailing = vec![1];
    trailing.extend_from_slice(&cpixel(1, 2, 3));
    trailing.push(0);
    let mut declared = Vec::new();
    push_u32(&mut declared, 67_108_865);
    let mut truncated = Vec::new();
    push_u32(&mut truncated, 10);
    truncated.extend_from_slice(&[1, 2, 3]);
    let mut no_progress = Vec::new();
    let bogus = [0x78, 0x9c, 0xff];
    push_u32(&mut no_progress, bogus.len() as u32);
    no_progress.extend_from_slice(&bogus);

    vec![
        candidate(
            "rfb_zrle",
            "raw-tile.bin",
            Category::Ok,
            None,
            "16x16 raw ZRLE tile",
            zrle_wire(&raw_tile),
        ),
        candidate(
            "rfb_zrle",
            "solid-tile.bin",
            Category::Ok,
            None,
            "solid ZRLE tile",
            zrle_wire(&[vec![1], cpixel(1, 2, 3).to_vec()].concat()),
        ),
        candidate(
            "rfb_zrle",
            "packed-palette-2.bin",
            Category::Ok,
            None,
            "1-bit packed palette",
            zrle_wire(&packed),
        ),
        candidate(
            "rfb_zrle",
            "plain-rle.bin",
            Category::Ok,
            None,
            "plain RLE run 256",
            zrle_wire(&plain_rle),
        ),
        candidate(
            "rfb_zrle",
            "palette-rle.bin",
            Category::Ok,
            None,
            "palette RLE run 256",
            zrle_wire(&palette_rle),
        ),
        candidate(
            "rfb_zrle",
            "palette-index-out-of-range.bin",
            Category::Decoder,
            None,
            "packed palette index 3 of 3",
            zrle_wire(&palette_index),
        ),
        candidate(
            "rfb_zrle",
            "run-beyond-tile.bin",
            Category::Decoder,
            None,
            "RLE run 257 exceeds 256 pixels",
            zrle_wire(&run_beyond),
        ),
        candidate(
            "rfb_zrle",
            "trailing-data.bin",
            Category::Decoder,
            None,
            "solid tile with trailing byte",
            zrle_wire(&trailing),
        ),
        candidate(
            "rfb_zrle",
            "empty-decoded.bin",
            Category::Decoder,
            None,
            "zlib of empty decoded stream",
            zrle_wire(&[]),
        ),
        candidate(
            "rfb_zrle",
            "no-progress-zlib.bin",
            Category::Decoder,
            None,
            "malformed zlib stream",
            no_progress,
        ),
        candidate(
            "rfb_zrle",
            "declared-over-limit.bin",
            Category::Limit,
            None,
            "declared compressed length 67108865",
            declared,
        ),
        candidate(
            "rfb_zrle",
            "truncated-compressed.bin",
            Category::IoEof,
            None,
            "declared 10 compressed bytes, 3 present",
            truncated,
        ),
    ]
}

fn tight_candidates() -> Vec<Candidate> {
    let fill = {
        let mut payload = vec![0x80];
        payload.extend_from_slice(&tpixel(1, 2, 3));
        tight_seed(0, &payload)
    };
    let copy_uncompressed = {
        let mut payload = vec![0x00];
        payload.extend_from_slice(&tpixel(1, 2, 3));
        tight_seed(0, &payload)
    };
    let copy_compressed = {
        let decoded = vec![1_u8; 768];
        let compressed = zlib(&decoded);
        let mut payload = vec![0x10];
        payload.extend_from_slice(&compact_length(compressed.len() as u32));
        payload.extend_from_slice(&compressed);
        tight_seed(4, &payload)
    };
    let palette2 = {
        let mut payload = vec![0x40, 0x01, 1];
        payload.extend_from_slice(&tpixel(1, 2, 3));
        payload.extend_from_slice(&tpixel(4, 5, 6));
        payload.push(0x00);
        tight_seed(0, &payload)
    };
    let palette2_compressed = {
        let compressed = zlib(&[0_u8; 32]);
        let mut payload = vec![0x40, 0x01, 1];
        payload.extend_from_slice(&tpixel(1, 2, 3));
        payload.extend_from_slice(&tpixel(4, 5, 6));
        payload.extend_from_slice(&compact_length(compressed.len() as u32));
        payload.extend_from_slice(&compressed);
        tight_seed(4, &payload)
    };
    let palette256 = {
        let mut payload = vec![0x40, 0x01, 255];
        for index in 0..256_u16 {
            payload.extend_from_slice(&tpixel(index as u8, 0, 0));
        }
        payload.push(0x00);
        tight_seed(0, &payload)
    };
    let gradient = {
        let compressed = zlib(&[0_u8; 768]);
        let mut payload = vec![0x40, 0x02];
        payload.extend_from_slice(&compact_length(compressed.len() as u32));
        payload.extend_from_slice(&compressed);
        tight_seed(4, &payload)
    };
    let jpeg_ok = {
        let encoded = jpeg_rgb(16, 16, [1, 2, 3]);
        let mut payload = vec![0x90];
        payload.extend_from_slice(&compact_length(encoded.len() as u32));
        payload.extend_from_slice(&encoded);
        tight_seed(4, &payload)
    };
    let jpeg_wrong = {
        let encoded = jpeg_rgb(17, 16, [1, 2, 3]);
        let mut payload = vec![0x90];
        payload.extend_from_slice(&compact_length(encoded.len() as u32));
        payload.extend_from_slice(&encoded);
        tight_seed(4, &payload)
    };
    let jpeg_truncated = tight_seed(4, &[0x90, 0x02, 0xff, 0xd8]);
    let compact_noncanonical = tight_seed(4, &[0x10, 0x80, 0x00]);
    let compact_truncated = tight_seed(4, &[0x10, 0x80]);
    let method10 = tight_seed(4, &[0xa0]);
    let mismatch = {
        let compressed = zlib(&[0_u8; 767]);
        let mut payload = vec![0x10];
        payload.extend_from_slice(&compact_length(compressed.len() as u32));
        payload.extend_from_slice(&compressed);
        tight_seed(4, &payload)
    };

    vec![
        candidate(
            "rfb_tight",
            "fill.bin",
            Category::Ok,
            None,
            "Tight fill 1x1",
            fill,
        ),
        candidate(
            "rfb_tight",
            "basic-copy-uncompressed.bin",
            Category::Ok,
            None,
            "uncompressed CopyFilter 1x1",
            copy_uncompressed,
        ),
        candidate(
            "rfb_tight",
            "basic-copy-compressed.bin",
            Category::Ok,
            None,
            "compressed CopyFilter 16x16",
            copy_compressed,
        ),
        candidate(
            "rfb_tight",
            "palette-2.bin",
            Category::Ok,
            None,
            "uncompressed 2-colour palette 1x1",
            palette2,
        ),
        candidate(
            "rfb_tight",
            "palette-2-compressed.bin",
            Category::Ok,
            None,
            "compressed 2-colour palette 16x16",
            palette2_compressed,
        ),
        candidate(
            "rfb_tight",
            "palette-256.bin",
            Category::Ok,
            None,
            "uncompressed 256-colour palette 1x1",
            palette256,
        ),
        candidate(
            "rfb_tight",
            "gradient.bin",
            Category::Ok,
            None,
            "compressed gradient 16x16",
            gradient,
        ),
        candidate(
            "rfb_tight",
            "jpeg-16x16.bin",
            Category::Ok,
            None,
            "JPEG 16x16",
            jpeg_ok,
        ),
        candidate(
            "rfb_tight",
            "jpeg-wrong-dimensions.bin",
            Category::Decoder,
            None,
            "JPEG 17x16 for 16x16 rect",
            jpeg_wrong,
        ),
        candidate(
            "rfb_tight",
            "jpeg-truncated.bin",
            Category::Decoder,
            None,
            "truncated JPEG SOI",
            jpeg_truncated,
        ),
        candidate(
            "rfb_tight",
            "compact-length-noncanonical.bin",
            Category::Decoder,
            None,
            "non-canonical compact length 0",
            compact_noncanonical,
        ),
        candidate(
            "rfb_tight",
            "compact-length-truncated.bin",
            Category::IoEof,
            None,
            "truncated two-byte compact length",
            compact_truncated,
        ),
        candidate(
            "rfb_tight",
            "method-10-no-zlib.bin",
            Category::Decoder,
            None,
            "Tight method 10",
            method10,
        ),
        candidate(
            "rfb_tight",
            "output-length-mismatch.bin",
            Category::Decoder,
            None,
            "zlib output 767 for 768 expected",
            mismatch,
        ),
    ]
}

fn hextile_candidates() -> Vec<Candidate> {
    let bg = pixel32(1, 2, 3);
    let fg = pixel32(4, 5, 6);
    let raw_tile = {
        let mut bytes = vec![0x01];
        bytes.extend_from_slice(&[0_u8; 1024]);
        bytes.push(0x02);
        bytes.extend_from_slice(&bg);
        bytes
    };
    let background = {
        let mut bytes = vec![0x02];
        bytes.extend_from_slice(&bg);
        bytes.push(0x02);
        bytes.extend_from_slice(&bg);
        bytes
    };
    let bg_fg = {
        let mut bytes = vec![0x0e];
        bytes.extend_from_slice(&bg);
        bytes.extend_from_slice(&fg);
        bytes.extend_from_slice(&[1, 0x00, 0x00]);
        bytes.push(0x02);
        bytes.extend_from_slice(&bg);
        bytes
    };
    let coloured = {
        let mut bytes = vec![0x1a];
        bytes.extend_from_slice(&bg);
        bytes.push(1);
        bytes.extend_from_slice(&fg);
        bytes.extend_from_slice(&[0x00, 0x00]);
        bytes.push(0x02);
        bytes.extend_from_slice(&bg);
        bytes
    };
    let raw_irrelevant = {
        let mut bytes = vec![0xff];
        bytes.extend_from_slice(&[0_u8; 1024]);
        bytes.push(0x02);
        bytes.extend_from_slice(&bg);
        bytes
    };
    let mut out_of_tile = vec![0x0e];
    out_of_tile.extend_from_slice(&bg);
    out_of_tile.extend_from_slice(&fg);
    out_of_tile.extend_from_slice(&[1, 0xf0, 0xff]);
    let full_raw = {
        let mut bytes = vec![0x01];
        bytes.extend_from_slice(&[0_u8; 1024]);
        bytes.push(0x01);
        bytes.extend_from_slice(&[0_u8; 1024]);
        bytes
    };

    vec![
        candidate(
            "rfb_hextile",
            "raw-tile.bin",
            Category::Ok,
            None,
            "raw 16x16 plus background tile",
            raw_tile,
        ),
        candidate(
            "rfb_hextile",
            "background-fill.bin",
            Category::Ok,
            None,
            "two background-specified tiles",
            background,
        ),
        candidate(
            "rfb_hextile",
            "bg-fg-subrects.bin",
            Category::Ok,
            None,
            "background foreground one subrect",
            bg_fg,
        ),
        candidate(
            "rfb_hextile",
            "coloured-subrects.bin",
            Category::Ok,
            None,
            "coloured subrect plus background tile",
            coloured,
        ),
        candidate(
            "rfb_hextile",
            "raw-with-irrelevant-bits.bin",
            Category::Ok,
            None,
            "raw tile with extra subtype bits",
            raw_irrelevant,
        ),
        candidate(
            "rfb_hextile",
            "subrect-out-of-tile.bin",
            Category::Decoder,
            None,
            "subrect exceeds 16x16 tile",
            out_of_tile,
        ),
        candidate(
            "rfb_hextile",
            "missing-background.bin",
            Category::Decoder,
            None,
            "subtype 0 without carried background",
            vec![0x00],
        ),
        candidate(
            "rfb_hextile",
            "foreground-without-spec.bin",
            Category::Decoder,
            None,
            "subrects without foreground",
            vec![0x08, 1, 0, 0],
        ),
        candidate(
            "rfb_hextile",
            "invalid-subtype-bits.bin",
            Category::Decoder,
            None,
            "unknown subtype bit 0x20",
            vec![0x20],
        ),
        candidate(
            "rfb_hextile",
            "truncated-raw.bin",
            Category::IoEof,
            None,
            "raw subtype with 3 pixel bytes",
            vec![0x01, 1, 2, 3],
        ),
        candidate(
            "rfb_hextile",
            "full-raw-over-budget.bin",
            Category::Limit,
            None,
            "two raw tiles exceed 2048 budget",
            full_raw,
        ),
    ]
}

pub fn all_candidates() -> Vec<Candidate> {
    handshake_candidates()
        .into_iter()
        .chain(session_candidates())
        .chain(zrle_candidates())
        .chain(tight_candidates())
        .chain(hextile_candidates())
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn validate_output_dir(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("output directory must be an absolute path".to_string());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("output directory must not contain ..".to_string());
    }
    if !path.is_dir() {
        return Err("output directory does not exist".to_string());
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| "could not canonicalize output directory".to_string())?;
    let forbidden = [
        PathBuf::from("/"),
        PathBuf::from("/tmp"),
        PathBuf::from("/var"),
        PathBuf::from("/private"),
        PathBuf::from("/Users"),
        PathBuf::from("/Users/Adam.Gell"),
        PathBuf::from("/Users/Adam.Gell/.local"),
        PathBuf::from("/Users/Adam.Gell/.config"),
        PathBuf::from("/Users/Adam.Gell/Desktop"),
        PathBuf::from("/opt"),
        PathBuf::from("/opt/homebrew"),
    ];
    if forbidden.iter().any(|item| item == &canonical) {
        return Err("refusing unsafe or broad output directory".to_string());
    }
    if let Ok(home) = std::env::var("HOME") {
        if Path::new(&home) == canonical {
            return Err("refusing home directory".to_string());
        }
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "could not locate repository root".to_string())?;
    if let Ok(repo) = repo.canonicalize() {
        if canonical == repo || canonical.starts_with(&repo) {
            return Err("refusing repository path".to_string());
        }
    }
    Ok(canonical)
}

pub fn write_candidates(output_dir: &Path) -> Result<CorpusManifest, String> {
    let output_dir = validate_output_dir(output_dir)?;
    let candidates = all_candidates();
    if candidates.len() != 62 {
        return Err(format!(
            "expected 62 candidates, generated {}",
            candidates.len()
        ));
    }
    for target in CANONICAL_TARGETS {
        fs::create_dir_all(output_dir.join(target))
            .map_err(|_| format!("could not create {target} directory"))?;
    }
    let mut seeds = Vec::new();
    for candidate in candidates {
        let relative = format!("{}/{}", candidate.target, candidate.name);
        let path = output_dir.join(candidate.target).join(candidate.name);
        fs::write(&path, &candidate.bytes).map_err(|_| format!("could not write {relative}"))?;
        seeds.push(SeedRecord {
            target: candidate.target.to_string(),
            file: relative,
            sha256: sha256_hex(&candidate.bytes),
            length: candidate.bytes.len() as u64,
            category: candidate.category,
            transition: candidate.transition.map(str::to_string),
            fixture: None,
            reason: None,
            behavior: candidate.behavior.to_string(),
        });
    }
    let manifest = CorpusManifest {
        version: 1,
        targets: CANONICAL_TARGETS
            .iter()
            .map(|target| (*target).to_string())
            .collect(),
        seeds,
    };
    let encoded = serde_json::to_string_pretty(&manifest)
        .map_err(|_| "could not encode candidate manifest".to_string())?;
    fs::write(output_dir.join("candidate-manifest.json"), encoded)
        .map_err(|_| "could not write candidate-manifest.json".to_string())?;
    Ok(manifest)
}

fn resolve_seed_path(root: &Path, file: &str) -> Result<(PathBuf, String), String> {
    let relative = Path::new(file);
    if relative.is_absolute() {
        return Err("relative".to_string());
    }
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("unsafe".to_string());
    }
    let parts: Vec<_> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect();
    let (target, name) = match parts.as_slice() {
        [target, name] => (*target, *name),
        ["corpus", target, name] => (*target, *name),
        _ => return Err("path".to_string()),
    };
    if !CANONICAL_TARGETS.contains(&target) {
        return Err("unknown".to_string());
    }
    if !is_allowed_seed_name(name) {
        return Err("filename".to_string());
    }
    let joined = root.join(relative);
    let canonical_root = root.canonicalize().map_err(|_| "root".to_string())?;
    if joined.exists() {
        let canonical = joined.canonicalize().map_err(|_| "canonical".to_string())?;
        if !canonical.starts_with(&canonical_root) {
            return Err("escaped".to_string());
        }
        Ok((canonical, target.to_string()))
    } else {
        Ok((joined, target.to_string()))
    }
}

pub fn verify_manifest(manifest: &CorpusManifest, root: &Path) -> Result<(), Vec<String>> {
    let mut failures = Vec::new();
    let mut passes = Vec::new();
    let mut seen_files = HashSet::new();
    let mut seen_paths = HashSet::new();
    if manifest.targets
        != CANONICAL_TARGETS
            .iter()
            .map(|target| (*target).to_string())
            .collect::<Vec<_>>()
    {
        failures.push("manifest targets drifted from the canonical five".to_string());
    }
    for seed in &manifest.seeds {
        if !seen_files.insert(seed.file.as_str()) {
            failures.push(fail_seed("duplicate-file"));
            continue;
        }
        if !CANONICAL_TARGETS.contains(&seed.target.as_str()) {
            failures.push(fail_seed("unknown-target"));
            continue;
        }
        let (path, file_target) = match resolve_seed_path(root, &seed.file) {
            Ok(resolved) => resolved,
            Err(error) => {
                failures.push(fail_seed(&error));
                continue;
            }
        };
        if !seen_paths.insert(path.clone()) {
            failures.push(fail_seed("duplicate-file"));
            continue;
        }
        if seed.target != file_target {
            failures.push(fail_seed("target-mismatch"));
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => {
                failures.push(fail_seed("missing"));
                continue;
            }
        };
        if bytes.len() as u64 != seed.length {
            failures.push(fail_seed("length-mismatch"));
            continue;
        }
        let digest = sha256_hex(&bytes);
        if digest != seed.sha256 {
            failures.push(fail_seed("hash-mismatch"));
            continue;
        }
        match execute_target(&seed.target, &bytes) {
            Ok(execution) => {
                if execution.category != seed.category {
                    failures.push(fail_seed(&format!(
                        "expected {} got {}",
                        seed.category.as_str(),
                        execution.category.as_str()
                    )));
                    continue;
                }
                if let Some(expected) = &seed.transition {
                    let missing: Vec<&str> = expected
                        .split(',')
                        .filter(|item| {
                            !item.is_empty()
                                && !execution
                                    .transitions
                                    .iter()
                                    .any(|observed| observed == item)
                        })
                        .collect();
                    if !missing.is_empty() {
                        failures.push(fail_seed("transition-mismatch"));
                        continue;
                    }
                }
                passes.push(format!(
                    "PASS {}/{} {}",
                    seed.target,
                    seed_name(&seed.file),
                    execution.category.as_str()
                ));
            }
            Err(error) => failures.push(fail_seed(&error)),
        }
    }
    if failures.is_empty() {
        for pass in passes {
            println!("{pass}");
        }
        Ok(())
    } else {
        Err(failures)
    }
}

fn seed_name(file: &str) -> &str {
    let name = Path::new(file)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if is_allowed_seed_name(name) {
        name
    } else {
        "invalid-name"
    }
}

fn is_allowed_seed_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn fail_seed(reason: &str) -> String {
    format!("FAIL seed {reason}")
}

pub fn load_manifest(path: &Path) -> Result<CorpusManifest, String> {
    let bytes = fs::read(path).map_err(|_| "could not read manifest".to_string())?;
    serde_json::from_slice(&bytes).map_err(|_| "could not parse manifest".to_string())
}
