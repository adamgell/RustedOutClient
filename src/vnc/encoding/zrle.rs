use flate2::{Decompress, FlushDecompress};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::vnc::{
    encoding::{checked_destination, commit},
    messages::PixelFormat,
    wire::allocate_zeroed,
    Framebuffer, RfbError, RfbPhase, RfbReader,
};

const TILE_EDGE: u16 = 64;
const MAX_PALETTE: usize = 127;

pub struct ZrleState {
    decompressor: Decompress,
}

impl ZrleState {
    pub fn new() -> Self {
        Self {
            decompressor: Decompress::new(true),
        }
    }
}

impl Default for ZrleState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn decode<S>(
    reader: &mut RfbReader<S>,
    framebuffer: &mut Framebuffer,
    pixel_format: &PixelFormat,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    state: &mut ZrleState,
) -> Result<(), RfbError>
where
    S: AsyncRead + Unpin,
{
    pixel_format.validate_for_phase(RfbPhase::Encoding)?;
    reader.limits().validate_for_phase(RfbPhase::Encoding)?;
    let rectangle = checked_destination(framebuffer, x, y, width, height)?;
    let maximum_output = max_decompressed_bytes(width, height, pixel_format)?;
    let declared = reader
        .read_u32()
        .await
        .map_err(|source| RfbError::io(RfbPhase::Encoding, source))?;
    let compressed = reader
        .read_bounded_bytes(
            u64::from(declared),
            u64::from(reader.limits().max_encoded_rect_bytes),
            "ZRLE compressed payload",
            RfbPhase::Encoding,
        )
        .await?;
    let decoded = decompress_bounded(&mut state.decompressor, &compressed, maximum_output)?;
    let rgba_length = rectangle.expected_rgba_bytes(RfbPhase::Encoding)?;
    let mut rgba = allocate_zeroed(rgba_length, RfbPhase::Encoding, "ZRLE RGBA")?;
    decode_tiles(&decoded, &mut rgba, pixel_format, width, height)?;
    commit(framebuffer, rectangle, &rgba)
}

/// Geometry-derived ceiling for a legal decompressed ZRLE tile stream.
///
/// For each tile this admits the largest of Raw, packed palette, plain RLE,
/// and palette RLE. Palette RLE includes the legal 127-entry palette even for
/// a tiny tile; plain RLE includes one terminating run byte per single-pixel
/// run. The bound is finite, checked, and independent of compressed input.
pub fn max_decompressed_bytes(
    width: u16,
    height: u16,
    pixel_format: &PixelFormat,
) -> Result<usize, RfbError> {
    pixel_format.validate_for_phase(RfbPhase::Encoding)?;
    if width == 0 || height == 0 {
        return Err(RfbError::decoder("ZRLE dimensions"));
    }
    let cpixel = pixel_format.cpixel_bytes()?;
    let mut total = 0_usize;
    let mut tile_y = 0_u16;
    while tile_y < height {
        let tile_height = TILE_EDGE.min(height - tile_y);
        let mut tile_x = 0_u16;
        while tile_x < width {
            let tile_width = TILE_EDGE.min(width - tile_x);
            let pixels = usize::from(tile_width)
                .checked_mul(usize::from(tile_height))
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE tile pixels"))?;
            let raw = 1_usize
                .checked_add(
                    pixels
                        .checked_mul(cpixel)
                        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE raw bound"))?,
                )
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE raw bound"))?;
            let packed_row = usize::from(tile_width)
                .checked_mul(4)
                .and_then(|bits| bits.checked_add(7))
                .map(|bits| bits / 8)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE packed bound"))?;
            let packed = 1_usize
                .checked_add(
                    16_usize
                        .checked_mul(cpixel)
                        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE packed bound"))?,
                )
                .and_then(|value| {
                    packed_row
                        .checked_mul(usize::from(tile_height))
                        .and_then(|indexes| value.checked_add(indexes))
                })
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE packed bound"))?;
            let plain_rle =
                1_usize
                    .checked_add(
                        pixels
                            .checked_mul(cpixel.checked_add(1).ok_or_else(|| {
                                RfbError::limit(RfbPhase::Encoding, "ZRLE RLE bound")
                            })?)
                            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE RLE bound"))?,
                    )
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE RLE bound"))?;
            let palette_rle =
                1_usize
                    .checked_add(MAX_PALETTE.checked_mul(cpixel).ok_or_else(|| {
                        RfbError::limit(RfbPhase::Encoding, "ZRLE palette RLE bound")
                    })?)
                    .and_then(|value| value.checked_add(pixels))
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE palette RLE bound"))?;
            let tile_bound = raw.max(packed).max(plain_rle).max(palette_rle);
            total = total
                .checked_add(tile_bound)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE output bound"))?;
            tile_x = tile_x
                .checked_add(tile_width)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE tile x"))?;
        }
        tile_y = tile_y
            .checked_add(tile_height)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE tile y"))?;
    }
    Ok(total)
}

fn decompress_bounded(
    decompressor: &mut Decompress,
    compressed: &[u8],
    maximum_output: usize,
) -> Result<Vec<u8>, RfbError> {
    let mut output = allocate_zeroed(maximum_output, RfbPhase::Encoding, "ZRLE output")?;
    let mut input_offset = 0_usize;
    let mut output_offset = 0_usize;
    let mut scratch = [0_u8; 8_192];

    loop {
        let before_input = decompressor.total_in();
        let before_output = decompressor.total_out();
        decompressor
            .decompress(
                compressed
                    .get(input_offset..)
                    .ok_or_else(|| RfbError::decoder("ZRLE compressed input"))?,
                &mut scratch,
                FlushDecompress::Sync,
            )
            .map_err(|_| RfbError::decoder("ZRLE decompression"))?;
        let consumed = usize::try_from(decompressor.total_in() - before_input)
            .map_err(|_| RfbError::limit(RfbPhase::Encoding, "ZRLE input progress"))?;
        let produced = usize::try_from(decompressor.total_out() - before_output)
            .map_err(|_| RfbError::limit(RfbPhase::Encoding, "ZRLE output progress"))?;
        input_offset = input_offset
            .checked_add(consumed)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE input progress"))?;
        let output_end = output_offset
            .checked_add(produced)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE output progress"))?;
        if output_end > maximum_output {
            return Err(RfbError::limit(RfbPhase::Encoding, "ZRLE output ceiling"));
        }
        let destination = output
            .get_mut(output_offset..output_end)
            .ok_or_else(|| RfbError::decoder("ZRLE output"))?;
        destination.copy_from_slice(
            scratch
                .get(..produced)
                .ok_or_else(|| RfbError::decoder("ZRLE output"))?,
        );
        output_offset = output_end;

        if consumed == 0 && produced == 0 {
            if input_offset == compressed.len() {
                break;
            }
            return Err(RfbError::decoder("ZRLE decompressor progress"));
        }
        if input_offset == compressed.len() && produced < scratch.len() {
            break;
        }
    }
    output.truncate(output_offset);
    Ok(output)
}

fn decode_tiles(
    decoded: &[u8],
    rgba: &mut [u8],
    pixel_format: &PixelFormat,
    rectangle_width: u16,
    rectangle_height: u16,
) -> Result<(), RfbError> {
    let mut input = SliceReader::new(decoded);
    let mut tile_y = 0_u16;
    while tile_y < rectangle_height {
        let tile_height = TILE_EDGE.min(rectangle_height - tile_y);
        let mut tile_x = 0_u16;
        while tile_x < rectangle_width {
            let tile_width = TILE_EDGE.min(rectangle_width - tile_x);
            decode_tile(
                &mut input,
                rgba,
                pixel_format,
                rectangle_width,
                tile_x,
                tile_y,
                tile_width,
                tile_height,
            )?;
            tile_x = tile_x
                .checked_add(tile_width)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE tile x"))?;
        }
        tile_y = tile_y
            .checked_add(tile_height)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE tile y"))?;
    }
    if !input.is_empty() {
        return Err(RfbError::decoder("ZRLE trailing data"));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_tile(
    input: &mut SliceReader<'_>,
    rgba: &mut [u8],
    pixel_format: &PixelFormat,
    rectangle_width: u16,
    tile_x: u16,
    tile_y: u16,
    tile_width: u16,
    tile_height: u16,
) -> Result<(), RfbError> {
    let subtype = input.read_u8("ZRLE subtype")?;
    let pixels = usize::from(tile_width)
        .checked_mul(usize::from(tile_height))
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE tile pixels"))?;
    match subtype {
        0 => {
            for index in 0..pixels {
                let colour = input.read_cpixel(pixel_format, "ZRLE raw pixel")?;
                set_tile_pixel(
                    rgba,
                    rectangle_width,
                    tile_x,
                    tile_y,
                    tile_width,
                    index,
                    colour,
                )?;
            }
        }
        1 => {
            let colour = input.read_cpixel(pixel_format, "ZRLE solid pixel")?;
            fill_tile(
                rgba,
                rectangle_width,
                tile_x,
                tile_y,
                tile_width,
                pixels,
                colour,
            )?;
        }
        2..=16 => decode_packed_palette(
            input,
            rgba,
            pixel_format,
            rectangle_width,
            tile_x,
            tile_y,
            tile_width,
            tile_height,
            usize::from(subtype),
        )?,
        17..=127 | 129 => return Err(RfbError::decoder("ZRLE subtype")),
        128 => decode_plain_rle(
            input,
            rgba,
            pixel_format,
            rectangle_width,
            tile_x,
            tile_y,
            tile_width,
            pixels,
        )?,
        130..=255 => decode_palette_rle(
            input,
            rgba,
            pixel_format,
            rectangle_width,
            tile_x,
            tile_y,
            tile_width,
            pixels,
            usize::from(subtype - 128),
        )?,
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_packed_palette(
    input: &mut SliceReader<'_>,
    rgba: &mut [u8],
    pixel_format: &PixelFormat,
    rectangle_width: u16,
    tile_x: u16,
    tile_y: u16,
    tile_width: u16,
    tile_height: u16,
    palette_size: usize,
) -> Result<(), RfbError> {
    let palette = input.read_palette(pixel_format, palette_size, "ZRLE palette")?;
    let bits = if palette_size == 2 {
        1_usize
    } else if palette_size <= 4 {
        2
    } else {
        4
    };
    let row_bits = usize::from(tile_width)
        .checked_mul(bits)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE packed row"))?;
    let row_bytes = row_bits
        .checked_add(7)
        .map(|value| value / 8)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE packed row"))?;
    for row in 0..tile_height {
        let row_data = input.read_bytes(row_bytes, "ZRLE packed indexes")?;
        for column in 0..tile_width {
            let bit_offset = usize::from(column)
                .checked_mul(bits)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE packed index"))?;
            let byte = *row_data
                .get(bit_offset / 8)
                .ok_or_else(|| RfbError::decoder("ZRLE packed index"))?;
            let shift = 8_usize
                .checked_sub(bits)
                .and_then(|value| value.checked_sub(bit_offset % 8))
                .ok_or_else(|| RfbError::decoder("ZRLE packed shift"))?;
            let mask = (1_u8 << bits) - 1;
            let palette_index = usize::from((byte >> shift) & mask);
            let colour = *palette
                .get(palette_index)
                .ok_or_else(|| RfbError::decoder("ZRLE palette index"))?;
            let pixel = usize::from(row)
                .checked_mul(usize::from(tile_width))
                .and_then(|offset| offset.checked_add(usize::from(column)))
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE packed pixel"))?;
            set_tile_pixel(
                rgba,
                rectangle_width,
                tile_x,
                tile_y,
                tile_width,
                pixel,
                colour,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_plain_rle(
    input: &mut SliceReader<'_>,
    rgba: &mut [u8],
    pixel_format: &PixelFormat,
    rectangle_width: u16,
    tile_x: u16,
    tile_y: u16,
    tile_width: u16,
    pixels: usize,
) -> Result<(), RfbError> {
    let mut written = 0_usize;
    while written < pixels {
        let colour = input.read_cpixel(pixel_format, "ZRLE RLE pixel")?;
        let run = input.read_run_length("ZRLE RLE run")?;
        let remaining = pixels - written;
        if run > remaining {
            return Err(RfbError::decoder("ZRLE RLE run"));
        }
        for offset in 0..run {
            set_tile_pixel(
                rgba,
                rectangle_width,
                tile_x,
                tile_y,
                tile_width,
                written
                    .checked_add(offset)
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE RLE pixel"))?,
                colour,
            )?;
        }
        written = written
            .checked_add(run)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE RLE run"))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_palette_rle(
    input: &mut SliceReader<'_>,
    rgba: &mut [u8],
    pixel_format: &PixelFormat,
    rectangle_width: u16,
    tile_x: u16,
    tile_y: u16,
    tile_width: u16,
    pixels: usize,
    palette_size: usize,
) -> Result<(), RfbError> {
    let palette = input.read_palette(pixel_format, palette_size, "ZRLE RLE palette")?;
    let mut written = 0_usize;
    while written < pixels {
        let encoded_index = input.read_u8("ZRLE palette RLE index")?;
        let palette_index = usize::from(encoded_index & 0x7f);
        let colour = *palette
            .get(palette_index)
            .ok_or_else(|| RfbError::decoder("ZRLE palette RLE index"))?;
        let run = if encoded_index & 0x80 == 0 {
            1
        } else {
            let run = input.read_run_length("ZRLE palette RLE run")?;
            if run == 1 {
                return Err(RfbError::decoder("ZRLE palette RLE run"));
            }
            run
        };
        let remaining = pixels - written;
        if run > remaining {
            return Err(RfbError::decoder("ZRLE palette RLE run"));
        }
        for offset in 0..run {
            set_tile_pixel(
                rgba,
                rectangle_width,
                tile_x,
                tile_y,
                tile_width,
                written
                    .checked_add(offset)
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE palette RLE pixel"))?,
                colour,
            )?;
        }
        written = written
            .checked_add(run)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE palette RLE run"))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn fill_tile(
    rgba: &mut [u8],
    rectangle_width: u16,
    tile_x: u16,
    tile_y: u16,
    tile_width: u16,
    pixels: usize,
    colour: [u8; 4],
) -> Result<(), RfbError> {
    for pixel in 0..pixels {
        set_tile_pixel(
            rgba,
            rectangle_width,
            tile_x,
            tile_y,
            tile_width,
            pixel,
            colour,
        )?;
    }
    Ok(())
}

fn set_tile_pixel(
    rgba: &mut [u8],
    rectangle_width: u16,
    tile_x: u16,
    tile_y: u16,
    tile_width: u16,
    tile_pixel: usize,
    colour: [u8; 4],
) -> Result<(), RfbError> {
    let row = tile_pixel / usize::from(tile_width);
    let column = tile_pixel % usize::from(tile_width);
    let x = usize::from(tile_x)
        .checked_add(column)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE target x"))?;
    let y = usize::from(tile_y)
        .checked_add(row)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE target y"))?;
    let pixel = y
        .checked_mul(usize::from(rectangle_width))
        .and_then(|offset| offset.checked_add(x))
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE target"))?;
    let start = pixel
        .checked_mul(4)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE target"))?;
    let end = start
        .checked_add(4)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "ZRLE target"))?;
    rgba.get_mut(start..end)
        .ok_or_else(|| RfbError::decoder("ZRLE target"))?
        .copy_from_slice(&colour);
    Ok(())
}

struct SliceReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> SliceReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }

    fn read_u8(&mut self, field: &'static str) -> Result<u8, RfbError> {
        let value = *self
            .bytes
            .get(self.position)
            .ok_or_else(|| RfbError::decoder(field))?;
        self.position = self
            .position
            .checked_add(1)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, field))?;
        Ok(value)
    }

    fn read_bytes(&mut self, length: usize, field: &'static str) -> Result<&'a [u8], RfbError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, field))?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| RfbError::decoder(field))?;
        self.position = end;
        Ok(bytes)
    }

    fn read_cpixel(
        &mut self,
        pixel_format: &PixelFormat,
        field: &'static str,
    ) -> Result<[u8; 4], RfbError> {
        let bytes = self.read_bytes(pixel_format.cpixel_bytes()?, field)?;
        let pixel = pixel_format.read_cpixel(bytes)?;
        let (red, green, blue) = pixel_format.to_rgb(pixel);
        Ok([red, green, blue, 255])
    }

    fn read_palette(
        &mut self,
        pixel_format: &PixelFormat,
        palette_size: usize,
        field: &'static str,
    ) -> Result<Vec<[u8; 4]>, RfbError> {
        if palette_size == 0 || palette_size > MAX_PALETTE {
            return Err(RfbError::decoder(field));
        }
        let mut palette = Vec::new();
        palette
            .try_reserve_exact(palette_size)
            .map_err(|_| RfbError::allocation(RfbPhase::Encoding, field))?;
        for _ in 0..palette_size {
            palette.push(self.read_cpixel(pixel_format, field)?);
        }
        Ok(palette)
    }

    fn read_run_length(&mut self, field: &'static str) -> Result<usize, RfbError> {
        let mut run = 1_usize;
        loop {
            let extension = usize::from(self.read_u8(field)?);
            run = run
                .checked_add(extension)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, field))?;
            if extension != 255 {
                return Ok(run);
            }
        }
    }
}
