//! Lossless codec backends.
//!
//! # Byte-oriented by design
//!
//! Encode and decode take and produce raw sample **bytes** in the source file's own byte
//! order, and write through a [`Write`]. Materialising an image as `Vec<u16>` merely to
//! change endianness duplicates the entire frame — ~300 MB on a 51MP file, twice per
//! round trip.
//!
//! # Why JPEG XL
//!
//! Measured on real 16-bit files, not chosen by reputation:
//!
//! - `zstd -19` reaches only 0.72 on a Bayer mosaic; LZ looks for repeated byte
//!   sequences and photographic data has 2D correlation instead.
//! - AVIF and HEIC cannot participate at all — AV1 tops out at 12 bits per sample.
//! - PNG handles 16-bit but is well behind JXL's modular mode.
//!
//! libjxl is vendored, so the binary links `libjxl.a` statically and depends on nothing
//! beyond the platform C/C++ runtime. That also pins the codec version, which keeps
//! benchmarks comparable over time instead of shifting under someone's package upgrade.
//!
//! # Effort is non-monotonic
//!
//! Default is 4, not 9. Measured: on Bayer planes effort 7 encoded *larger* than 4 and
//! 3.8x slower; on a 51MP RGB frame effort 9 was larger than 7 and 36x slower than 4.
//! Do not raise the default without measuring on both CFA and RGB content.

use std::io::Write;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("libjxl: {0}")]
    Backend(String),
    #[error("byte count mismatch: got {actual}, expected {expected}")]
    SizeMismatch { expected: u64, actual: u64 },
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    Gray,
    Rgb,
}

impl Channels {
    pub fn count(self) -> u64 {
        match self {
            Channels::Gray => 1,
            Channels::Rgb => 3,
        }
    }
}

/// Bits per sample. Real files mix depths — a 3FR holds a 16-bit sensor mosaic *and* an
/// 8-bit embedded preview — so this cannot be assumed away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Eight,
    Sixteen,
}

impl Depth {
    pub fn from_bits(bits: u16) -> Option<Self> {
        match bits {
            8 => Some(Depth::Eight),
            16 => Some(Depth::Sixteen),
            _ => None,
        }
    }
    pub fn bytes(self) -> u64 {
        match self {
            Depth::Eight => 1,
            Depth::Sixteen => 2,
        }
    }
}

/// Everything a codec needs to know about a rectangle of raw samples.
///
/// Encode and decode take the *same* descriptor, and that symmetry is the point: a
/// round trip whose two halves disagree about any one field does not fail loudly, it
/// silently produces wrong pixels. One value passed to both sides cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub channels: Channels,
    pub depth: Depth,
    /// Byte order of the samples *as stored in the file*.
    pub little_endian: bool,
}

impl Frame {
    /// Raw byte length of the samples this frame describes.
    pub fn byte_len(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * self.channels.count() * self.depth.bytes()
    }
}

/// A lossless image codec operating on raw sample bytes.
///
/// Implementations must be exactly reversible. This is asserted end-to-end on every
/// archive rather than trusted.
pub trait Codec: Sync {
    /// Short stable identifier recorded in the archive manifest, so a future version
    /// knows which backend produced a payload.
    fn id(&self) -> &'static str;

    /// Human description of the exact encoder and settings, for the archive's record.
    ///
    /// Reported by the linked library rather than assumed from the crate version: what
    /// matters to someone holding the archive in ten years is which encoder actually
    /// produced those bytes, and a hardcoded string can drift from reality without
    /// anyone noticing.
    fn describe(&self) -> String {
        self.id().to_string()
    }

    /// Encode `src` (raw samples in the file's byte order), writing the compressed
    /// representation to `out`. Returns bytes written.
    fn encode(&self, src: &[u8], frame: Frame, out: &mut dyn Write) -> Result<u64>;

    /// Decode `data`, writing raw samples in the file's byte order to `out`.
    /// Returns bytes written.
    fn decode(&self, data: &[u8], frame: Frame, out: &mut dyn Write) -> Result<u64>;
}

/// JPEG XL via vendored libjxl, in process.
#[derive(Debug, Clone)]
pub struct Jxl {
    /// 1-10, mapped to libjxl's speed tiers. See the module note on non-monotonicity
    /// before changing this.
    pub effort: u8,
}

impl Default for Jxl {
    fn default() -> Self {
        Self { effort: 4 }
    }
}

impl Jxl {
    fn speed(&self) -> jpegxl_rs::encode::EncoderSpeed {
        use jpegxl_rs::encode::EncoderSpeed::*;
        match self.effort.clamp(1, 10) {
            1 => Lightning,
            2 => Thunder,
            3 => Falcon,
            4 => Cheetah,
            5 => Hare,
            6 => Wombat,
            7 => Squirrel,
            8 => Kitten,
            9 => Tortoise,
            _ => Glacier,
        }
    }
}

/// Reinterpret raw bytes as 16-bit samples without copying.
///
/// Large allocations come back well-aligned from every allocator we care about, so the
/// zero-copy path is the normal one; the copy is a correctness fallback, not the
/// expected case.
fn as_u16(src: &[u8]) -> std::borrow::Cow<'_, [u16]> {
    match bytemuck::try_cast_slice::<u8, u16>(src) {
        Ok(s) => std::borrow::Cow::Borrowed(s),
        Err(_) => std::borrow::Cow::Owned(
            src.chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect(),
        ),
    }
}

fn endianness(little_endian: bool) -> jpegxl_rs::Endianness {
    if little_endian {
        jpegxl_rs::Endianness::Little
    } else {
        jpegxl_rs::Endianness::Big
    }
}

/// libjxl's own version, e.g. `0.12.0`, from the linked library.
pub fn libjxl_version() -> String {
    // Encoded as major*1_000_000 + minor*1_000 + patch.
    let v = unsafe { jpegxl_sys::encoder::encode::JxlEncoderVersion() };
    format!("{}.{}.{}", v / 1_000_000, (v / 1_000) % 1_000, v % 1_000)
}

impl Codec for Jxl {
    fn id(&self) -> &'static str {
        "jxl"
    }

    fn describe(&self) -> String {
        format!(
            "libjxl {} effort {}",
            libjxl_version(),
            self.effort.clamp(1, 10)
        )
    }

    fn encode(&self, src: &[u8], frame: Frame, out: &mut dyn Write) -> Result<u64> {
        let Frame {
            width,
            height,
            channels: ch,
            depth,
            little_endian,
        } = frame;
        let expected = frame.byte_len();
        if src.len() as u64 != expected {
            return Err(Error::SizeMismatch {
                expected,
                actual: src.len() as u64,
            });
        }

        // The colour encoding must match the channel count or libjxl rejects the
        // configuration outright. It is metadata only: lossless recovers every sample
        // exactly regardless, and what these samples *mean* (sensor response, Adobe RGB,
        // whatever) is defined by the container we preserve verbatim — never by this tag.
        let color = match ch {
            Channels::Gray => jpegxl_rs::encode::ColorEncoding::SrgbLuma,
            Channels::Rgb => jpegxl_rs::encode::ColorEncoding::Srgb,
        };
        // Without an explicit runner libjxl encodes single-threaded. Measured cost of
        // omitting it: 51-79% slower — enough to lose to spawning a subprocess.
        let runner = jpegxl_rs::ThreadsRunner::default();
        let mut enc = jpegxl_rs::encoder_builder()
            .parallel_runner(&runner)
            .lossless(true)
            // libjxl rejects lossless unless the original profile is kept: lossless
            // means the encoder must not transform the samples, and substituting a
            // profile is exactly what would license it to.
            .uses_original_profile(true)
            .speed(self.speed())
            .color_encoding(color)
            .has_alpha(false)
            .build()
            .map_err(|e| Error::Backend(e.to_string()))?;

        let nch = ch.count() as u32;
        let encoded: Vec<u8> = match depth {
            Depth::Eight => {
                let ef = jpegxl_rs::encode::EncoderFrame::new(src)
                    .num_channels(nch)
                    .endianness(endianness(little_endian));
                enc.encode_frame::<u8, u8>(&ef, width, height)
                    .map_err(|e| Error::Backend(e.to_string()))?
                    .data
            }
            Depth::Sixteen => {
                let samples = as_u16(src);
                let ef = jpegxl_rs::encode::EncoderFrame::new(samples.as_ref())
                    .num_channels(nch)
                    .endianness(endianness(little_endian));
                enc.encode_frame::<u16, u16>(&ef, width, height)
                    .map_err(|e| Error::Backend(e.to_string()))?
                    .data
            }
        };

        out.write_all(&encoded)?;
        Ok(encoded.len() as u64)
    }

    fn decode(&self, data: &[u8], frame: Frame, out: &mut dyn Write) -> Result<u64> {
        let Frame {
            depth,
            little_endian,
            ..
        } = frame;
        let runner = jpegxl_rs::ThreadsRunner::default();
        let dec = jpegxl_rs::decoder_builder()
            .parallel_runner(&runner)
            .build()
            .map_err(|e| Error::Backend(e.to_string()))?;
        let expected = frame.byte_len();

        let bytes: Vec<u8> = match depth {
            Depth::Eight => {
                dec.decode_with::<u8>(data)
                    .map_err(|e| Error::Backend(e.to_string()))?
                    .1
            }
            Depth::Sixteen => {
                let (_, px) = dec
                    .decode_with::<u16>(data)
                    .map_err(|e| Error::Backend(e.to_string()))?;
                // libjxl hands back native-order samples; write them in the file's order.
                let mut v = Vec::with_capacity(px.len() * 2);
                if little_endian {
                    for s in &px {
                        v.extend_from_slice(&s.to_le_bytes());
                    }
                } else {
                    for s in &px {
                        v.extend_from_slice(&s.to_be_bytes());
                    }
                }
                v
            }
        };

        if bytes.len() as u64 != expected {
            return Err(Error::SizeMismatch {
                expected,
                actual: bytes.len() as u64,
            });
        }
        out.write_all(&bytes)?;
        Ok(bytes.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes; no dev-dependency needed.
    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect()
    }

    fn gray16(w: u32, h: u32, le: bool) -> Frame {
        Frame {
            width: w,
            height: h,
            channels: Channels::Gray,
            depth: Depth::Sixteen,
            little_endian: le,
        }
    }

    fn round_trip(w: u32, h: u32, ch: Channels, depth: Depth, le: bool) {
        let c = Jxl { effort: 1 };
        let f = Frame {
            width: w,
            height: h,
            channels: ch,
            depth,
            little_endian: le,
        };
        let src = noise(f.byte_len() as usize, 99);
        let mut enc = Vec::new();
        c.encode(&src, f, &mut enc).unwrap();
        let mut dec = Vec::new();
        c.decode(&enc, f, &mut dec).unwrap();
        assert_eq!(src, dec, "{w}x{h} {ch:?} {depth:?} le={le}");
    }

    /// The whole archive guarantee rests on this: exactly reversible for every shape
    /// and byte order we hand it.
    #[test]
    fn round_trips_losslessly() {
        round_trip(64, 48, Channels::Gray, Depth::Sixteen, true);
        round_trip(64, 48, Channels::Gray, Depth::Sixteen, false);
        round_trip(40, 24, Channels::Rgb, Depth::Eight, true);
        round_trip(33, 17, Channels::Rgb, Depth::Sixteen, true);
        round_trip(33, 17, Channels::Rgb, Depth::Sixteen, false);
    }

    /// Odd dimensions have no clean 2x2 CFA split, so the archive layer falls back to
    /// whole-frame encoding. That path must still be exact.
    #[test]
    fn handles_odd_dimensions() {
        round_trip(1, 1, Channels::Gray, Depth::Sixteen, true);
        round_trip(7, 3, Channels::Rgb, Depth::Eight, false);
    }

    #[test]
    fn encode_rejects_wrong_byte_count() {
        let c = Jxl::default();
        assert!(matches!(
            c.encode(&[0u8; 10], gray16(4, 4, false), &mut Vec::new()),
            Err(Error::SizeMismatch { .. })
        ));
    }

    /// Byte order must survive the round trip in both directions.
    #[test]
    fn byte_order_is_preserved() {
        let c = Jxl { effort: 1 };
        let src = noise(16 * 16 * 2, 3);
        for le in [true, false] {
            let f = gray16(16, 16, le);
            let mut enc = Vec::new();
            c.encode(&src, f, &mut enc).unwrap();
            let mut out = Vec::new();
            c.decode(&enc, f, &mut out).unwrap();
            assert_eq!(src, out, "le={le}");
        }
    }
}

#[cfg(test)]
mod version_tests {
    /// The version must come from the linked library, so that an archive's record of
    /// how it was made cannot drift from what actually made it.
    #[test]
    fn reports_the_linked_libjxl_version() {
        let v = super::libjxl_version();
        assert!(
            v.starts_with("0.") || v.starts_with("1."),
            "odd version {v}"
        );
        assert_eq!(
            v.matches('.').count(),
            2,
            "expected major.minor.patch, got {v}"
        );
        use super::Codec;
        assert!(super::Jxl::default().describe().contains(&v));
    }
}
