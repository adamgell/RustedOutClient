use std::{
    fmt, io,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

use super::ProtocolLimits;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RfbPhase {
    Banner,
    SecurityTypes,
    Authentication,
    SecurityResult,
    ServerInit,
    Framebuffer,
    Encoding,
    Session,
    EventQueue,
    Cleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RfbErrorKind {
    ProtocolBanner,
    SecurityAllowlist,
    SecurityFailure,
    Limit,
    Allocation,
    ServerInit,
    Decoder,
    Protocol,
    Queue,
    Io,
}

/// Bounded, redacted protocol error. It deliberately retains only static field
/// labels and an I/O error kind, never remote bytes or credential material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RfbError {
    phase: RfbPhase,
    kind: RfbErrorKind,
    field: &'static str,
    io_kind: Option<io::ErrorKind>,
    cleanup_io_kind: Option<io::ErrorKind>,
}

impl RfbError {
    pub(crate) fn new(phase: RfbPhase, kind: RfbErrorKind, field: &'static str) -> Self {
        Self {
            phase,
            kind,
            field,
            io_kind: None,
            cleanup_io_kind: None,
        }
    }

    pub fn io(phase: RfbPhase, source: io::Error) -> Self {
        Self {
            phase,
            kind: RfbErrorKind::Io,
            field: "wire I/O",
            io_kind: Some(source.kind()),
            cleanup_io_kind: None,
        }
    }

    pub(crate) fn limit(phase: RfbPhase, field: &'static str) -> Self {
        Self::new(phase, RfbErrorKind::Limit, field)
    }

    pub(crate) fn allocation(phase: RfbPhase, field: &'static str) -> Self {
        Self::new(phase, RfbErrorKind::Allocation, field)
    }

    pub(crate) fn decoder(field: &'static str) -> Self {
        Self::new(RfbPhase::Encoding, RfbErrorKind::Decoder, field)
    }

    pub fn phase(&self) -> RfbPhase {
        self.phase
    }

    pub fn kind(&self) -> RfbErrorKind {
        self.kind
    }

    pub fn io_kind(&self) -> Option<io::ErrorKind> {
        self.io_kind
    }

    pub fn has_cleanup_failure(&self) -> bool {
        self.cleanup_io_kind.is_some()
    }

    pub fn cleanup_io_kind(&self) -> Option<io::ErrorKind> {
        self.cleanup_io_kind
    }

    pub(crate) fn with_cleanup_failure(mut self, source: io::Error) -> Self {
        self.cleanup_io_kind = Some(source.kind());
        self
    }
}

impl fmt::Display for RfbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "RFB {:?} during {:?} ({})",
            self.kind, self.phase, self.field
        )
    }
}

impl std::error::Error for RfbError {}

pub(crate) fn allocate_zeroed(
    length: usize,
    phase: RfbPhase,
    field: &'static str,
) -> Result<Vec<u8>, RfbError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| RfbError::allocation(phase, field))?;
    bytes.resize(length, 0);
    Ok(bytes)
}

pub(crate) fn sanitize_remote_text(
    bytes: &[u8],
    phase: RfbPhase,
    field: &'static str,
) -> Result<String, RfbError> {
    let mut sanitized = String::new();
    sanitized
        .try_reserve_exact(bytes.len())
        .map_err(|_| RfbError::allocation(phase, field))?;
    for byte in bytes {
        sanitized.push(if *byte == b' ' || byte.is_ascii_graphic() {
            char::from(*byte)
        } else {
            '?'
        });
    }
    Ok(sanitized)
}

pub struct RfbReader<S> {
    inner: S,
    limits: ProtocolLimits,
}

impl<S> RfbReader<S> {
    pub fn new(inner: S, limits: ProtocolLimits) -> Self {
        Self { inner, limits }
    }

    pub fn limits(&self) -> ProtocolLimits {
        self.limits
    }

    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: AsyncRead + Unpin> RfbReader<S> {
    pub async fn read_bounded_bytes(
        &mut self,
        declared: u64,
        limit: u64,
        field: &'static str,
        phase: RfbPhase,
    ) -> Result<Vec<u8>, RfbError> {
        if declared > limit {
            return Err(RfbError::limit(phase, field));
        }
        let length = usize::try_from(declared).map_err(|_| RfbError::limit(phase, field))?;
        let mut bytes = allocate_zeroed(length, phase, field)?;
        self.read_exact(&mut bytes)
            .await
            .map_err(|source| RfbError::io(phase, source))?;
        Ok(bytes)
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for RfbReader<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_read(context, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for RfbReader<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(context)
    }
}
