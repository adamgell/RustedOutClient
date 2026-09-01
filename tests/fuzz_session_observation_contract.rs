#![cfg(fuzzing)]

use std::{
    io::{Cursor, ErrorKind},
    pin::Pin,
    task::{Context, Poll},
};

use rustedoutclient::vnc::{
    fuzz_run_session, Framebuffer, FuzzSessionObservation, ProtocolLimits, RfbErrorKind, RfbPhase,
    RfbReader,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct SliceSink {
    input: Cursor<Vec<u8>>,
}

impl SliceSink {
    fn new(input: &[u8]) -> Self {
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
        let position = usize::try_from(this.input.position()).unwrap();
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

fn fuzz_limits() -> ProtocolLimits {
    ProtocolLimits {
        max_dimension: 64,
        max_pixels: 4_096,
        max_framebuffer_bytes: 16_384,
        max_text_bytes: 4_096,
        max_rectangles: 64,
        max_encoded_rect_bytes: 4_096,
        max_clipboard_bytes: 4_096,
    }
}

async fn observe(bytes: &[u8]) -> FuzzSessionObservation {
    let limits = fuzz_limits();
    let mut reader = RfbReader::new(SliceSink::new(bytes), limits);
    let mut framebuffer = Framebuffer::new(64, 64, limits).unwrap();
    fuzz_run_session(&mut reader, &mut framebuffer).await
}

#[tokio::test]
async fn finite_session_input_ends_as_typed_eof_not_a_disconnected_command_queue() {
    let observation = observe(&[2]).await;
    let error = observation.result.unwrap_err();

    assert_eq!(error.kind(), RfbErrorKind::Io);
    assert_eq!(error.phase(), RfbPhase::Session);
    assert_eq!(error.io_kind(), Some(ErrorKind::UnexpectedEof));
    assert_eq!(
        (observation.final_width, observation.final_height),
        (64, 64)
    );
    assert_eq!(observation.framebuffer_events, 0);
    assert_eq!(observation.desktop_size_events, 0);
    assert_eq!(observation.resize_outcome_events, 0);
}

#[tokio::test]
async fn completed_raw_rectangle_is_observed_without_exposing_its_pixels() {
    let bytes = [
        0, 0, 0, 1, // FramebufferUpdate, padding, one rectangle.
        0, 0, 0, 0, 0, 1, 0, 1, // x, y, width, height.
        0, 0, 0, 0, // RAW encoding.
        0x11, 0x22, 0x33, 0, // One canonical wire pixel.
    ];

    let observation = observe(&bytes).await;
    let error = observation.result.unwrap_err();

    assert_eq!(error.kind(), RfbErrorKind::Io);
    assert_eq!(error.io_kind(), Some(ErrorKind::UnexpectedEof));
    assert_eq!(observation.framebuffer_events, 1);
    assert_eq!(observation.desktop_size_events, 0);
    assert_eq!(observation.resize_outcome_events, 0);
}
