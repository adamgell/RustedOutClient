use super::{RfbError, RfbErrorKind, RfbPhase};

/// RFB pixel format as negotiated during ServerInit.
#[derive(Clone, Debug)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    #[inline]
    pub fn to_rgb(&self, pixel: u32) -> (u8, u8, u8) {
        let r = ((pixel >> self.red_shift) & self.red_max as u32) * 255 / self.red_max as u32;
        let g = ((pixel >> self.green_shift) & self.green_max as u32) * 255 / self.green_max as u32;
        let b = ((pixel >> self.blue_shift) & self.blue_max as u32) * 255 / self.blue_max as u32;
        (r as u8, g as u8, b as u8)
    }

    pub(crate) fn validate_for_phase(&self, phase: RfbPhase) -> Result<(), RfbError> {
        let bits = self.bits_per_pixel;
        if !matches!(bits, 8 | 16 | 32) || self.depth == 0 || self.depth > bits || !self.true_colour
        {
            return Err(pixel_format_error(phase));
        }

        let (red_mask, red_bits) = checked_channel_mask(self.red_max, self.red_shift, bits)
            .ok_or_else(|| pixel_format_error(phase))?;
        let (green_mask, green_bits) = checked_channel_mask(self.green_max, self.green_shift, bits)
            .ok_or_else(|| pixel_format_error(phase))?;
        let (blue_mask, blue_bits) = checked_channel_mask(self.blue_max, self.blue_shift, bits)
            .ok_or_else(|| pixel_format_error(phase))?;
        let useful_bits = red_bits
            .checked_add(green_bits)
            .and_then(|count| count.checked_add(blue_bits))
            .ok_or_else(|| pixel_format_error(phase))?;
        if red_mask & green_mask != 0
            || red_mask & blue_mask != 0
            || green_mask & blue_mask != 0
            || useful_bits != u32::from(self.depth)
        {
            return Err(pixel_format_error(phase));
        }
        Ok(())
    }

    pub(crate) fn bytes_per_pixel(&self) -> Result<usize, RfbError> {
        self.validate_for_phase(RfbPhase::Encoding)?;
        Ok(usize::from(self.bits_per_pixel / 8))
    }

    pub(crate) fn read_pixel(&self, bytes: &[u8]) -> Result<u32, RfbError> {
        let expected = self.bytes_per_pixel()?;
        if bytes.len() < expected {
            return Err(RfbError::decoder("pixel bytes"));
        }
        read_pixel_width(bytes, expected, self.big_endian)
    }

    pub(crate) fn cpixel_bytes(&self) -> Result<usize, RfbError> {
        Ok(match self.cpixel_mode()? {
            CpixelMode::Full => self.bytes_per_pixel()?,
            CpixelMode::LeastThree | CpixelMode::MostThree => 3,
        })
    }

    pub(crate) fn read_cpixel(&self, bytes: &[u8]) -> Result<u32, RfbError> {
        match self.cpixel_mode()? {
            CpixelMode::Full => self.read_pixel(bytes),
            CpixelMode::LeastThree => {
                let bytes: [u8; 3] = bytes
                    .get(..3)
                    .ok_or_else(|| RfbError::decoder("CPIXEL bytes"))?
                    .try_into()
                    .map_err(|_| RfbError::decoder("CPIXEL bytes"))?;
                Ok(if self.big_endian {
                    u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]])
                } else {
                    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
                })
            }
            CpixelMode::MostThree => {
                let bytes: [u8; 3] = bytes
                    .get(..3)
                    .ok_or_else(|| RfbError::decoder("CPIXEL bytes"))?
                    .try_into()
                    .map_err(|_| RfbError::decoder("CPIXEL bytes"))?;
                Ok(if self.big_endian {
                    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], 0])
                } else {
                    u32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]])
                })
            }
        }
    }

    pub(crate) fn tight_bytes_per_pixel(&self) -> Result<usize, RfbError> {
        self.validate_for_phase(RfbPhase::Encoding)?;
        if self.true_colour
            && self.bits_per_pixel == 32
            && self.depth == 24
            && self.red_max == 255
            && self.green_max == 255
            && self.blue_max == 255
        {
            Ok(3)
        } else {
            self.bytes_per_pixel()
        }
    }

    pub(crate) fn read_tpixel(&self, bytes: &[u8]) -> Result<(u8, u8, u8), RfbError> {
        let width = self.tight_bytes_per_pixel()?;
        if width == 3 {
            let pixel = bytes
                .get(..3)
                .ok_or_else(|| RfbError::decoder("TPIXEL bytes"))?;
            Ok((pixel[0], pixel[1], pixel[2]))
        } else {
            let pixel = read_pixel_width(bytes, width, self.big_endian)?;
            Ok(self.to_rgb(pixel))
        }
    }

    fn cpixel_mode(&self) -> Result<CpixelMode, RfbError> {
        self.validate_for_phase(RfbPhase::Encoding)?;
        if !self.true_colour || self.bits_per_pixel != 32 || self.depth > 24 {
            return Ok(CpixelMode::Full);
        }
        let red = u32::from(self.red_max)
            .checked_shl(u32::from(self.red_shift))
            .ok_or_else(|| RfbError::decoder("CPIXEL format"))?;
        let green = u32::from(self.green_max)
            .checked_shl(u32::from(self.green_shift))
            .ok_or_else(|| RfbError::decoder("CPIXEL format"))?;
        let blue = u32::from(self.blue_max)
            .checked_shl(u32::from(self.blue_shift))
            .ok_or_else(|| RfbError::decoder("CPIXEL format"))?;
        let used = red | green | blue;
        let fits_least = used & !0x00ff_ffff == 0;
        let fits_most = used & !0xffff_ff00 == 0;
        if fits_least {
            Ok(CpixelMode::LeastThree)
        } else if fits_most {
            Ok(CpixelMode::MostThree)
        } else {
            Ok(CpixelMode::Full)
        }
    }
}

#[derive(Clone, Copy)]
enum CpixelMode {
    Full,
    LeastThree,
    MostThree,
}

fn pixel_format_error(phase: RfbPhase) -> RfbError {
    let kind = if phase == RfbPhase::ServerInit {
        RfbErrorKind::ServerInit
    } else {
        RfbErrorKind::Decoder
    };
    RfbError::new(phase, kind, "pixel format")
}

fn checked_channel_mask(maximum: u16, shift: u8, bits_per_pixel: u8) -> Option<(u64, u32)> {
    if maximum == 0 {
        return None;
    }
    let levels = u32::from(maximum).checked_add(1)?;
    if !levels.is_power_of_two() {
        return None;
    }
    let channel_bits = levels.trailing_zeros();
    let mask = u64::from(maximum).checked_shl(u32::from(shift))?;
    let pixel_mask = (1_u64.checked_shl(u32::from(bits_per_pixel))?).checked_sub(1)?;
    if mask & !pixel_mask != 0 {
        return None;
    }
    Some((mask, channel_bits))
}

fn read_pixel_width(bytes: &[u8], width: usize, big_endian: bool) -> Result<u32, RfbError> {
    let bytes = bytes
        .get(..width)
        .ok_or_else(|| RfbError::decoder("pixel bytes"))?;
    match (width, big_endian) {
        (4, true) => Ok(u32::from_be_bytes(
            bytes
                .try_into()
                .map_err(|_| RfbError::decoder("pixel bytes"))?,
        )),
        (4, false) => Ok(u32::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| RfbError::decoder("pixel bytes"))?,
        )),
        (2, true) => Ok(u32::from(u16::from_be_bytes(
            bytes
                .try_into()
                .map_err(|_| RfbError::decoder("pixel bytes"))?,
        ))),
        (2, false) => Ok(u32::from(u16::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| RfbError::decoder("pixel bytes"))?,
        ))),
        (1, _) => Ok(u32::from(bytes[0])),
        _ => Err(RfbError::decoder("pixel width")),
    }
}

pub mod encoding {
    pub const RAW: i32 = 0;
    pub const COPY_RECT: i32 = 1;
    pub const RRE: i32 = 2;
    pub const HEXTILE: i32 = 5;
    pub const TIGHT: i32 = 7;
    pub const ZRLE: i32 = 16;
    pub const CURSOR: i32 = -239;
    pub const DESKTOP_SIZE: i32 = -223;
}

pub mod client_msg {
    pub const SET_PIXEL_FORMAT: u8 = 0;
    pub const SET_ENCODINGS: u8 = 2;
    pub const FB_UPDATE_REQUEST: u8 = 3;
    pub const KEY_EVENT: u8 = 4;
    pub const POINTER_EVENT: u8 = 5;
    pub const CLIENT_CUT_TEXT: u8 = 6;
}

pub mod server_msg {
    pub const FB_UPDATE: u8 = 0;
    pub const SET_COLOUR_MAP_ENTRIES: u8 = 1;
    pub const BELL: u8 = 2;
    pub const SERVER_CUT_TEXT: u8 = 3;
}

pub struct Rectangle {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub encoding: i32,
}
