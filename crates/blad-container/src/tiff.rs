//! TIFF/EP parsing.
//!
//! Covers plain TIFF and the raw formats built on it — 3FR, FFF, DNG, NEF, ARW, CR2.
//! We deliberately parse only what is needed to locate pixel data. Everything we do not
//! understand stays verbatim, which is why partial knowledge of a format is still safe.

use super::{tile, Error, ImageSpec, Layout, PixelLayout, Result};
use std::io::{Read, Seek, SeekFrom};

// Tags we care about. Everything else is somebody else's problem (see blad-meta).
const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_ORIENTATION: u16 = 274;
const TAG_SUB_IFDS: u16 = 330;

const COMPRESSION_NONE: u16 = 1;
const PHOTOMETRIC_RGB: u16 = 2;
const PHOTOMETRIC_CFA: u16 = 32803;

/// Ignore small images (thumbnails, previews). Recompressing them complicates the
/// layout for a negligible win; they stay in the skeleton.
const MIN_IMAGE_BYTES: u64 = 1 << 20;

/// Bounds on a hostile file. A crafted TIFF can otherwise ask us to allocate wildly or
/// recurse forever; resource limits are a feature, not an afterthought.
const MAX_IFDS: usize = 64;
const MAX_ENTRIES_PER_IFD: u16 = 4096;
const MAX_VALUES: u64 = 1 << 20;

struct Reader<'a, R: Read + Seek> {
    src: &'a mut R,
    little_endian: bool,
    file_len: u64,
}

impl<'a, R: Read + Seek> Reader<'a, R> {
    fn u16_at(&mut self, off: u64) -> Result<u16> {
        let mut b = [0u8; 2];
        self.src.seek(SeekFrom::Start(off))?;
        self.src.read_exact(&mut b)?;
        Ok(if self.little_endian {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32_at(&mut self, off: u64) -> Result<u32> {
        let mut b = [0u8; 4];
        self.src.seek(SeekFrom::Start(off))?;
        self.src.read_exact(&mut b)?;
        Ok(if self.little_endian {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn bytes_at(&mut self, off: u64, len: usize) -> Result<Vec<u8>> {
        if off.saturating_add(len as u64) > self.file_len {
            return Err(malformed(format!("read of {len} bytes at {off} runs past EOF")));
        }
        let mut v = vec![0u8; len];
        self.src.seek(SeekFrom::Start(off))?;
        self.src.read_exact(&mut v)?;
        Ok(v)
    }
}

fn malformed(detail: String) -> Error {
    Error::Malformed {
        container: "tiff",
        detail,
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    tag: u16,
    dtype: u16,
    count: u32,
    /// Raw contents of the 4-byte value field, unswapped.
    value_field: [u8; 4],
}

fn type_size(dtype: u16) -> Option<u64> {
    Some(match dtype {
        1 | 2 | 6 | 7 => 1,  // BYTE, ASCII, SBYTE, UNDEFINED
        3 | 8 => 2,          // SHORT, SSHORT
        4 | 9 | 11 | 13 => 4, // LONG, SLONG, FLOAT, IFD
        5 | 10 | 12 => 8,    // RATIONAL, SRATIONAL, DOUBLE
        _ => return None,
    })
}

struct Ifd {
    entries: Vec<Entry>,
}

impl Ifd {
    fn get(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    /// Read an entry's values as u32, handling both inline and out-of-line storage.
    fn values<R: Read + Seek>(&self, r: &mut Reader<R>, tag: u16) -> Result<Option<Vec<u32>>> {
        let Some(e) = self.get(tag) else {
            return Ok(None);
        };
        let esize = type_size(e.dtype)
            .ok_or_else(|| malformed(format!("tag {tag}: unknown type {}", e.dtype)))?;
        if u64::from(e.count) > MAX_VALUES {
            return Err(malformed(format!("tag {tag}: {} values exceeds cap", e.count)));
        }
        let total = esize * u64::from(e.count);

        let raw = if total <= 4 {
            e.value_field.to_vec()
        } else {
            let off = u64::from(if r.little_endian {
                u32::from_le_bytes(e.value_field)
            } else {
                u32::from_be_bytes(e.value_field)
            });
            r.bytes_at(off, total as usize)?
        };

        let mut out = Vec::with_capacity(e.count as usize);
        for i in 0..e.count as usize {
            let s = i * esize as usize;
            let v = match esize {
                1 => u32::from(raw[s]),
                2 => {
                    let b = [raw[s], raw[s + 1]];
                    u32::from(if r.little_endian {
                        u16::from_le_bytes(b)
                    } else {
                        u16::from_be_bytes(b)
                    })
                }
                4 => {
                    let b = [raw[s], raw[s + 1], raw[s + 2], raw[s + 3]];
                    if r.little_endian {
                        u32::from_le_bytes(b)
                    } else {
                        u32::from_be_bytes(b)
                    }
                }
                // RATIONAL and friends are never used for the tags we read.
                _ => return Ok(None),
            };
            out.push(v);
        }
        Ok(Some(out))
    }

    fn scalar<R: Read + Seek>(&self, r: &mut Reader<R>, tag: u16) -> Result<Option<u32>> {
        Ok(self.values(r, tag)?.and_then(|v| v.first().copied()))
    }
}

fn read_ifd<R: Read + Seek>(r: &mut Reader<R>, off: u64) -> Result<Ifd> {
    let count = r.u16_at(off)?;
    if count > MAX_ENTRIES_PER_IFD {
        return Err(malformed(format!("IFD at {off} declares {count} entries")));
    }
    let raw = r.bytes_at(off + 2, usize::from(count) * 12)?;
    let mut entries = Vec::with_capacity(usize::from(count));
    for i in 0..usize::from(count) {
        let b = &raw[i * 12..i * 12 + 12];
        let rd16 = |lo: usize| {
            let x = [b[lo], b[lo + 1]];
            if r.little_endian {
                u16::from_le_bytes(x)
            } else {
                u16::from_be_bytes(x)
            }
        };
        let rd32 = |lo: usize| {
            let x = [b[lo], b[lo + 1], b[lo + 2], b[lo + 3]];
            if r.little_endian {
                u32::from_le_bytes(x)
            } else {
                u32::from_be_bytes(x)
            }
        };
        entries.push(Entry {
            tag: rd16(0),
            dtype: rd16(2),
            count: rd32(4),
            value_field: [b[8], b[9], b[10], b[11]],
        });
    }
    Ok(Ifd { entries })
}

/// Walk the IFD chain plus any SubIFDs, breadth-first, with hard caps.
fn collect_ifds<R: Read + Seek>(r: &mut Reader<R>, first: u64) -> Result<Vec<Ifd>> {
    let mut out = Vec::new();
    let mut queue = vec![first];
    let mut seen = Vec::new();

    while let Some(off) = queue.pop() {
        if off == 0 || seen.contains(&off) || out.len() >= MAX_IFDS {
            continue;
        }
        seen.push(off);
        let ifd = read_ifd(r, off)?;

        if let Some(subs) = ifd.values(r, TAG_SUB_IFDS)? {
            queue.extend(subs.into_iter().map(u64::from));
        }
        // Next IFD pointer sits after the entry array.
        let next_off = off + 2 + u64::from(ifd.entries.len() as u32) * 12;
        if next_off + 4 <= r.file_len {
            queue.push(u64::from(r.u32_at(next_off)?));
        }
        out.push(ifd);
    }
    Ok(out)
}

/// Locate the pixel data described by one IFD, if it is something we can model.
///
/// Returns `None` — not an error — for anything unsupported. An unmodelled image simply
/// stays in the skeleton, which is always correct, merely less compact.
fn image_region<R: Read + Seek>(r: &mut Reader<R>, ifd: &Ifd) -> Result<Option<(u64, u64, ImageSpec)>> {
    if ifd.scalar(r, TAG_COMPRESSION)?.unwrap_or(COMPRESSION_NONE as u32) != u32::from(COMPRESSION_NONE) {
        return Ok(None); // already compressed; recompressing it is the Lepton problem
    }
    let (Some(width), Some(height)) = (
        ifd.scalar(r, TAG_IMAGE_WIDTH)?,
        ifd.scalar(r, TAG_IMAGE_LENGTH)?,
    ) else {
        return Ok(None);
    };
    let photometric = ifd.scalar(r, TAG_PHOTOMETRIC)?.unwrap_or(0) as u16;
    let layout = match photometric {
        PHOTOMETRIC_RGB => PixelLayout::Chunky,
        PHOTOMETRIC_CFA => PixelLayout::Cfa,
        _ => return Ok(None),
    };

    let bits = ifd.values(r, TAG_BITS_PER_SAMPLE)?.unwrap_or_default();
    if bits.is_empty() || bits.iter().any(|&b| b != bits[0]) || (bits[0] != 8 && bits[0] != 16) {
        return Ok(None); // mixed or exotic bit depths: not worth modelling yet
    }
    let samples = ifd.scalar(r, TAG_SAMPLES_PER_PIXEL)?.unwrap_or(1) as u16;

    let (Some(offsets), Some(counts)) = (
        ifd.values(r, TAG_STRIP_OFFSETS)?,
        ifd.values(r, TAG_STRIP_BYTE_COUNTS)?,
    ) else {
        return Ok(None);
    };
    if offsets.is_empty() || offsets.len() != counts.len() {
        return Ok(None);
    }

    // Accept multiple strips only when they are contiguous and in order, so the whole
    // image is one flat run we can hand to a codec.
    let start = u64::from(offsets[0]);
    let mut cursor = start;
    for (&o, &c) in offsets.iter().zip(counts.iter()) {
        if u64::from(o) != cursor {
            return Ok(None);
        }
        cursor += u64::from(c);
    }
    let len = cursor - start;

    let spec = ImageSpec {
        width,
        height,
        bits_per_sample: bits[0] as u16,
        samples_per_pixel: samples,
        layout,
        little_endian: r.little_endian,
    };
    if spec.byte_len() != len || len < MIN_IMAGE_BYTES {
        return Ok(None);
    }
    if start + len > r.file_len {
        return Err(malformed(format!("strip data at {start}+{len} runs past EOF")));
    }
    Ok(Some((start, len, spec)))
}

pub fn analyze<R: Read + Seek>(src: &mut R, file_len: u64) -> Result<Layout> {
    let mut head = [0u8; 8];
    src.seek(SeekFrom::Start(0))?;
    src.read_exact(&mut head)?;
    let little_endian = match &head[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err(Error::UnknownFormat),
    };
    let mut r = Reader {
        src,
        little_endian,
        file_len,
    };
    let magic = r.u16_at(2)?;
    if magic != 42 {
        return Err(Error::Unsupported(format!("TIFF magic {magic} (BigTIFF?)")));
    }
    let ifd0 = u64::from(r.u32_at(4)?);

    let ifds = collect_ifds(&mut r, ifd0)?;
    // Orientation lives in IFD0, which is the first entry collect_ifds returns.
    let orientation = ifds
        .first()
        .and_then(|ifd| ifd.scalar(&mut r, TAG_ORIENTATION).ok().flatten())
        .filter(|v| (1..=8).contains(v))
        .unwrap_or(1) as u16;
    let mut regions = Vec::new();
    for ifd in &ifds {
        if let Some(region) = image_region(&mut r, ifd)? {
            regions.push(region);
        }
    }

    tile("tiff", file_len, orientation, regions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a minimal little-endian TIFF with one uncompressed image strip.
    fn synth(width: u32, height: u32, samples: u16, photometric: u16) -> Vec<u8> {
        let px = (width * height * u32::from(samples) * 2) as usize;
        let entries: [(u16, u16, u32, u32); 7] = [
            (TAG_IMAGE_WIDTH, 4, 1, width),
            (TAG_IMAGE_LENGTH, 4, 1, height),
            (TAG_BITS_PER_SAMPLE, 3, 1, 16),
            (TAG_COMPRESSION, 3, 1, 1),
            (TAG_PHOTOMETRIC, 3, 1, u32::from(photometric)),
            (TAG_SAMPLES_PER_PIXEL, 3, 1, u32::from(samples)),
            (TAG_STRIP_BYTE_COUNTS, 4, 1, px as u32),
        ];
        // header(8) + count(2) + entries*12 + next(4), then StripOffsets appended
        let n = entries.len() + 1;
        let ifd_off = 8u32;
        let data_off = ifd_off + 2 + (n as u32) * 12 + 4;

        let mut b = Vec::new();
        b.extend_from_slice(b"II");
        b.extend_from_slice(&42u16.to_le_bytes());
        b.extend_from_slice(&ifd_off.to_le_bytes());
        b.extend_from_slice(&(n as u16).to_le_bytes());
        let mut all: Vec<(u16, u16, u32, u32)> = entries.to_vec();
        all.push((TAG_STRIP_OFFSETS, 4, 1, data_off));
        all.sort_by_key(|e| e.0);
        for (tag, dtype, count, val) in all {
            b.extend_from_slice(&tag.to_le_bytes());
            b.extend_from_slice(&dtype.to_le_bytes());
            b.extend_from_slice(&count.to_le_bytes());
            if dtype == 3 {
                b.extend_from_slice(&(val as u16).to_le_bytes());
                b.extend_from_slice(&[0, 0]);
            } else {
                b.extend_from_slice(&val.to_le_bytes());
            }
        }
        b.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(b.len() as u32, data_off);
        b.extend(std::iter::repeat(0xAB).take(px));
        b
    }

    #[test]
    fn finds_cfa_strip_and_tiles_file() {
        let w = 1024;
        let h = 1024;
        let buf = synth(w, h, 1, PHOTOMETRIC_CFA);
        let len = buf.len() as u64;
        let l = analyze(&mut Cursor::new(&buf), len).unwrap();
        l.validate().unwrap();

        assert_eq!(l.payload_len(), u64::from(w) * u64::from(h) * 2);
        assert_eq!(l.skeleton_len(), len - l.payload_len());
        let (_, _, spec) = l.image_segments().next().unwrap();
        assert_eq!(spec.layout, PixelLayout::Cfa);
        assert!(spec.little_endian);
    }

    #[test]
    fn finds_rgb_strip() {
        let buf = synth(512, 700, 3, PHOTOMETRIC_RGB);
        let len = buf.len() as u64;
        let l = analyze(&mut Cursor::new(&buf), len).unwrap();
        let (_, _, spec) = l.image_segments().next().unwrap();
        assert_eq!(spec.layout, PixelLayout::Chunky);
        assert_eq!(spec.samples_per_pixel, 3);
    }

    #[test]
    fn small_images_stay_in_skeleton() {
        // 64x64 is below MIN_IMAGE_BYTES, so nothing should be modelled.
        let buf = synth(64, 64, 1, PHOTOMETRIC_CFA);
        let len = buf.len() as u64;
        let l = analyze(&mut Cursor::new(&buf), len).unwrap();
        assert_eq!(l.payload_len(), 0);
        assert_eq!(l.skeleton_len(), len);
    }

    #[test]
    fn rejects_non_tiff() {
        let buf = vec![0u8; 64];
        assert!(analyze(&mut Cursor::new(&buf), 64).is_err());
    }

    #[test]
    fn truncated_file_is_an_error_not_a_panic() {
        let mut buf = synth(1024, 1024, 1, PHOTOMETRIC_CFA);
        buf.truncate(buf.len() / 2);
        let len = buf.len() as u64;
        // Must not panic; either an error or a layout with no image segment is fine.
        match analyze(&mut Cursor::new(&buf), len) {
            Ok(l) => assert_eq!(l.payload_len(), 0),
            Err(_) => {}
        }
    }
}
