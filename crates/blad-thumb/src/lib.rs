//! Thumbnail generation.
//!
//! Produces a small JPEG that lives at the **head** of a `.blad` archive, so a file
//! browser can show a preview with one seek and no knowledge of JPEG XL, the manifest
//! format, or anything else blad does.
//!
//! # Downscaling happens in linear light
//!
//! Most software resizes in gamma-encoded space, averaging sRGB code values as though
//! they were light. They are not: sRGB is roughly a 2.2-power encoding, so averaging
//! encoded values under-weights bright pixels and visibly darkens fine detail — the
//! error is largest exactly where a thumbnail has the most to lose, in high-contrast
//! texture.
//!
//! We decode to linear, area-average, and re-encode. blad's whole pitch is that the
//! mechanical layer should be correct; getting this wrong in our own output would be a
//! poor advertisement.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("jpeg: {0}")]
    Jpeg(String),
    #[error("expected {expected} bytes for {width}x{height}, got {actual}")]
    WrongLength {
        width: u32,
        height: u32,
        expected: usize,
        actual: usize,
    },
    #[error("zero-sized image")]
    Empty,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Longest edge of a generated thumbnail, in pixels.
///
/// macOS Quick Look asks for up to 1024 on Retina displays; Finder icons need far less.
/// 512 keeps the payload to a few tens of KB — negligible beside a 56 MB archive — while
/// still looking sharp at icon and preview sizes.
pub const MAX_EDGE: u32 = 512;

/// JPEG quality. 85 is the usual point of diminishing returns; a thumbnail does not
/// need to survive re-editing.
const QUALITY: u8 = 85;

/// sRGB transfer function, encoded byte to linear.
fn srgb_to_linear(v: u8) -> f32 {
    let c = f32::from(v) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear to encoded byte.
fn linear_to_srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let v = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (v * 255.0 + 0.5) as u8
}

fn lut() -> [f32; 256] {
    let mut t = [0.0f32; 256];
    for (i, e) in t.iter_mut().enumerate() {
        *e = srgb_to_linear(i as u8);
    }
    t
}

/// Target dimensions preserving aspect ratio, with the longest edge capped at `max_edge`.
/// Never upscales — a preview smaller than the cap is already the right size.
pub fn fit(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= max_edge || longest == 0 {
        return (width.max(1), height.max(1));
    }
    let scale = f64::from(max_edge) / f64::from(longest);
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

/// Area-average downscale of interleaved RGB, in linear light.
///
/// `bytes_per_sample` may be 1 or 2; 16-bit input is reduced by taking the high byte,
/// which is ample for a thumbnail.
fn downscale(
    rgb: &[u8],
    width: u32,
    height: u32,
    bytes_per_sample: usize,
    little_endian: bool,
    out_w: u32,
    out_h: u32,
) -> Vec<u8> {
    let table = lut();
    let (w, h) = (width as usize, height as usize);
    let (ow, oh) = (out_w as usize, out_h as usize);
    let stride = w * 3 * bytes_per_sample;
    // Offset of the most significant byte within a sample.
    let hi = if bytes_per_sample == 2 && little_endian {
        1
    } else {
        0
    };

    let mut out = vec![0u8; ow * oh * 3];
    for oy in 0..oh {
        // Source rows covered by this output row, as a half-open range.
        let y0 = oy * h / oh;
        let y1 = ((oy + 1) * h).div_ceil(oh).min(h).max(y0 + 1);
        for ox in 0..ow {
            let x0 = ox * w / ow;
            let x1 = ((ox + 1) * w).div_ceil(ow).min(w).max(x0 + 1);

            let mut acc = [0.0f32; 3];
            let mut n = 0.0f32;
            for y in y0..y1 {
                let row = y * stride;
                for x in x0..x1 {
                    let px = row + x * 3 * bytes_per_sample;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += table[usize::from(rgb[px + c * bytes_per_sample + hi])];
                    }
                    n += 1.0;
                }
            }
            let o = (oy * ow + ox) * 3;
            for c in 0..3 {
                out[o + c] = linear_to_srgb(acc[c] / n);
            }
        }
    }
    out
}

/// Apply a TIFF/Exif orientation (1-8) to interleaved RGB, returning the upright image
/// and its new dimensions.
///
/// Cameras record the sensor readout as captured and note the rotation in a tag. Display
/// the pixels without consulting it and every portrait photo shows up on its side — the
/// single most visible way a thumbnailer can be wrong.
pub fn orient(rgb: &[u8], width: u32, height: u32, orientation: u16) -> (Vec<u8>, u32, u32) {
    if orientation <= 1 || orientation > 8 {
        return (rgb.to_vec(), width, height);
    }
    let (w, h) = (width as usize, height as usize);
    // Orientations 5-8 exchange the axes.
    let transposed = matches!(orientation, 5..=8);
    let (ow, oh) = if transposed { (h, w) } else { (w, h) };
    let mut out = vec![0u8; ow * oh * 3];

    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = match orientation {
                2 => (w - 1 - x, y),         // flip horizontal
                3 => (w - 1 - x, h - 1 - y), // rotate 180
                4 => (x, h - 1 - y),         // flip vertical
                5 => (y, x),                 // transpose
                6 => (h - 1 - y, x),         // rotate 90 CW
                7 => (h - 1 - y, w - 1 - x), // transverse
                8 => (y, w - 1 - x),         // rotate 270 CW
                _ => (x, y),
            };
            let src = (y * w + x) * 3;
            let dst = (dy * ow + dx) * 3;
            out[dst..dst + 3].copy_from_slice(&rgb[src..src + 3]);
        }
    }
    (out, ow as u32, oh as u32)
}

/// Downscale interleaved RGB and encode it as a JPEG.
pub fn thumbnail(
    rgb: &[u8],
    width: u32,
    height: u32,
    bytes_per_sample: usize,
    little_endian: bool,
    orientation: u16,
    max_edge: u32,
) -> Result<Vec<u8>> {
    if width == 0 || height == 0 {
        return Err(Error::Empty);
    }
    let expected = width as usize * height as usize * 3 * bytes_per_sample;
    if rgb.len() != expected {
        return Err(Error::WrongLength {
            width,
            height,
            expected,
            actual: rgb.len(),
        });
    }

    // Downscale first, rotate second: rotating a 512px image moves ~0.75 MB instead of
    // the tens of MB a full-size preview would.
    let (ow, oh) = fit(width, height, max_edge);
    let small = downscale(rgb, width, height, bytes_per_sample, little_endian, ow, oh);
    let (small, ow, oh) = orient(&small, ow, oh, orientation);

    let mut buf = Vec::new();
    jpeg_encoder::Encoder::new(&mut buf, QUALITY)
        .encode(&small, ow as u16, oh as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| Error::Jpeg(e.to_string()))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        (0..(w * h) as usize).flat_map(|_| rgb).collect()
    }

    #[test]
    fn fit_preserves_aspect_and_never_upscales() {
        assert_eq!(fit(1440, 1080, 512), (512, 384));
        assert_eq!(fit(1080, 1440, 512), (384, 512));
        assert_eq!(fit(100, 80, 512), (100, 80)); // already small
        assert_eq!(fit(1024, 1024, 512), (512, 512));
    }

    #[test]
    fn produces_a_valid_jpeg() {
        let j = thumbnail(&solid(64, 48, [200, 100, 50]), 64, 48, 1, false, 1, 32).unwrap();
        assert_eq!(&j[0..2], &[0xFF, 0xD8], "SOI marker");
        assert_eq!(&j[j.len() - 2..], &[0xFF, 0xD9], "EOI marker");
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(matches!(
            thumbnail(&[0u8; 10], 64, 48, 1, false, 1, 32),
            Err(Error::WrongLength { .. })
        ));
    }

    #[test]
    fn handles_16_bit_input() {
        // 16-bit little-endian mid-grey: high byte 0x80 in the second position.
        let px: Vec<u8> = (0..(16 * 16))
            .flat_map(|_| [0x00u8, 0x80, 0x00, 0x80, 0x00, 0x80])
            .collect();
        let j = thumbnail(&px, 16, 16, 2, true, 1, 8).unwrap();
        assert_eq!(&j[0..2], &[0xFF, 0xD8]);
    }

    /// The reason for all the linear-light machinery. Averaging a checkerboard of black
    /// and white should give the *photometric* mean — about 188 in sRGB — not 128, which
    /// is what averaging encoded values produces. A tool that gets this wrong makes every
    /// downscaled thumbnail visibly too dark.
    #[test]
    fn averages_in_linear_light_not_gamma_space() {
        let mut px = Vec::new();
        for y in 0..8u32 {
            for x in 0..8u32 {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                px.extend_from_slice(&[v, v, v]);
            }
        }
        let out = downscale(&px, 8, 8, 1, false, 1, 1);
        assert!(
            out[0] > 180 && out[0] < 195,
            "expected ~188 (linear mean), got {} — {}",
            out[0],
            if out[0] < 140 {
                "this is the gamma-space answer, i.e. the bug"
            } else {
                "unexpected"
            }
        );
    }

    /// Orientation 8 is "rotate 270 CW", which real Hasselblad files use. A landscape
    /// preview must come out portrait, with the corners where they belong.
    #[test]
    fn orientation_8_rotates_and_swaps_axes() {
        // 2x1 image: left pixel red, right pixel green.
        let px = vec![255, 0, 0, 0, 255, 0];
        let (out, w, h) = orient(&px, 2, 1, 8);
        assert_eq!((w, h), (1, 2), "axes must swap");
        assert_eq!(&out[0..3], &[0, 255, 0], "right pixel moves to the top");
        assert_eq!(&out[3..6], &[255, 0, 0], "left pixel moves to the bottom");
    }

    #[test]
    fn orientation_1_and_out_of_range_are_identity() {
        let px = vec![1, 2, 3, 4, 5, 6];
        for o in [0u16, 1, 9, 255] {
            let (out, w, h) = orient(&px, 2, 1, o);
            assert_eq!((out, w, h), (px.clone(), 2, 1), "orientation {o}");
        }
    }

    /// Every orientation must preserve the pixel count, and the four rotations must be
    /// self-inverse in pairs. A transform that loses pixels would crop the thumbnail.
    #[test]
    fn all_orientations_preserve_every_pixel() {
        let px: Vec<u8> = (0..(4 * 3 * 3) as u8).collect();
        for o in 1..=8u16 {
            let (out, w, h) = orient(&px, 4, 3, o);
            assert_eq!(
                out.len(),
                px.len(),
                "orientation {o} changed the pixel count"
            );
            assert_eq!((w * h) as usize * 3, out.len());
            let mut a = px.clone();
            let mut b = out.clone();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b, "orientation {o} lost or invented pixel data");
        }
    }

    #[test]
    fn solid_colour_survives_downscaling() {
        // A uniform field must come back uniform: no drift from the round trip through
        // linear and back.
        let out = downscale(&solid(32, 32, [70, 140, 210]), 32, 32, 1, false, 4, 4);
        for px in out.chunks_exact(3) {
            assert!((i16::from(px[0]) - 70).abs() <= 1, "{px:?}");
            assert!((i16::from(px[1]) - 140).abs() <= 1, "{px:?}");
            assert!((i16::from(px[2]) - 210).abs() <= 1, "{px:?}");
        }
    }
}
