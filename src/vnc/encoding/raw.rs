use tokio::io::AsyncRead;

use crate::vnc::{
    encoding::{checked_destination, commit, validate_session_framebuffer},
    messages::PixelFormat,
    wire::allocate_zeroed,
    Framebuffer, RfbError, RfbPhase, RfbReader,
};

pub async fn decode<S>(
    reader: &mut RfbReader<S>,
    framebuffer: &mut Framebuffer,
    pixel_format: &PixelFormat,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> Result<(), RfbError>
where
    S: AsyncRead + Unpin,
{
    pixel_format.validate_for_phase(RfbPhase::Encoding)?;
    validate_session_framebuffer(reader, framebuffer)?;
    let rectangle = checked_destination(framebuffer, x, y, width, height)?;
    let pixel_count = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "raw pixels"))?;
    let bytes_per_pixel = u64::try_from(pixel_format.bytes_per_pixel()?)
        .map_err(|_| RfbError::limit(RfbPhase::Encoding, "raw pixel width"))?;
    let declared = pixel_count
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "raw payload"))?;
    let encoded = reader
        .read_bounded_bytes(
            declared,
            u64::from(reader.limits().max_encoded_rect_bytes),
            "raw payload",
            RfbPhase::Encoding,
        )
        .await?;

    let rgba_length = rectangle.expected_rgba_bytes(RfbPhase::Encoding)?;
    let mut rgba = allocate_zeroed(rgba_length, RfbPhase::Encoding, "raw RGBA")?;
    let bytes_per_pixel = usize::try_from(bytes_per_pixel)
        .map_err(|_| RfbError::limit(RfbPhase::Encoding, "raw pixel width"))?;
    let pixel_count = usize::try_from(pixel_count)
        .map_err(|_| RfbError::limit(RfbPhase::Encoding, "raw pixels"))?;
    for index in 0..pixel_count {
        let source_start = index
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "raw source"))?;
        let source = encoded
            .get(source_start..)
            .ok_or_else(|| RfbError::decoder("raw source"))?;
        let pixel = pixel_format.read_pixel(source)?;
        let (red, green, blue) = pixel_format.to_rgb(pixel);
        let destination_start = index
            .checked_mul(4)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "raw target"))?;
        let destination_end = destination_start
            .checked_add(4)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "raw target"))?;
        let destination = rgba
            .get_mut(destination_start..destination_end)
            .ok_or_else(|| RfbError::decoder("raw target"))?;
        destination.copy_from_slice(&[red, green, blue, 255]);
    }
    commit(framebuffer, rectangle, &rgba)
}
