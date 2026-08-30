use tokio::io::{AsyncRead, AsyncReadExt};

use crate::vnc::{
    encoding::{checked_destination, commit},
    messages::PixelFormat,
    wire::allocate_zeroed,
    Framebuffer, RfbError, RfbPhase, RfbReader,
};

const RAW: u8 = 0x01;
const BACKGROUND_SPECIFIED: u8 = 0x02;
const FOREGROUND_SPECIFIED: u8 = 0x04;
const ANY_SUBRECTS: u8 = 0x08;
const SUBRECTS_COLOURED: u8 = 0x10;
const KNOWN_NONRAW_BITS: u8 =
    BACKGROUND_SPECIFIED | FOREGROUND_SPECIFIED | ANY_SUBRECTS | SUBRECTS_COLOURED;

pub async fn decode<S>(
    reader: &mut RfbReader<S>,
    framebuffer: &mut Framebuffer,
    pixel_format: &PixelFormat,
    rectangle_x: u16,
    rectangle_y: u16,
    rectangle_width: u16,
    rectangle_height: u16,
) -> Result<(), RfbError>
where
    S: AsyncRead + Unpin,
{
    pixel_format.validate_for_phase(RfbPhase::Encoding)?;
    reader.limits().validate_for_phase(RfbPhase::Encoding)?;
    let rectangle = checked_destination(
        framebuffer,
        rectangle_x,
        rectangle_y,
        rectangle_width,
        rectangle_height,
    )?;
    let rgba_length = rectangle.expected_rgba_bytes(RfbPhase::Encoding)?;
    let mut rgba = allocate_zeroed(rgba_length, RfbPhase::Encoding, "hextile RGBA")?;
    let bytes_per_pixel = pixel_format.bytes_per_pixel()?;
    let mut budget = ReadBudget::new(reader.limits().max_encoded_rect_bytes);
    let mut background = None;
    let mut foreground = None;

    let mut tile_y = 0_u16;
    while tile_y < rectangle_height {
        let tile_height = 16_u16.min(rectangle_height - tile_y);
        let mut tile_x = 0_u16;
        while tile_x < rectangle_width {
            let tile_width = 16_u16.min(rectangle_width - tile_x);
            let subtype = read_u8(reader, &mut budget, "hextile subtype").await?;
            if subtype & RAW != 0 {
                background = None;
                foreground = None;
                let raw_length = usize::from(tile_width)
                    .checked_mul(usize::from(tile_height))
                    .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile raw tile"))?;
                let mut raw = allocate_zeroed(raw_length, RfbPhase::Encoding, "hextile raw")?;
                read_exact(reader, &mut budget, &mut raw, "hextile raw").await?;
                convert_raw_tile(
                    &raw,
                    &mut rgba,
                    pixel_format,
                    tile_x,
                    tile_y,
                    tile_width,
                    tile_height,
                    rectangle_width,
                )?;
                tile_x = tile_x
                    .checked_add(tile_width)
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile tile x"))?;
                continue;
            }

            if subtype & !KNOWN_NONRAW_BITS != 0
                || subtype & FOREGROUND_SPECIFIED != 0 && subtype & SUBRECTS_COLOURED != 0
            {
                return Err(RfbError::decoder("hextile subtype"));
            }
            if subtype & BACKGROUND_SPECIFIED != 0 {
                background = Some(read_colour(reader, &mut budget, pixel_format).await?);
            }
            let tile_background =
                background.ok_or_else(|| RfbError::decoder("hextile background"))?;
            if subtype & FOREGROUND_SPECIFIED != 0 {
                foreground = Some(read_colour(reader, &mut budget, pixel_format).await?);
            }
            fill_tile(
                &mut rgba,
                rectangle_width,
                tile_x,
                tile_y,
                tile_width,
                tile_height,
                tile_background,
            )?;

            if subtype & ANY_SUBRECTS != 0 {
                let count = read_u8(reader, &mut budget, "hextile subrectangle count").await?;
                let coloured = subtype & SUBRECTS_COLOURED != 0;
                if coloured {
                    foreground = None;
                } else if count != 0 && foreground.is_none() {
                    return Err(RfbError::decoder("hextile foreground"));
                }
                for _ in 0..count {
                    let colour = if coloured {
                        read_colour(reader, &mut budget, pixel_format).await?
                    } else {
                        foreground.ok_or_else(|| RfbError::decoder("hextile foreground"))?
                    };
                    let position = read_u8(reader, &mut budget, "hextile subrectangle").await?;
                    let dimensions = read_u8(reader, &mut budget, "hextile subrectangle").await?;
                    let sub_x = u16::from(position >> 4);
                    let sub_y = u16::from(position & 0x0f);
                    let sub_width = u16::from(dimensions >> 4)
                        .checked_add(1)
                        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile width"))?;
                    let sub_height = u16::from(dimensions & 0x0f)
                        .checked_add(1)
                        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile height"))?;
                    let right = sub_x
                        .checked_add(sub_width)
                        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile bounds"))?;
                    let bottom = sub_y
                        .checked_add(sub_height)
                        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile bounds"))?;
                    if right > tile_width || bottom > tile_height {
                        return Err(RfbError::decoder("hextile subrectangle bounds"));
                    }
                    fill_tile(
                        &mut rgba,
                        rectangle_width,
                        tile_x
                            .checked_add(sub_x)
                            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile x"))?,
                        tile_y
                            .checked_add(sub_y)
                            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile y"))?,
                        sub_width,
                        sub_height,
                        colour,
                    )?;
                }
            }

            tile_x = tile_x
                .checked_add(tile_width)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile tile x"))?;
        }
        tile_y = tile_y
            .checked_add(tile_height)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile tile y"))?;
    }
    commit(framebuffer, rectangle, &rgba)
}

struct ReadBudget {
    consumed: u64,
    limit: u64,
}

impl ReadBudget {
    fn new(limit: u32) -> Self {
        Self {
            consumed: 0,
            limit: u64::from(limit),
        }
    }

    fn consume(&mut self, length: usize, field: &'static str) -> Result<(), RfbError> {
        let length =
            u64::try_from(length).map_err(|_| RfbError::limit(RfbPhase::Encoding, field))?;
        let next = self
            .consumed
            .checked_add(length)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, field))?;
        if next > self.limit {
            return Err(RfbError::limit(RfbPhase::Encoding, field));
        }
        self.consumed = next;
        Ok(())
    }
}

async fn read_u8<S>(
    reader: &mut RfbReader<S>,
    budget: &mut ReadBudget,
    field: &'static str,
) -> Result<u8, RfbError>
where
    S: AsyncRead + Unpin,
{
    budget.consume(1, field)?;
    reader
        .read_u8()
        .await
        .map_err(|source| RfbError::io(RfbPhase::Encoding, source))
}

async fn read_exact<S>(
    reader: &mut RfbReader<S>,
    budget: &mut ReadBudget,
    destination: &mut [u8],
    field: &'static str,
) -> Result<(), RfbError>
where
    S: AsyncRead + Unpin,
{
    budget.consume(destination.len(), field)?;
    reader
        .read_exact(destination)
        .await
        .map_err(|source| RfbError::io(RfbPhase::Encoding, source))?;
    Ok(())
}

async fn read_colour<S>(
    reader: &mut RfbReader<S>,
    budget: &mut ReadBudget,
    pixel_format: &PixelFormat,
) -> Result<[u8; 4], RfbError>
where
    S: AsyncRead + Unpin,
{
    let bytes_per_pixel = pixel_format.bytes_per_pixel()?;
    let mut bytes = [0_u8; 4];
    read_exact(
        reader,
        budget,
        &mut bytes[..bytes_per_pixel],
        "hextile colour",
    )
    .await?;
    let pixel = pixel_format.read_pixel(&bytes)?;
    let (red, green, blue) = pixel_format.to_rgb(pixel);
    Ok([red, green, blue, 255])
}

#[allow(clippy::too_many_arguments)]
fn convert_raw_tile(
    raw: &[u8],
    rgba: &mut [u8],
    pixel_format: &PixelFormat,
    tile_x: u16,
    tile_y: u16,
    tile_width: u16,
    tile_height: u16,
    rectangle_width: u16,
) -> Result<(), RfbError> {
    let bytes_per_pixel = pixel_format.bytes_per_pixel()?;
    for row in 0..tile_height {
        for column in 0..tile_width {
            let tile_pixel = usize::from(row)
                .checked_mul(usize::from(tile_width))
                .and_then(|offset| offset.checked_add(usize::from(column)))
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile raw source"))?;
            let source = tile_pixel
                .checked_mul(bytes_per_pixel)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile raw source"))?;
            let pixel = pixel_format.read_pixel(
                raw.get(source..)
                    .ok_or_else(|| RfbError::decoder("hextile raw source"))?,
            )?;
            let (red, green, blue) = pixel_format.to_rgb(pixel);
            set_rgba(
                rgba,
                rectangle_width,
                tile_x
                    .checked_add(column)
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile raw x"))?,
                tile_y
                    .checked_add(row)
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile raw y"))?,
                [red, green, blue, 255],
            )?;
        }
    }
    Ok(())
}

fn fill_tile(
    rgba: &mut [u8],
    rectangle_width: u16,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    colour: [u8; 4],
) -> Result<(), RfbError> {
    for row in 0..height {
        for column in 0..width {
            set_rgba(
                rgba,
                rectangle_width,
                x.checked_add(column)
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile fill x"))?,
                y.checked_add(row)
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile fill y"))?,
                colour,
            )?;
        }
    }
    Ok(())
}

fn set_rgba(
    rgba: &mut [u8],
    rectangle_width: u16,
    x: u16,
    y: u16,
    colour: [u8; 4],
) -> Result<(), RfbError> {
    let pixel = usize::from(y)
        .checked_mul(usize::from(rectangle_width))
        .and_then(|offset| offset.checked_add(usize::from(x)))
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile target"))?;
    let start = pixel
        .checked_mul(4)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile target"))?;
    let end = start
        .checked_add(4)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "hextile target"))?;
    let destination = rgba
        .get_mut(start..end)
        .ok_or_else(|| RfbError::decoder("hextile target"))?;
    destination.copy_from_slice(&colour);
    Ok(())
}
