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

/// How the samples encode light.
///
/// A thumbnailer that assumes sRGB will render a PQ master with lifted blacks and no
/// contrast — the encoding puts SDR-range content in roughly the lower half of the code
/// range, so reading those values as sRGB brightens everything and flattens it. That is
/// not a subtle error; it looks washed out at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transfer {
    #[default]
    Srgb,
    /// SMPTE ST 2084, absolute luminance up to 10,000 nits.
    Pq,
    /// ARIB STD-B67 hybrid log-gamma, relative.
    Hlg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Primaries {
    #[default]
    Srgb,
    Bt2020,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Color {
    pub transfer: Transfer,
    pub primaries: Primaries,
}

impl Color {
    pub fn is_hdr(&self) -> bool {
        matches!(self.transfer, Transfer::Pq | Transfer::Hlg)
    }
}

/// BT.2408 diffuse-white reference. PQ is an *absolute* encoding, so something has to
/// say which luminance counts as "white paper"; 203 nits is the broadcast convention.
const REFERENCE_WHITE_NITS: f32 = 203.0;

/// SMPTE ST 2084 inverse EOTF, normalised so diffuse white lands at 1.0.
fn pq_to_linear(e: f32) -> f32 {
    const M1: f32 = 0.159_301_76;
    const M2: f32 = 78.84375;
    const C1: f32 = 0.8359375;
    const C2: f32 = 18.851_562;
    const C3: f32 = 18.6875;
    let p = e.clamp(0.0, 1.0).powf(1.0 / M2);
    let num = (p - C1).max(0.0);
    let den = C2 - C3 * p;
    if den <= 0.0 {
        return 0.0;
    }
    (num / den).powf(1.0 / M1) * 10_000.0 / REFERENCE_WHITE_NITS
}

/// ARIB STD-B67 inverse OETF, normalised so the 75% signal — HLG's diffuse white —
/// lands at 1.0.
///
/// The display OOTF is not applied: it depends on a peak luminance the file does not
/// state, and for a thumbnail the difference is smaller than the tone mapping that
/// follows.
fn hlg_to_linear(e: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92;
    const C: f32 = 0.559_910_7;
    let e = e.clamp(0.0, 1.0);
    let s = if e <= 0.5 {
        e * e / 3.0
    } else {
        (((e - C) / A).exp() + B) / 12.0
    };
    // s(0.75), the scene-linear value of diffuse white.
    const WHITE: f32 = 0.264_900_6;
    s / WHITE
}

/// BT.2020 to sRGB/BT.709 primaries, both D65, in linear light.
const BT2020_TO_SRGB: [[f32; 3]; 3] = [
    [1.660_491, -0.587_641, -0.072_850],
    [-0.124_55, 1.132_9, -0.008_349],
    [-0.018_151, -0.100_579, 1.118_73],
];

/// Roll HDR highlights into the display range, leaving everything below the knee alone.
///
/// The obvious choice, extended Reinhard, was tried and rejected by measurement: with a
/// white point of 8 it maps 0.5 to 0.34, darkening every mid-tone in the picture. A
/// photograph mastered in PQ has its diffuse white at 1.0 and most of its content below
/// that, so the tones that matter must pass through untouched and only the specular
/// highlights should compress.
///
/// Below `KNEE` the transfer is the identity; above it an exponential soft-clip
/// approaches 1.0 asymptotically, so nothing ever clips hard.
///
/// Luminance drives the curve and the channels scale together, which preserves hue —
/// a per-channel curve would desaturate highlights toward white.
fn tone_map(rgb: &mut [f32]) {
    const KNEE: f32 = 0.8;
    for px in rgb.chunks_exact_mut(3) {
        // BT.2020 luminance weights; pixels are still in their source primaries.
        let l = 0.2627 * px[0] + 0.6780 * px[1] + 0.0593 * px[2];
        if l <= KNEE || l <= 0.0 {
            continue;
        }
        let mapped = KNEE + (1.0 - KNEE) * (1.0 - (-(l - KNEE) / (1.0 - KNEE)).exp());
        let gain = mapped / l;
        for c in px.iter_mut() {
            *c *= gain;
        }
    }
}

fn to_srgb_primaries(rgb: &mut [f32]) {
    for px in rgb.chunks_exact_mut(3) {
        let (r, g, b) = (px[0], px[1], px[2]);
        for (i, row) in BT2020_TO_SRGB.iter().enumerate() {
            px[i] = row[0] * r + row[1] * g + row[2] * b;
        }
    }
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

/// Area-average downscale of interleaved RGB into **linear** f32.
///
/// `bytes_per_sample` may be 1 or 2. For sRGB input the high byte is ample, but an HDR
/// encoding puts a lot of its precision low in the range, so 16-bit PQ is decoded at
/// full depth — reducing it to 8 bits first visibly bands the shadows.
#[allow(clippy::too_many_arguments)]
fn downscale_linear(
    rgb: &[u8],
    width: u32,
    height: u32,
    bytes_per_sample: usize,
    little_endian: bool,
    color: Color,
    out_w: u32,
    out_h: u32,
) -> Vec<f32> {
    let (w, h) = (width as usize, height as usize);
    let (ow, oh) = (out_w as usize, out_h as usize);
    let stride = w * 3 * bytes_per_sample;
    let hi = if bytes_per_sample == 2 && little_endian {
        1
    } else {
        0
    };
    let lo = 1 - hi;

    let byte_table = lut();
    // 16-bit HDR is decoded through a full table rather than the high byte alone.
    let deep = bytes_per_sample == 2 && color.is_hdr();
    let wide_table: Vec<f32> = if deep {
        (0..=u16::MAX)
            .map(|v| {
                let e = f32::from(v) / 65535.0;
                match color.transfer {
                    Transfer::Pq => pq_to_linear(e),
                    Transfer::Hlg => hlg_to_linear(e),
                    Transfer::Srgb => e,
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let narrow_table: Vec<f32> = if !deep && color.is_hdr() {
        (0..256)
            .map(|v| {
                let e = v as f32 / 255.0;
                match color.transfer {
                    Transfer::Pq => pq_to_linear(e),
                    Transfer::Hlg => hlg_to_linear(e),
                    Transfer::Srgb => e,
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    let sample = |px: usize, c: usize| -> f32 {
        let at = px + c * bytes_per_sample;
        if deep {
            let v = if little_endian {
                u16::from(rgb[at + 1]) << 8 | u16::from(rgb[at])
            } else {
                u16::from(rgb[at]) << 8 | u16::from(rgb[at + 1])
            };
            wide_table[usize::from(v)]
        } else if color.is_hdr() {
            narrow_table[usize::from(rgb[at + hi])]
        } else {
            byte_table[usize::from(rgb[at + hi])]
        }
    };
    let _ = lo;

    let mut out = vec![0.0f32; ow * oh * 3];
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
                        *a += sample(px, c);
                    }
                    n += 1.0;
                }
            }
            let o = (oy * ow + ox) * 3;
            for c in 0..3 {
                out[o + c] = acc[c] / n;
            }
        }
    }
    out
}

/// Linear f32 to encoded sRGB bytes, tone mapping and gamut converting on the way if
/// the source was HDR.
fn finish(mut linear: Vec<f32>, color: Color) -> Vec<u8> {
    if color.is_hdr() {
        tone_map(&mut linear);
    }
    if color.primaries == Primaries::Bt2020 {
        to_srgb_primaries(&mut linear);
    }
    linear.iter().map(|&c| linear_to_srgb(c)).collect()
}

/// Apply a TIFF/Exif orientation (1-8) to interleaved RGB/// Apply a TIFF/Exif orientation (1-8) to interleaved RGB, returning the upright image
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
#[allow(clippy::too_many_arguments)]
pub fn thumbnail(
    rgb: &[u8],
    width: u32,
    height: u32,
    bytes_per_sample: usize,
    little_endian: bool,
    orientation: u16,
    max_edge: u32,
    color: Color,
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
    let linear = downscale_linear(
        rgb,
        width,
        height,
        bytes_per_sample,
        little_endian,
        color,
        ow,
        oh,
    );
    let small = finish(linear, color);
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

    /// PQ is an absolute encoding: full scale is 10,000 nits, and 203 nits is diffuse
    /// white by convention.
    #[test]
    fn pq_decodes_to_absolute_luminance() {
        assert!((pq_to_linear(1.0) - 10_000.0 / REFERENCE_WHITE_NITS).abs() < 0.5);
        assert_eq!(pq_to_linear(0.0), 0.0);
        let white = pq_to_linear(0.58);
        assert!(
            (0.8..1.4).contains(&white),
            "diffuse white decoded to {white}"
        );
        assert!(pq_to_linear(0.25) < pq_to_linear(0.5));
    }

    /// The washout, precisely. PQ puts its shadows far lower than sRGB does — code 0.05
    /// is thirteen times darker — so reading those values as sRGB lifts the blacks and
    /// flattens the picture. Highlights run the other way, which is the rest of the lost
    /// contrast.
    #[test]
    fn pq_shadows_are_far_darker_than_srgb_and_highlights_far_brighter() {
        assert!(
            pq_to_linear(0.05) < srgb_to_linear(13) / 8.0,
            "PQ shadow {} was not far below sRGB {}",
            pq_to_linear(0.05),
            srgb_to_linear(13)
        );
        assert!(
            pq_to_linear(0.75) > 4.0,
            "PQ highlight {}",
            pq_to_linear(0.75)
        );
    }

    #[test]
    fn hlg_diffuse_white_lands_at_one() {
        let w = hlg_to_linear(0.75);
        assert!((w - 1.0).abs() < 0.02, "HLG white decoded to {w}");
        assert_eq!(hlg_to_linear(0.0), 0.0);
    }

    /// Mid-tones pass through untouched; only highlights compress, and never clip hard.
    #[test]
    fn tone_mapping_leaves_midtones_alone_and_rolls_off_highlights() {
        let mut px = vec![0.1, 0.1, 0.1, 0.5, 0.5, 0.5, 49.0, 49.0, 49.0];
        tone_map(&mut px);
        assert_eq!(px[0], 0.1, "shadow moved");
        assert_eq!(px[3], 0.5, "midtone moved");
        assert!(px[6] <= 1.0 && px[6] > 0.99, "peak landed on {}", px[6]);
    }

    /// Hue must survive: channels scale together rather than each taking its own curve.
    #[test]
    fn tone_mapping_keeps_hue() {
        let mut px = vec![4.0, 2.0, 1.0];
        tone_map(&mut px);
        let ratio = px[0] / px[1];
        assert!((ratio - 2.0).abs() < 0.001, "hue shifted, r/g = {ratio}");
    }

    #[test]
    fn bt2020_converts_toward_srgb_primaries() {
        let mut px = vec![0.0, 1.0, 0.0];
        to_srgb_primaries(&mut px);
        assert!(px[1] > 1.0, "pure BT.2020 green should exceed sRGB green");
        assert!(px[0] < 0.0 || px[2] < 0.0, "expected out-of-gamut channels");
        let mut w = vec![1.0, 1.0, 1.0];
        to_srgb_primaries(&mut w);
        for c in w {
            assert!((c - 1.0).abs() < 0.005, "white shifted to {c}");
        }
    }

    /// End to end: the same samples read as PQ come out darker than read as sRGB. This
    /// is the regression that made HDR previews look washed out.
    #[test]
    fn hdr_thumbnails_are_not_washed_out() {
        let mut px = Vec::new();
        for _ in 0..16 * 16 * 3 {
            px.extend(0x4000u16.to_le_bytes());
        }
        let sdr = downscale_linear(&px, 16, 16, 2, true, Color::default(), 1, 1);
        let hdr = downscale_linear(
            &px,
            16,
            16,
            2,
            true,
            Color {
                transfer: Transfer::Pq,
                primaries: Primaries::Bt2020,
            },
            1,
            1,
        );
        assert!(
            hdr[0] < sdr[0],
            "PQ {} not darker than sRGB {}",
            hdr[0],
            sdr[0]
        );
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
        let j = thumbnail(
            &solid(64, 48, [200, 100, 50]),
            64,
            48,
            1,
            false,
            1,
            32,
            Color::default(),
        )
        .unwrap();
        assert_eq!(&j[0..2], &[0xFF, 0xD8], "SOI marker");
        assert_eq!(&j[j.len() - 2..], &[0xFF, 0xD9], "EOI marker");
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(matches!(
            thumbnail(&[0u8; 10], 64, 48, 1, false, 1, 32, Color::default()),
            Err(Error::WrongLength { .. })
        ));
    }

    #[test]
    fn handles_16_bit_input() {
        // 16-bit little-endian mid-grey: high byte 0x80 in the second position.
        let px: Vec<u8> = (0..(16 * 16))
            .flat_map(|_| [0x00u8, 0x80, 0x00, 0x80, 0x00, 0x80])
            .collect();
        let j = thumbnail(&px, 16, 16, 2, true, 1, 8, Color::default()).unwrap();
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
        let out = finish(
            downscale_linear(&px, 8, 8, 1, false, Color::default(), 1, 1),
            Color::default(),
        );
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
        let out = finish(
            downscale_linear(
                &solid(32, 32, [70, 140, 210]),
                32,
                32,
                1,
                false,
                Color::default(),
                4,
                4,
            ),
            Color::default(),
        );
        for px in out.chunks_exact(3) {
            assert!((i16::from(px[0]) - 70).abs() <= 1, "{px:?}");
            assert!((i16::from(px[1]) - 140).abs() <= 1, "{px:?}");
            assert!((i16::from(px[2]) - 210).abs() <= 1, "{px:?}");
        }
    }
}
