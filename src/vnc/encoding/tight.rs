use std::io::Cursor;

use flate2::{Decompress, FlushDecompress};
use image::{codecs::jpeg::JpegDecoder, ColorType, ImageDecoder};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::vnc::{
    encoding::{checked_destination, commit, validate_session_framebuffer},
    messages::PixelFormat,
    wire::allocate_zeroed,
    Framebuffer, RfbError, RfbPhase, RfbReader,
};

const MAX_TIGHT_WIDTH: u16 = 2_048;
const MIN_TO_COMPRESS: usize = 12;
const FILTER_COPY: u8 = 0;
const FILTER_PALETTE: u8 = 1;
const FILTER_GRADIENT: u8 = 2;
type GradientPixel = (u32, u32, u32);

pub struct TightState {
    streams: [Decompress; 4],
}

impl TightState {
    pub fn new() -> Self {
        Self {
            streams: [
                Decompress::new(true),
                Decompress::new(true),
                Decompress::new(true),
                Decompress::new(true),
            ],
        }
    }

    fn reset(&mut self, index: usize) -> Result<(), RfbError> {
        *self
            .streams
            .get_mut(index)
            .ok_or_else(|| RfbError::decoder("Tight stream reset"))? = Decompress::new(true);
        Ok(())
    }
}

impl Default for TightState {
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
    state: &mut TightState,
) -> Result<(), RfbError>
where
    S: AsyncRead + Unpin,
{
    pixel_format.validate_for_phase(RfbPhase::Encoding)?;
    validate_session_framebuffer(reader, framebuffer)?;
    let rectangle = checked_destination(framebuffer, x, y, width, height)?;
    if width > MAX_TIGHT_WIDTH {
        return Err(RfbError::limit(RfbPhase::Encoding, "Tight rectangle width"));
    }
    let mut budget = TightBudget::new(reader.limits().max_encoded_rect_bytes);
    let control = read_u8(reader, &mut budget, "Tight control").await?;
    for index in 0..4 {
        if control & (1_u8 << index) != 0 {
            state.reset(index)?;
        }
    }
    let method = control >> 4;
    let rgba = match method {
        0..=3 => {
            let stream = usize::from(method);
            let decompressor = state
                .streams
                .get_mut(stream)
                .ok_or_else(|| RfbError::decoder("Tight stream"))?;
            basic(
                reader,
                pixel_format,
                width,
                height,
                false,
                decompressor,
                &mut budget,
            )
            .await?
        }
        4..=7 => {
            let stream = usize::from(method & 0x03);
            let decompressor = state
                .streams
                .get_mut(stream)
                .ok_or_else(|| RfbError::decoder("Tight stream"))?;
            basic(
                reader,
                pixel_format,
                width,
                height,
                true,
                decompressor,
                &mut budget,
            )
            .await?
        }
        8 => fill(reader, pixel_format, width, height, &mut budget).await?,
        9 if matches!(pixel_format.bits_per_pixel, 16 | 32) => {
            jpeg(reader, width, height, &mut budget).await?
        }
        9 => return Err(RfbError::decoder("Tight JPEG pixel format")),
        10 | 14 => return Err(RfbError::decoder("Tight without zlib")),
        _ => return Err(RfbError::decoder("Tight compression method")),
    };
    commit(framebuffer, rectangle, &rgba)
}

async fn fill<S>(
    reader: &mut RfbReader<S>,
    pixel_format: &PixelFormat,
    width: u16,
    height: u16,
    budget: &mut TightBudget,
) -> Result<Vec<u8>, RfbError>
where
    S: AsyncRead + Unpin,
{
    let colour = read_tpixel(reader, pixel_format, budget, "Tight fill").await?;
    let pixels = checked_pixels(width, height, "Tight fill pixels")?;
    let length = pixels
        .checked_mul(4)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight fill RGBA"))?;
    let mut rgba = allocate_zeroed(length, RfbPhase::Encoding, "Tight fill RGBA")?;
    for destination in rgba.chunks_exact_mut(4) {
        destination.copy_from_slice(&colour);
    }
    Ok(rgba)
}

async fn jpeg<S>(
    reader: &mut RfbReader<S>,
    width: u16,
    height: u16,
    budget: &mut TightBudget,
) -> Result<Vec<u8>, RfbError>
where
    S: AsyncRead + Unpin,
{
    let declared = read_compact_length_with_budget(reader, budget).await?;
    budget.consume(
        usize::try_from(declared)
            .map_err(|_| RfbError::limit(RfbPhase::Encoding, "Tight JPEG length"))?,
        "Tight JPEG length",
    )?;
    let encoded = reader
        .read_bounded_bytes(
            u64::from(declared),
            u64::from(reader.limits().max_encoded_rect_bytes),
            "Tight JPEG",
            RfbPhase::Encoding,
        )
        .await?;
    let decoder = JpegDecoder::new(Cursor::new(encoded.as_slice()))
        .map_err(|_| RfbError::decoder("Tight JPEG header"))?;
    if decoder.dimensions() != (u32::from(width), u32::from(height)) {
        return Err(RfbError::decoder("Tight JPEG dimensions"));
    }
    let colour_type = decoder.color_type();
    let channels = match colour_type {
        ColorType::L8 => 1_usize,
        ColorType::Rgb8 => 3,
        _ => return Err(RfbError::decoder("Tight JPEG colour type")),
    };
    let pixels = checked_pixels(width, height, "Tight JPEG pixels")?;
    let decoded_length = pixels
        .checked_mul(channels)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight JPEG output"))?;
    if u64::try_from(decoded_length)
        .map_err(|_| RfbError::limit(RfbPhase::Encoding, "Tight JPEG output"))?
        != decoder.total_bytes()
    {
        return Err(RfbError::decoder("Tight JPEG output"));
    }
    let rgba_length = pixels
        .checked_mul(4)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight JPEG RGBA"))?;
    if u64::try_from(rgba_length)
        .map_err(|_| RfbError::limit(RfbPhase::Encoding, "Tight JPEG RGBA"))?
        > reader.limits().max_framebuffer_bytes
    {
        return Err(RfbError::limit(RfbPhase::Encoding, "Tight JPEG RGBA"));
    }
    let mut decoded = allocate_zeroed(decoded_length, RfbPhase::Encoding, "Tight JPEG output")?;
    decoder
        .read_image(&mut decoded)
        .map_err(|_| RfbError::decoder("Tight JPEG decode"))?;
    let mut rgba = allocate_zeroed(rgba_length, RfbPhase::Encoding, "Tight JPEG RGBA")?;
    match colour_type {
        ColorType::L8 => {
            for (value, destination) in decoded.iter().zip(rgba.chunks_exact_mut(4)) {
                destination.copy_from_slice(&[*value, *value, *value, 255]);
            }
        }
        ColorType::Rgb8 => {
            for (source, destination) in decoded.chunks_exact(3).zip(rgba.chunks_exact_mut(4)) {
                let [red, green, blue] = source else {
                    return Err(RfbError::decoder("Tight JPEG output"));
                };
                destination.copy_from_slice(&[*red, *green, *blue, 255]);
            }
        }
        _ => return Err(RfbError::decoder("Tight JPEG colour type")),
    }
    Ok(rgba)
}

#[allow(clippy::too_many_arguments)]
async fn basic<S>(
    reader: &mut RfbReader<S>,
    pixel_format: &PixelFormat,
    width: u16,
    height: u16,
    has_filter: bool,
    decompressor: &mut Decompress,
    budget: &mut TightBudget,
) -> Result<Vec<u8>, RfbError>
where
    S: AsyncRead + Unpin,
{
    let filter = if has_filter {
        read_u8(reader, budget, "Tight filter").await?
    } else {
        FILTER_COPY
    };
    let pixels = checked_pixels(width, height, "Tight pixels")?;
    match filter {
        FILTER_COPY => {
            let expected = pixels
                .checked_mul(pixel_format.tight_bytes_per_pixel()?)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight CopyFilter"))?;
            let filtered =
                read_filtered(reader, expected, decompressor, budget, "Tight CopyFilter").await?;
            tpixels_to_rgba(&filtered, pixel_format, pixels)
        }
        FILTER_PALETTE => {
            let palette_size = usize::from(read_u8(reader, budget, "Tight palette size").await?)
                .checked_add(1)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight palette size"))?;
            if palette_size < 2 {
                return Err(RfbError::decoder("Tight palette size"));
            }
            let mut palette = Vec::new();
            palette
                .try_reserve_exact(palette_size)
                .map_err(|_| RfbError::allocation(RfbPhase::Encoding, "Tight palette"))?;
            for _ in 0..palette_size {
                palette.push(read_tpixel(reader, pixel_format, budget, "Tight palette").await?);
            }
            let (expected, one_bit) = if palette_size == 2 {
                let row_bytes = usize::from(width)
                    .checked_add(7)
                    .map(|value| value / 8)
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight palette row"))?;
                (
                    row_bytes.checked_mul(usize::from(height)).ok_or_else(|| {
                        RfbError::limit(RfbPhase::Encoding, "Tight palette indexes")
                    })?,
                    true,
                )
            } else {
                (pixels, false)
            };
            let indexes = read_filtered(
                reader,
                expected,
                decompressor,
                budget,
                "Tight palette indexes",
            )
            .await?;
            palette_to_rgba(&indexes, &palette, width, height, one_bit)
        }
        FILTER_GRADIENT => {
            if !matches!(pixel_format.bits_per_pixel, 16 | 32) {
                return Err(RfbError::decoder("Tight GradientFilter format"));
            }
            let expected = pixels
                .checked_mul(pixel_format.tight_bytes_per_pixel()?)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight GradientFilter"))?;
            let filtered = read_filtered(
                reader,
                expected,
                decompressor,
                budget,
                "Tight GradientFilter",
            )
            .await?;
            gradient_to_rgba(&filtered, pixel_format, width, height)
        }
        _ => Err(RfbError::decoder("Tight filter")),
    }
}

async fn read_filtered<S>(
    reader: &mut RfbReader<S>,
    expected: usize,
    decompressor: &mut Decompress,
    budget: &mut TightBudget,
    field: &'static str,
) -> Result<Vec<u8>, RfbError>
where
    S: AsyncRead + Unpin,
{
    if expected < MIN_TO_COMPRESS {
        let mut bytes = allocate_zeroed(expected, RfbPhase::Encoding, field)?;
        read_exact(reader, budget, &mut bytes, field).await?;
        return Ok(bytes);
    }
    let declared = read_compact_length_with_budget(reader, budget).await?;
    let declared = usize::try_from(declared)
        .map_err(|_| RfbError::limit(RfbPhase::Encoding, "Tight compressed length"))?;
    budget.consume(declared, "Tight compressed length")?;
    let compressed = reader
        .read_bounded_bytes(
            u64::try_from(declared)
                .map_err(|_| RfbError::limit(RfbPhase::Encoding, "Tight compressed length"))?,
            u64::from(reader.limits().max_encoded_rect_bytes),
            "Tight compressed payload",
            RfbPhase::Encoding,
        )
        .await?;
    decompress_exact(decompressor, &compressed, expected)
}

fn decompress_exact(
    decompressor: &mut Decompress,
    compressed: &[u8],
    expected: usize,
) -> Result<Vec<u8>, RfbError> {
    let mut output = allocate_zeroed(expected, RfbPhase::Encoding, "Tight output")?;
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
                    .ok_or_else(|| RfbError::decoder("Tight compressed input"))?,
                &mut scratch,
                FlushDecompress::Sync,
            )
            .map_err(|_| RfbError::decoder("Tight decompression"))?;
        let consumed = usize::try_from(decompressor.total_in() - before_input)
            .map_err(|_| RfbError::limit(RfbPhase::Encoding, "Tight input progress"))?;
        let produced = usize::try_from(decompressor.total_out() - before_output)
            .map_err(|_| RfbError::limit(RfbPhase::Encoding, "Tight output progress"))?;
        input_offset = input_offset
            .checked_add(consumed)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight input progress"))?;
        let output_end = output_offset
            .checked_add(produced)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight output progress"))?;
        if output_end > expected {
            return Err(RfbError::decoder("Tight output length"));
        }
        output
            .get_mut(output_offset..output_end)
            .ok_or_else(|| RfbError::decoder("Tight output"))?
            .copy_from_slice(
                scratch
                    .get(..produced)
                    .ok_or_else(|| RfbError::decoder("Tight output"))?,
            );
        output_offset = output_end;
        if consumed == 0 && produced == 0 {
            if input_offset == compressed.len() {
                break;
            }
            return Err(RfbError::decoder("Tight decompressor progress"));
        }
        if input_offset == compressed.len() && produced < scratch.len() {
            break;
        }
    }
    if input_offset != compressed.len() || output_offset != expected {
        return Err(RfbError::decoder("Tight output length"));
    }
    Ok(output)
}

fn tpixels_to_rgba(
    filtered: &[u8],
    pixel_format: &PixelFormat,
    pixels: usize,
) -> Result<Vec<u8>, RfbError> {
    let tight_width = pixel_format.tight_bytes_per_pixel()?;
    let rgba_length = pixels
        .checked_mul(4)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight RGBA"))?;
    let mut rgba = allocate_zeroed(rgba_length, RfbPhase::Encoding, "Tight RGBA")?;
    for index in 0..pixels {
        let source = index
            .checked_mul(tight_width)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight source"))?;
        let colour = pixel_format.read_tpixel(
            filtered
                .get(source..)
                .ok_or_else(|| RfbError::decoder("Tight source"))?,
        )?;
        let destination = index
            .checked_mul(4)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight target"))?;
        let end = destination
            .checked_add(4)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight target"))?;
        rgba.get_mut(destination..end)
            .ok_or_else(|| RfbError::decoder("Tight target"))?
            .copy_from_slice(&[colour.0, colour.1, colour.2, 255]);
    }
    Ok(rgba)
}

fn palette_to_rgba(
    indexes: &[u8],
    palette: &[[u8; 4]],
    width: u16,
    height: u16,
    one_bit: bool,
) -> Result<Vec<u8>, RfbError> {
    let pixels = checked_pixels(width, height, "Tight palette pixels")?;
    let mut rgba = allocate_zeroed(
        pixels
            .checked_mul(4)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight palette RGBA"))?,
        RfbPhase::Encoding,
        "Tight palette RGBA",
    )?;
    let row_bytes = usize::from(width)
        .checked_add(7)
        .map(|value| value / 8)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight palette row"))?;
    for row in 0..height {
        for column in 0..width {
            let pixel = usize::from(row)
                .checked_mul(usize::from(width))
                .and_then(|offset| offset.checked_add(usize::from(column)))
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight palette pixel"))?;
            let index = if one_bit {
                let byte_offset = usize::from(row)
                    .checked_mul(row_bytes)
                    .and_then(|offset| offset.checked_add(usize::from(column) / 8))
                    .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight palette index"))?;
                let byte = *indexes
                    .get(byte_offset)
                    .ok_or_else(|| RfbError::decoder("Tight palette index"))?;
                usize::from((byte >> (7 - usize::from(column) % 8)) & 1)
            } else {
                usize::from(
                    *indexes
                        .get(pixel)
                        .ok_or_else(|| RfbError::decoder("Tight palette index"))?,
                )
            };
            let colour = palette
                .get(index)
                .ok_or_else(|| RfbError::decoder("Tight palette index"))?;
            let destination = pixel
                .checked_mul(4)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight palette target"))?;
            let end = destination
                .checked_add(4)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight palette target"))?;
            rgba.get_mut(destination..end)
                .ok_or_else(|| RfbError::decoder("Tight palette target"))?
                .copy_from_slice(colour);
        }
    }
    Ok(rgba)
}

fn gradient_to_rgba(
    filtered: &[u8],
    pixel_format: &PixelFormat,
    width: u16,
    height: u16,
) -> Result<Vec<u8>, RfbError> {
    let pixels = checked_pixels(width, height, "Tight gradient pixels")?;
    let tight_width = pixel_format.tight_bytes_per_pixel()?;
    let row_width = usize::from(width);
    let mut previous = gradient_row(row_width)?;
    let mut current = gradient_row(row_width)?;
    let mut rgba = allocate_zeroed(
        pixels
            .checked_mul(4)
            .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight gradient RGBA"))?,
        RfbPhase::Encoding,
        "Tight gradient RGBA",
    )?;
    for row in 0..usize::from(height) {
        for column in 0..usize::from(width) {
            let pixel_index = row
                .checked_mul(usize::from(width))
                .and_then(|offset| offset.checked_add(column))
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight gradient pixel"))?;
            let source = pixel_index
                .checked_mul(tight_width)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight gradient source"))?;
            let source = filtered
                .get(source..)
                .ok_or_else(|| RfbError::decoder("Tight gradient source"))?;
            let differences = if tight_width == 3 {
                let bytes: &[u8; 3] = source
                    .get(..3)
                    .ok_or_else(|| RfbError::decoder("Tight gradient source"))?
                    .try_into()
                    .map_err(|_| RfbError::decoder("Tight gradient source"))?;
                (
                    u32::from(bytes[0]),
                    u32::from(bytes[1]),
                    u32::from(bytes[2]),
                )
            } else {
                let packed = pixel_format.read_pixel(source)?;
                (
                    (packed >> pixel_format.red_shift) & u32::from(pixel_format.red_max),
                    (packed >> pixel_format.green_shift) & u32::from(pixel_format.green_max),
                    (packed >> pixel_format.blue_shift) & u32::from(pixel_format.blue_max),
                )
            };
            let left = if column == 0 {
                (0, 0, 0)
            } else {
                current
                    .get(column - 1)
                    .copied()
                    .ok_or_else(|| RfbError::decoder("Tight gradient left"))?
            };
            let top = if row == 0 {
                (0, 0, 0)
            } else {
                previous
                    .get(column)
                    .copied()
                    .ok_or_else(|| RfbError::decoder("Tight gradient top"))?
            };
            let top_left = if row == 0 || column == 0 {
                (0, 0, 0)
            } else {
                previous
                    .get(column - 1)
                    .copied()
                    .ok_or_else(|| RfbError::decoder("Tight gradient top left"))?
            };
            let reconstructed = (
                reconstruct_component(
                    differences.0,
                    left.0,
                    top.0,
                    top_left.0,
                    u32::from(pixel_format.red_max),
                )?,
                reconstruct_component(
                    differences.1,
                    left.1,
                    top.1,
                    top_left.1,
                    u32::from(pixel_format.green_max),
                )?,
                reconstruct_component(
                    differences.2,
                    left.2,
                    top.2,
                    top_left.2,
                    u32::from(pixel_format.blue_max),
                )?,
            );
            *current
                .get_mut(column)
                .ok_or_else(|| RfbError::decoder("Tight gradient current"))? = reconstructed;
            let destination = pixel_index
                .checked_mul(4)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight gradient target"))?;
            let end = destination
                .checked_add(4)
                .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight gradient target"))?;
            rgba.get_mut(destination..end)
                .ok_or_else(|| RfbError::decoder("Tight gradient target"))?
                .copy_from_slice(&[
                    scale_component(reconstructed.0, u32::from(pixel_format.red_max))?,
                    scale_component(reconstructed.1, u32::from(pixel_format.green_max))?,
                    scale_component(reconstructed.2, u32::from(pixel_format.blue_max))?,
                    255,
                ]);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    Ok(rgba)
}

fn gradient_row(width: usize) -> Result<Vec<GradientPixel>, RfbError> {
    let mut row = Vec::new();
    row.try_reserve_exact(width)
        .map_err(|_| RfbError::allocation(RfbPhase::Encoding, "Tight gradient row"))?;
    row.resize(width, (0, 0, 0));
    Ok(row)
}

fn reconstruct_component(
    difference: u32,
    left: u32,
    top: u32,
    top_left: u32,
    maximum: u32,
) -> Result<u32, RfbError> {
    let predictor = i64::from(left)
        .checked_add(i64::from(top))
        .and_then(|value| value.checked_sub(i64::from(top_left)))
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight gradient predictor"))?
        .clamp(0, i64::from(maximum));
    let modulus = maximum
        .checked_add(1)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight gradient modulus"))?;
    difference
        .checked_add(
            u32::try_from(predictor)
                .map_err(|_| RfbError::limit(RfbPhase::Encoding, "Tight gradient predictor"))?,
        )
        .map(|value| value % modulus)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight gradient value"))
}

fn scale_component(value: u32, maximum: u32) -> Result<u8, RfbError> {
    let scaled = value
        .checked_mul(255)
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, "Tight gradient colour"))?
        / maximum;
    u8::try_from(scaled).map_err(|_| RfbError::limit(RfbPhase::Encoding, "Tight gradient colour"))
}

async fn read_tpixel<S>(
    reader: &mut RfbReader<S>,
    pixel_format: &PixelFormat,
    budget: &mut TightBudget,
    field: &'static str,
) -> Result<[u8; 4], RfbError>
where
    S: AsyncRead + Unpin,
{
    let width = pixel_format.tight_bytes_per_pixel()?;
    let mut bytes = [0_u8; 4];
    read_exact(reader, budget, &mut bytes[..width], field).await?;
    let (red, green, blue) = pixel_format.read_tpixel(&bytes)?;
    Ok([red, green, blue, 255])
}

fn checked_pixels(width: u16, height: u16, field: &'static str) -> Result<usize, RfbError> {
    usize::from(width)
        .checked_mul(usize::from(height))
        .ok_or_else(|| RfbError::limit(RfbPhase::Encoding, field))
}

pub async fn read_compact_length<S>(reader: &mut RfbReader<S>) -> Result<u32, RfbError>
where
    S: AsyncRead + Unpin,
{
    let mut budget = TightBudget::new(reader.limits().max_encoded_rect_bytes);
    read_compact_length_with_budget(reader, &mut budget).await
}

async fn read_compact_length_with_budget<S>(
    reader: &mut RfbReader<S>,
    budget: &mut TightBudget,
) -> Result<u32, RfbError>
where
    S: AsyncRead + Unpin,
{
    let first = read_u8(reader, budget, "Tight compact length").await?;
    if first & 0x80 == 0 {
        return Ok(u32::from(first));
    }
    let second = read_u8(reader, budget, "Tight compact length").await?;
    let mut value = u32::from(first & 0x7f) | (u32::from(second & 0x7f) << 7);
    if second & 0x80 == 0 {
        if value <= 127 {
            return Err(RfbError::decoder("Tight compact length"));
        }
        return Ok(value);
    }
    let third = read_u8(reader, budget, "Tight compact length").await?;
    value |= u32::from(third) << 14;
    if value <= 16_383 {
        return Err(RfbError::decoder("Tight compact length"));
    }
    Ok(value)
}

struct TightBudget {
    consumed: u64,
    limit: u64,
}

impl TightBudget {
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
    budget: &mut TightBudget,
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
    budget: &mut TightBudget,
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
