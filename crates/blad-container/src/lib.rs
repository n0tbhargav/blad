//! Container parsing and lossless decomposition.
//!
//! The central idea: any image file can be described as an ordered list of byte
//! [`Segment`]s covering it completely and without overlap. Most segments are
//! [`SegmentKind::Verbatim`] — headers, IFDs, metadata, previews — and are stored as-is
//! because they are small and not worth modelling. One or more segments are
//! [`SegmentKind::Image`], holding raw pixel or sensor data, and those are worth handing
//! to a real image codec.
//!
//! Reassembling the segments in `src_offset` order must reproduce the original file
//! byte for byte. That property is the entire contract, and it is what makes archival
//! safe: we never need to understand a file completely, only the parts we choose to
//! recompress.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub mod tiff;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a recognised container")]
    UnknownFormat,
    #[error("malformed {container}: {detail}")]
    Malformed {
        container: &'static str,
        detail: String,
    },
    #[error("unsupported: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// How pixel data in an image segment is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PixelLayout {
    /// Interleaved samples, e.g. RGBRGB.
    Chunky,
    /// Single-channel colour filter array (Bayer mosaic).
    Cfa,
}

/// Description of a run of raw pixel data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImageSpec {
    pub width: u32,
    pub height: u32,
    pub bits_per_sample: u16,
    pub samples_per_pixel: u16,
    pub layout: PixelLayout,
    /// Byte order of the samples *as stored in the file*.
    pub little_endian: bool,
}

impl ImageSpec {
    /// Expected byte length of the pixel data.
    pub fn byte_len(&self) -> u64 {
        u64::from(self.width)
            * u64::from(self.height)
            * u64::from(self.samples_per_pixel)
            * u64::from(self.bits_per_sample / 8)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SegmentKind {
    /// Stored byte-for-byte. Headers, metadata, previews, anything we do not model.
    Verbatim,
    /// Raw pixel data, eligible for recompression by a codec.
    Image(ImageSpec),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    pub src_offset: u64,
    pub len: u64,
    pub kind: SegmentKind,
}

/// A complete, gapless, non-overlapping description of a file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Layout {
    pub container: String,
    pub total_len: u64,
    pub segments: Vec<Segment>,
}

impl Layout {
    /// Verify the invariant the whole design rests on: segments must tile the file
    /// exactly — sorted, contiguous, starting at 0, ending at `total_len`.
    ///
    /// Called on every archive and every restore. A layout that fails this can never
    /// reproduce the original, so we refuse it rather than write an archive we cannot
    /// honour.
    pub fn validate(&self) -> Result<()> {
        let mut cursor = 0u64;
        for (i, s) in self.segments.iter().enumerate() {
            if s.src_offset != cursor {
                return Err(Error::Malformed {
                    container: "layout",
                    detail: format!(
                        "segment {i} starts at {} but previous coverage ended at {cursor}",
                        s.src_offset
                    ),
                });
            }
            if let SegmentKind::Image(spec) = &s.kind {
                if spec.byte_len() != s.len {
                    return Err(Error::Malformed {
                        container: "layout",
                        detail: format!(
                            "segment {i}: spec implies {} bytes, segment claims {}",
                            spec.byte_len(),
                            s.len
                        ),
                    });
                }
            }
            cursor = cursor.checked_add(s.len).ok_or_else(|| Error::Malformed {
                container: "layout",
                detail: "segment length overflow".into(),
            })?;
        }
        if cursor != self.total_len {
            return Err(Error::Malformed {
                container: "layout",
                detail: format!("segments cover {cursor} bytes, file is {}", self.total_len),
            });
        }
        Ok(())
    }

    pub fn image_segments(&self) -> impl Iterator<Item = (usize, &Segment, &ImageSpec)> {
        self.segments.iter().enumerate().filter_map(|(i, s)| match &s.kind {
            SegmentKind::Image(spec) => Some((i, s, spec)),
            SegmentKind::Verbatim => None,
        })
    }

    /// Total bytes held in image segments — the part a codec can act on.
    pub fn payload_len(&self) -> u64 {
        self.image_segments().map(|(_, s, _)| s.len).sum()
    }

    /// Total bytes stored verbatim — the skeleton.
    pub fn skeleton_len(&self) -> u64 {
        self.total_len - self.payload_len()
    }
}

/// Identify a file and decompose it.
pub fn analyze(path: &Path) -> Result<Layout> {
    let mut f = File::open(path)?;
    let total_len = f.metadata()?.len();
    let mut magic = [0u8; 4];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut magic)?;
    f.seek(SeekFrom::Start(0))?;

    match &magic {
        // TIFF/EP and everything built on it: TIFF, DNG, 3FR, FFF, NEF, ARW, CR2 …
        [b'I', b'I', 42, 0] | [b'M', b'M', 0, 42] => tiff::analyze(&mut f, total_len),
        _ => Err(Error::UnknownFormat),
    }
}

/// Build a gapless layout from a set of known image regions, filling everything else
/// with verbatim segments.
///
/// Regions must be non-overlapping; they are sorted here, so callers may discover them
/// in any order (IFD traversal order is not file order).
pub(crate) fn tile(
    container: &str,
    total_len: u64,
    mut regions: Vec<(u64, u64, ImageSpec)>,
) -> Result<Layout> {
    regions.sort_by_key(|(off, _, _)| *off);

    let mut segments = Vec::with_capacity(regions.len() * 2 + 1);
    let mut cursor = 0u64;
    for (off, len, spec) in regions {
        if off < cursor {
            return Err(Error::Malformed {
                container: "layout",
                detail: format!("image region at {off} overlaps previous coverage ending {cursor}"),
            });
        }
        if off > cursor {
            segments.push(Segment {
                src_offset: cursor,
                len: off - cursor,
                kind: SegmentKind::Verbatim,
            });
        }
        segments.push(Segment {
            src_offset: off,
            len,
            kind: SegmentKind::Image(spec),
        });
        cursor = off + len;
    }
    if cursor < total_len {
        segments.push(Segment {
            src_offset: cursor,
            len: total_len - cursor,
            kind: SegmentKind::Verbatim,
        });
    }

    let layout = Layout {
        container: container.to_string(),
        total_len,
        segments,
    };
    layout.validate()?;
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(w: u32, h: u32) -> ImageSpec {
        ImageSpec {
            width: w,
            height: h,
            bits_per_sample: 16,
            samples_per_pixel: 1,
            layout: PixelLayout::Cfa,
            little_endian: true,
        }
    }

    #[test]
    fn tile_fills_gaps_and_validates() {
        // 100-byte file with one 40-byte image region at offset 20.
        let l = tile("test", 100, vec![(20, 40, spec(4, 5))]).unwrap();
        assert_eq!(l.segments.len(), 3);
        assert_eq!(l.segments[0].kind, SegmentKind::Verbatim);
        assert_eq!(l.segments[0].len, 20);
        assert!(matches!(l.segments[1].kind, SegmentKind::Image(_)));
        assert_eq!(l.segments[2].src_offset, 60);
        assert_eq!(l.segments[2].len, 40);
        assert_eq!(l.payload_len(), 40);
        assert_eq!(l.skeleton_len(), 60);
    }

    #[test]
    fn tile_sorts_out_of_order_regions() {
        let l = tile("test", 100, vec![(60, 20, spec(2, 5)), (10, 20, spec(2, 5))]).unwrap();
        l.validate().unwrap();
        let offsets: Vec<u64> = l.segments.iter().map(|s| s.src_offset).collect();
        assert_eq!(offsets, vec![0, 10, 30, 60, 80]);
    }

    #[test]
    fn image_region_at_file_start_and_end_needs_no_padding() {
        let l = tile("test", 40, vec![(0, 40, spec(4, 5))]).unwrap();
        assert_eq!(l.segments.len(), 1);
        assert_eq!(l.skeleton_len(), 0);
    }

    #[test]
    fn overlapping_regions_are_rejected() {
        let e = tile("test", 100, vec![(10, 30, spec(2, 5)), (20, 20, spec(2, 5))]);
        assert!(e.is_err());
    }

    #[test]
    fn spec_mismatch_is_rejected() {
        // Claim 40 bytes but the spec describes 4*5*1*2 = 40 … make it disagree.
        let bad = tile("test", 100, vec![(0, 39, spec(4, 5))]);
        assert!(bad.is_err());
    }

    #[test]
    fn validate_rejects_short_coverage() {
        let l = Layout {
            container: "test".into(),
            total_len: 100,
            segments: vec![Segment {
                src_offset: 0,
                len: 50,
                kind: SegmentKind::Verbatim,
            }],
        };
        assert!(l.validate().is_err());
    }
}
