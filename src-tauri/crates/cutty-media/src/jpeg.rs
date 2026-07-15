//! JPEG encoding of decoded frames for the preview IPC stream (turbojpeg,
//! per PLAN.md §3 — this is codec work, not a CPU pixel loop).

use crate::error::MediaError;

/// Preview stream quality. 80 keeps 720p frames around 60–120 KB — well
/// inside the IPC budget — with no visible banding on screen-sized preview.
const JPEG_QUALITY: i32 = 80;

/// Reusable JPEG compressor (holds a turbojpeg handle).
pub struct JpegEncoder {
    compressor: turbojpeg::Compressor,
}

impl JpegEncoder {
    pub fn new() -> Result<Self, MediaError> {
        let mut compressor = turbojpeg::Compressor::new().map_err(jpeg_err)?;
        compressor.set_quality(JPEG_QUALITY).map_err(jpeg_err)?;
        compressor
            .set_subsamp(turbojpeg::Subsamp::Sub2x2)
            .map_err(jpeg_err)?;
        Ok(Self { compressor })
    }

    /// Compress a tightly-packed rgb24 buffer.
    pub fn encode_rgb(
        &mut self,
        width: u32,
        height: u32,
        rgb: &[u8],
    ) -> Result<Vec<u8>, MediaError> {
        self.encode_rgb_strided(width, height, width as usize * 3, rgb)
    }

    /// Compress an rgb24 buffer whose rows are `pitch` bytes apart
    /// (libav-decoded frames carry row alignment padding).
    pub fn encode_rgb_strided(
        &mut self,
        width: u32,
        height: u32,
        pitch: usize,
        rgb: &[u8],
    ) -> Result<Vec<u8>, MediaError> {
        let image = turbojpeg::Image {
            pixels: rgb,
            width: width as usize,
            pitch,
            height: height as usize,
            format: turbojpeg::PixelFormat::RGB,
        };
        self.compressor.compress_to_vec(image).map_err(jpeg_err)
    }

    /// Compress an rgba buffer whose rows are `pitch` bytes apart (both
    /// decoded frames and GPU readbacks are RGBA with row padding; alpha
    /// is ignored by the encoder).
    pub fn encode_rgba_strided(
        &mut self,
        width: u32,
        height: u32,
        pitch: usize,
        rgba: &[u8],
    ) -> Result<Vec<u8>, MediaError> {
        let image = turbojpeg::Image {
            pixels: rgba,
            width: width as usize,
            pitch,
            height: height as usize,
            format: turbojpeg::PixelFormat::RGBA,
        };
        self.compressor.compress_to_vec(image).map_err(jpeg_err)
    }
}

fn jpeg_err(e: turbojpeg::Error) -> MediaError {
    MediaError::Jpeg(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_valid_jpeg() {
        let (w, h) = (64u32, 32u32);
        // Simple gradient so the encoder has real content.
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 3) as usize;
                rgb[i] = (x * 4) as u8;
                rgb[i + 1] = (y * 8) as u8;
                rgb[i + 2] = 128;
            }
        }
        let mut enc = JpegEncoder::new().unwrap();
        let jpeg = enc.encode_rgb(w, h, &rgb).unwrap();
        // SOI + EOI markers.
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9]);
        assert!(jpeg.len() < rgb.len(), "must actually compress");
    }
}
