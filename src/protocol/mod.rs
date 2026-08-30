pub mod client;
pub mod encoding;
pub mod messages;
pub mod security;

/// Object-safe alias for a bidirectional async stream. The security handshake
/// returns a buffered stream for the client after negotiation.
pub trait AsyncRw: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AsyncRw for T {}
