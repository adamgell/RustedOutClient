use super::{RfbError, RfbErrorKind, RfbPhase};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolLimits {
    pub max_dimension: u16,
    pub max_pixels: u64,
    pub max_framebuffer_bytes: u64,
    pub max_text_bytes: u32,
    pub max_rectangles: u16,
    pub max_encoded_rect_bytes: u32,
    pub max_clipboard_bytes: u32,
}

impl Default for ProtocolLimits {
    fn default() -> Self {
        Self {
            max_dimension: 8_192,
            max_pixels: 33_554_432,
            max_framebuffer_bytes: 134_217_728,
            max_text_bytes: 65_536,
            max_rectangles: 4_096,
            max_encoded_rect_bytes: 67_108_864,
            max_clipboard_bytes: 1_048_576,
        }
    }
}

impl ProtocolLimits {
    pub(crate) fn validate_for_phase(self, phase: RfbPhase) -> Result<(), RfbError> {
        let ceiling = Self::default();
        if self.max_dimension > ceiling.max_dimension
            || self.max_pixels > ceiling.max_pixels
            || self.max_framebuffer_bytes > ceiling.max_framebuffer_bytes
            || self.max_text_bytes > ceiling.max_text_bytes
            || self.max_rectangles > ceiling.max_rectangles
            || self.max_encoded_rect_bytes > ceiling.max_encoded_rect_bytes
            || self.max_clipboard_bytes > ceiling.max_clipboard_bytes
        {
            return Err(RfbError::limit(phase, "protocol limit configuration"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FramebufferLayout {
    pub width: u16,
    pub height: u16,
    pub pixels: u64,
    pub rgba_bytes: usize,
}

pub fn validate_framebuffer_layout(
    width: u16,
    height: u16,
    limits: ProtocolLimits,
) -> Result<FramebufferLayout, RfbError> {
    validate_framebuffer_layout_for_phase(width, height, limits, RfbPhase::ServerInit)
}

pub(crate) fn validate_framebuffer_layout_for_phase(
    width: u16,
    height: u16,
    limits: ProtocolLimits,
    phase: RfbPhase,
) -> Result<FramebufferLayout, RfbError> {
    limits.validate_for_phase(phase)?;
    if width == 0 || height == 0 {
        return Err(RfbError::new(
            phase,
            RfbErrorKind::ServerInit,
            "framebuffer dimensions",
        ));
    }
    if width > limits.max_dimension || height > limits.max_dimension {
        return Err(RfbError::limit(phase, "framebuffer dimensions"));
    }

    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| RfbError::limit(phase, "framebuffer pixels"))?;
    if pixels > limits.max_pixels {
        return Err(RfbError::limit(phase, "framebuffer pixels"));
    }

    let rgba_bytes = pixels
        .checked_mul(4)
        .ok_or_else(|| RfbError::limit(phase, "framebuffer bytes"))?;
    if rgba_bytes > limits.max_framebuffer_bytes {
        return Err(RfbError::limit(phase, "framebuffer bytes"));
    }
    let rgba_bytes =
        usize::try_from(rgba_bytes).map_err(|_| RfbError::limit(phase, "framebuffer bytes"))?;

    Ok(FramebufferLayout {
        width,
        height,
        pixels,
        rgba_bytes,
    })
}
