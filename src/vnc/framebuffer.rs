use super::{
    limits::validate_framebuffer_layout_for_phase, wire::allocate_zeroed, ProtocolLimits, RfbError,
    RfbErrorKind, RfbPhase,
};

const RGBA_BYTES_PER_PIXEL: u64 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckedRect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    framebuffer_width: u16,
    framebuffer_height: u16,
}

impl CheckedRect {
    pub fn new(
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        framebuffer_width: u16,
        framebuffer_height: u16,
    ) -> Result<Self, RfbError> {
        if width == 0 || height == 0 {
            return Err(RfbError::new(
                RfbPhase::Framebuffer,
                RfbErrorKind::Protocol,
                "rectangle dimensions",
            ));
        }
        let right = x
            .checked_add(width)
            .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "rectangle x extent"))?;
        let bottom = y
            .checked_add(height)
            .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "rectangle y extent"))?;
        if right > framebuffer_width || bottom > framebuffer_height {
            return Err(RfbError::new(
                RfbPhase::Framebuffer,
                RfbErrorKind::Protocol,
                "rectangle bounds",
            ));
        }
        Ok(Self {
            x,
            y,
            width,
            height,
            framebuffer_width,
            framebuffer_height,
        })
    }

    pub fn x(self) -> u16 {
        self.x
    }

    pub fn y(self) -> u16 {
        self.y
    }

    pub fn width(self) -> u16 {
        self.width
    }

    pub fn height(self) -> u16 {
        self.height
    }

    pub(crate) fn expected_rgba_bytes(self, phase: RfbPhase) -> Result<usize, RfbError> {
        let bytes = u64::from(self.width)
            .checked_mul(u64::from(self.height))
            .and_then(|pixels| pixels.checked_mul(RGBA_BYTES_PER_PIXEL))
            .ok_or_else(|| RfbError::limit(phase, "rectangle RGBA bytes"))?;
        usize::try_from(bytes).map_err(|_| RfbError::limit(phase, "rectangle RGBA bytes"))
    }

    fn validate_framebuffer(self, framebuffer: &Framebuffer) -> Result<(), RfbError> {
        if self.framebuffer_width != framebuffer.width
            || self.framebuffer_height != framebuffer.height
        {
            return Err(RfbError::new(
                RfbPhase::Framebuffer,
                RfbErrorKind::Protocol,
                "rectangle framebuffer",
            ));
        }
        Ok(())
    }
}

pub struct Framebuffer {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
    limits: ProtocolLimits,
}

impl Framebuffer {
    pub fn new(width: u16, height: u16, limits: ProtocolLimits) -> Result<Self, RfbError> {
        let layout =
            validate_framebuffer_layout_for_phase(width, height, limits, RfbPhase::Framebuffer)?;
        let pixels = allocate_zeroed(
            layout.rgba_bytes,
            RfbPhase::Framebuffer,
            "framebuffer storage",
        )?;
        Ok(Self {
            width,
            height,
            pixels,
            limits,
        })
    }

    pub(crate) fn from_pixels(
        width: u16,
        height: u16,
        limits: ProtocolLimits,
        pixels: Vec<u8>,
    ) -> Result<Self, RfbError> {
        let layout =
            validate_framebuffer_layout_for_phase(width, height, limits, RfbPhase::Framebuffer)?;
        if pixels.len() != layout.rgba_bytes {
            return Err(RfbError::new(
                RfbPhase::Framebuffer,
                RfbErrorKind::Protocol,
                "framebuffer storage length",
            ));
        }
        Ok(Self {
            width,
            height,
            pixels,
            limits,
        })
    }

    pub fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn limits(&self) -> ProtocolLimits {
        self.limits
    }

    pub fn resize(&mut self, width: u16, height: u16) -> Result<(), RfbError> {
        self.resize_with(width, height, |length| {
            allocate_zeroed(length, RfbPhase::Framebuffer, "resized framebuffer")
        })
    }

    fn resize_with<F>(&mut self, width: u16, height: u16, allocate: F) -> Result<(), RfbError>
    where
        F: FnOnce(usize) -> Result<Vec<u8>, RfbError>,
    {
        if self.width == width && self.height == height {
            return Ok(());
        }
        let layout = validate_framebuffer_layout_for_phase(
            width,
            height,
            self.limits,
            RfbPhase::Framebuffer,
        )?;
        let replacement = allocate(layout.rgba_bytes)?;
        if replacement.len() != layout.rgba_bytes {
            return Err(RfbError::allocation(
                RfbPhase::Framebuffer,
                "resized framebuffer",
            ));
        }
        self.width = width;
        self.height = height;
        self.pixels = replacement;
        Ok(())
    }

    pub fn write_rgba(&mut self, rectangle: CheckedRect, data: &[u8]) -> Result<(), RfbError> {
        rectangle.validate_framebuffer(self)?;
        let expected = rectangle.expected_rgba_bytes(RfbPhase::Framebuffer)?;
        if data.len() != expected {
            return Err(RfbError::new(
                RfbPhase::Framebuffer,
                RfbErrorKind::Protocol,
                "rectangle RGBA length",
            ));
        }

        let row_bytes = usize::from(rectangle.width)
            .checked_mul(4)
            .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "rectangle row bytes"))?;
        for row in 0..rectangle.height {
            let source_start = usize::from(row)
                .checked_mul(row_bytes)
                .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "rectangle source"))?;
            let source_end = source_start
                .checked_add(row_bytes)
                .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "rectangle source"))?;
            let destination_y = rectangle
                .y
                .checked_add(row)
                .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "rectangle target"))?;
            let destination_start = self.pixel_offset(rectangle.x, destination_y)?;
            let destination_end = destination_start
                .checked_add(row_bytes)
                .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "rectangle target"))?;
            let source = data.get(source_start..source_end).ok_or_else(|| {
                RfbError::new(
                    RfbPhase::Framebuffer,
                    RfbErrorKind::Protocol,
                    "rectangle source",
                )
            })?;
            let destination = self
                .pixels
                .get_mut(destination_start..destination_end)
                .ok_or_else(|| {
                    RfbError::new(
                        RfbPhase::Framebuffer,
                        RfbErrorKind::Protocol,
                        "rectangle target",
                    )
                })?;
            destination.copy_from_slice(source);
        }
        Ok(())
    }

    pub fn snapshot(&self, rectangle: CheckedRect) -> Result<Vec<u8>, RfbError> {
        rectangle.validate_framebuffer(self)?;
        let length = rectangle.expected_rgba_bytes(RfbPhase::Framebuffer)?;
        let mut snapshot = allocate_zeroed(length, RfbPhase::Framebuffer, "framebuffer snapshot")?;
        let row_bytes = usize::from(rectangle.width)
            .checked_mul(4)
            .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "snapshot row bytes"))?;
        for row in 0..rectangle.height {
            let source_y = rectangle
                .y
                .checked_add(row)
                .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "snapshot source"))?;
            let source_start = self.pixel_offset(rectangle.x, source_y)?;
            let source_end = source_start
                .checked_add(row_bytes)
                .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "snapshot source"))?;
            let destination_start = usize::from(row)
                .checked_mul(row_bytes)
                .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "snapshot target"))?;
            let destination_end = destination_start
                .checked_add(row_bytes)
                .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "snapshot target"))?;
            let source = self.pixels.get(source_start..source_end).ok_or_else(|| {
                RfbError::new(
                    RfbPhase::Framebuffer,
                    RfbErrorKind::Protocol,
                    "snapshot source",
                )
            })?;
            let destination = snapshot
                .get_mut(destination_start..destination_end)
                .ok_or_else(|| {
                    RfbError::new(
                        RfbPhase::Framebuffer,
                        RfbErrorKind::Protocol,
                        "snapshot target",
                    )
                })?;
            destination.copy_from_slice(source);
        }
        Ok(snapshot)
    }

    pub fn copy_rect(
        &mut self,
        destination: CheckedRect,
        source_x: u16,
        source_y: u16,
    ) -> Result<(), RfbError> {
        destination.validate_framebuffer(self)?;
        let source = CheckedRect::new(
            source_x,
            source_y,
            destination.width,
            destination.height,
            self.width,
            self.height,
        )?;
        let snapshot = self.snapshot(source)?;
        self.write_rgba(destination, &snapshot)
    }

    fn pixel_offset(&self, x: u16, y: u16) -> Result<usize, RfbError> {
        let pixel = u64::from(y)
            .checked_mul(u64::from(self.width))
            .and_then(|offset| offset.checked_add(u64::from(x)))
            .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "framebuffer offset"))?;
        let bytes = pixel
            .checked_mul(RGBA_BYTES_PER_PIXEL)
            .ok_or_else(|| RfbError::limit(RfbPhase::Framebuffer, "framebuffer offset"))?;
        usize::try_from(bytes)
            .map_err(|_| RfbError::limit(RfbPhase::Framebuffer, "framebuffer offset"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_failure_during_resize_preserves_dimensions_and_storage() {
        let mut framebuffer = Framebuffer::new(2, 2, ProtocolLimits::default()).unwrap();
        let before = framebuffer.pixels.clone();
        let error = framebuffer
            .resize_with(3, 3, |_| {
                Err(RfbError::allocation(
                    RfbPhase::Framebuffer,
                    "synthetic allocation",
                ))
            })
            .unwrap_err();
        assert_eq!(error.kind(), RfbErrorKind::Allocation);
        assert_eq!(framebuffer.dimensions(), (2, 2));
        assert_eq!(framebuffer.pixels, before);
    }
}
