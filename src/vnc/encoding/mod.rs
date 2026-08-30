use tokio::io::AsyncRead;

use super::{
    messages::PixelFormat, CheckedRect, Framebuffer, RfbError, RfbErrorKind, RfbPhase, RfbReader,
};

pub mod copyrect;
pub mod hextile;
pub mod raw;
pub mod tight;
pub mod zrle;

pub(crate) fn checked_destination(
    framebuffer: &Framebuffer,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> Result<CheckedRect, RfbError> {
    CheckedRect::new(
        x,
        y,
        width,
        height,
        framebuffer.width(),
        framebuffer.height(),
    )
    .map_err(map_framebuffer_error)
}

pub(crate) fn commit(
    framebuffer: &mut Framebuffer,
    rectangle: CheckedRect,
    rgba: &[u8],
) -> Result<(), RfbError> {
    framebuffer
        .write_rgba(rectangle, rgba)
        .map_err(map_framebuffer_error)
}

pub(crate) fn map_framebuffer_error(error: RfbError) -> RfbError {
    match error.kind() {
        RfbErrorKind::Limit => RfbError::limit(RfbPhase::Encoding, "framebuffer bounds"),
        RfbErrorKind::Allocation => RfbError::allocation(RfbPhase::Encoding, "framebuffer storage"),
        _ => RfbError::new(
            RfbPhase::Encoding,
            RfbErrorKind::Protocol,
            "framebuffer bounds",
        ),
    }
}

pub fn decode_desktop_size(
    framebuffer: &mut Framebuffer,
    width: u16,
    height: u16,
) -> Result<(), RfbError> {
    framebuffer
        .resize(width, height)
        .map_err(map_framebuffer_error)
}

pub async fn decode_cursor<S>(
    reader: &mut RfbReader<S>,
    pixel_format: &PixelFormat,
    hotspot_x: u16,
    hotspot_y: u16,
    width: u16,
    height: u16,
) -> Result<(), RfbError>
where
    S: AsyncRead + Unpin,
{
    let _hotspot = (hotspot_x, hotspot_y);
    pixel_format.validate_for_phase(RfbPhase::Encoding)?;
    let limits = reader.limits();
    limits.validate_for_phase(RfbPhase::Encoding)?;
    if width > limits.max_dimension || height > limits.max_dimension {
        return Err(RfbError::limit(RfbPhase::Encoding, "cursor dimensions"));
    }
    let bytes_per_pixel = u64::try_from(pixel_format.bytes_per_pixel()?)
        .map_err(|_| RfbError::limit(RfbPhase::Encoding, "cursor pixel width"))?;
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "cursor pixels"))?;
    if pixels > limits.max_pixels {
        return Err(RfbError::limit(RfbPhase::Encoding, "cursor pixels"));
    }
    let image_bytes = pixels
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "cursor image"))?;
    if image_bytes > limits.max_framebuffer_bytes {
        return Err(RfbError::limit(RfbPhase::Encoding, "cursor image"));
    }
    let mask_row_bytes = u64::from(width)
        .checked_add(7)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "cursor mask"))?
        / 8;
    let mask_bytes = mask_row_bytes
        .checked_mul(u64::from(height))
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "cursor mask"))?;
    let declared = image_bytes
        .checked_add(mask_bytes)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "cursor payload"))?;
    let bytes = reader
        .read_bounded_bytes(
            declared,
            u64::from(limits.max_encoded_rect_bytes),
            "cursor payload",
            RfbPhase::Encoding,
        )
        .await?;
    drop(bytes);
    Ok(())
}
