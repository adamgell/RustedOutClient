use tokio::io::{AsyncRead, AsyncReadExt};

use crate::vnc::{
    encoding::{checked_destination, map_framebuffer_error},
    Framebuffer, RfbError, RfbPhase, RfbReader,
};

pub async fn decode<S>(
    reader: &mut RfbReader<S>,
    framebuffer: &mut Framebuffer,
    destination_x: u16,
    destination_y: u16,
    width: u16,
    height: u16,
) -> Result<(), RfbError>
where
    S: AsyncRead + Unpin,
{
    let destination =
        checked_destination(framebuffer, destination_x, destination_y, width, height)?;
    let source_x = reader
        .read_u16()
        .await
        .map_err(|source| RfbError::io(RfbPhase::Encoding, source))?;
    let source_y = reader
        .read_u16()
        .await
        .map_err(|source| RfbError::io(RfbPhase::Encoding, source))?;
    framebuffer
        .copy_rect(destination, source_x, source_y)
        .map_err(map_framebuffer_error)
}
