//! Public, lossless access to a TIFF file's directory structure.
//!
//! The archival path reads only the handful of tags needed to find pixels. This module
//! is the other half: every entry in every directory, with its type, count, and the
//! offset its bytes live at, and no interpretation whatsoever.
//!
//! The split matters. [`crate::tiff::analyze`] must never be influenced by metadata it
//! does not understand, and a metadata reader must never drop something merely because
//! archival had no use for it. Keeping them separate means neither constrains the other;
//! naming and units are somebody else's problem (see `blad-meta`).

use crate::tiff::{
    malformed, read_ifd, type_size, Reader, MAX_ENTRIES_PER_IFD, MAX_IFDS, MAX_VALUES,
};
use crate::{Error, Result};
use std::fs::File;
use std::io::{Read, Seek};
use std::path::Path;

/// Tags whose value is a pointer to another directory.
const TAG_SUB_IFDS: u16 = 330;
const TAG_EXIF_IFD: u16 = 34665;
const TAG_GPS_IFD: u16 = 34853;
const TAG_INTEROP_IFD: u16 = 40965;

/// Which directory an entry was found in.
///
/// Worth distinguishing because the same tag number means different things in different
/// directories — tag 1 is `InteroperabilityIndex` under Interop and `GPSLatitudeRef`
/// under GPS. A reader that forgets where an entry came from cannot name it correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum IfdKind {
    /// Main image directory, and any further directories chained after it.
    Main(u16),
    /// A directory referenced by tag 330. On raw files this is where the sensor lives.
    Sub(u16),
    Exif,
    Gps,
    Interop,
}

impl IfdKind {
    pub fn label(&self) -> String {
        match self {
            IfdKind::Main(0) => "IFD0".into(),
            IfdKind::Main(n) => format!("IFD{n}"),
            IfdKind::Sub(n) => format!("SubIFD{n}"),
            IfdKind::Exif => "Exif".into(),
            IfdKind::Gps => "GPS".into(),
            IfdKind::Interop => "Interop".into(),
        }
    }
}

/// One directory entry, with its bytes but without any interpretation of them.
#[derive(Debug, Clone)]
pub struct RawEntry {
    pub tag: u16,
    /// TIFF type code. Kept as a number because an unrecognised type must still be
    /// reportable — refusing to describe an entry we cannot decode would hide it.
    pub dtype: u16,
    pub count: u32,
    /// Where the value bytes live. For values of four bytes or fewer this points inside
    /// the 12-byte entry itself.
    pub value_offset: u64,
    /// True when the value was small enough to be stored in the entry.
    pub inline: bool,
    /// The value bytes, in file order. Empty if the entry could not be read — which is
    /// reported rather than treated as fatal, because one bad entry should not cost you
    /// the other two hundred.
    pub bytes: Vec<u8>,
    /// Why `bytes` is empty, when it is.
    pub unreadable: Option<String>,
}

impl RawEntry {
    /// Byte length the type and count imply, whether or not it was readable.
    pub fn declared_len(&self) -> u64 {
        type_size(self.dtype).unwrap_or(0) * u64::from(self.count)
    }
}

#[derive(Debug, Clone)]
pub struct RawIfd {
    pub kind: IfdKind,
    pub offset: u64,
    pub entries: Vec<RawEntry>,
}

#[derive(Debug, Clone)]
pub struct Directories {
    pub little_endian: bool,
    pub file_len: u64,
    pub ifds: Vec<RawIfd>,
}

impl Directories {
    pub fn entry(&self, kind: IfdKind, tag: u16) -> Option<&RawEntry> {
        self.ifds
            .iter()
            .find(|i| i.kind == kind)?
            .entries
            .iter()
            .find(|e| e.tag == tag)
    }
}

/// Read every directory in a TIFF-based file.
pub fn read(path: &Path) -> Result<Directories> {
    let mut f = File::open(path)?;
    let file_len = f.metadata()?.len();
    read_from(&mut f, file_len)
}

pub fn read_from<R: Read + Seek>(src: &mut R, file_len: u64) -> Result<Directories> {
    let mut magic = [0u8; 4];
    src.seek(std::io::SeekFrom::Start(0))?;
    src.read_exact(&mut magic)
        .map_err(|_| Error::UnknownFormat)?;
    let little_endian = match &magic {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return Err(Error::UnknownFormat),
    };

    let mut r = Reader {
        src,
        little_endian,
        file_len,
    };
    let first = u64::from(r.u32_at(4)?);

    // Breadth-first, so IFD0 is reported before the directories it points at — which is
    // both the order a reader expects and the order the file is usually laid out in.
    let mut ifds: Vec<RawIfd> = Vec::new();
    let mut seen: Vec<u64> = Vec::new();
    let mut queue: std::collections::VecDeque<(u64, IfdKind)> = std::collections::VecDeque::new();
    queue.push_back((first, IfdKind::Main(0)));

    let mut sub_n = 0u16;
    let mut main_n = 0u16;

    while let Some((off, kind)) = queue.pop_front() {
        if off == 0 || off >= file_len || seen.contains(&off) || ifds.len() >= MAX_IFDS {
            continue;
        }
        seen.push(off);

        let ifd = match read_ifd(&mut r, off) {
            Ok(i) => i,
            // A directory that will not parse should not cost us the ones that will.
            Err(_) => continue,
        };
        if ifd.entries.len() > usize::from(MAX_ENTRIES_PER_IFD) {
            continue;
        }

        let mut entries = Vec::with_capacity(ifd.entries.len());
        for (i, e) in ifd.entries.iter().enumerate() {
            let entry_off = off + 2 + (i as u64) * 12;
            let esize = type_size(e.dtype).unwrap_or(0);
            let total = esize.saturating_mul(u64::from(e.count));
            let inline = total <= 4;
            let value_offset = if inline {
                entry_off + 8
            } else {
                u64::from(if little_endian {
                    u32::from_le_bytes(e.value_field)
                } else {
                    u32::from_be_bytes(e.value_field)
                })
            };

            let (bytes, unreadable) = if esize == 0 {
                (Vec::new(), Some(format!("unknown TIFF type {}", e.dtype)))
            } else if u64::from(e.count) > MAX_VALUES {
                (
                    Vec::new(),
                    Some(format!("{} values exceeds the {MAX_VALUES} cap", e.count)),
                )
            } else if inline {
                (e.value_field[..total as usize].to_vec(), None)
            } else {
                match r.bytes_at(value_offset, total as usize) {
                    Ok(b) => (b, None),
                    Err(err) => (Vec::new(), Some(err.to_string())),
                }
            };

            // Follow pointers to nested directories.
            if unreadable.is_none() {
                let ptrs: Vec<u64> = match e.tag {
                    TAG_SUB_IFDS => decode_offsets(&bytes, esize, little_endian),
                    TAG_EXIF_IFD | TAG_GPS_IFD | TAG_INTEROP_IFD => {
                        decode_offsets(&bytes, esize, little_endian)
                    }
                    _ => Vec::new(),
                };
                for p in ptrs {
                    let k = match e.tag {
                        TAG_SUB_IFDS => {
                            sub_n += 1;
                            IfdKind::Sub(sub_n - 1)
                        }
                        TAG_EXIF_IFD => IfdKind::Exif,
                        TAG_GPS_IFD => IfdKind::Gps,
                        _ => IfdKind::Interop,
                    };
                    queue.push_back((p, k));
                }
            }

            entries.push(RawEntry {
                tag: e.tag,
                dtype: e.dtype,
                count: e.count,
                value_offset,
                inline,
                bytes,
                unreadable,
            });
        }

        // The next-IFD pointer follows the entry array. Only main directories chain;
        // Exif and GPS sub-directories terminate.
        if matches!(kind, IfdKind::Main(_)) {
            let next_off = off + 2 + (ifd.entries.len() as u64) * 12;
            if next_off + 4 <= file_len {
                if let Ok(next) = r.u32_at(next_off) {
                    main_n += 1;
                    queue.push_back((u64::from(next), IfdKind::Main(main_n)));
                }
            }
        }

        ifds.push(RawIfd {
            kind,
            offset: off,
            entries,
        });
    }

    if ifds.is_empty() {
        return Err(malformed("no readable directories".into()));
    }

    Ok(Directories {
        little_endian,
        file_len,
        ifds,
    })
}

fn decode_offsets(bytes: &[u8], esize: u64, le: bool) -> Vec<u64> {
    if esize != 4 {
        return Vec::new();
    }
    bytes
        .chunks_exact(4)
        .map(|c| {
            let b = [c[0], c[1], c[2], c[3]];
            u64::from(if le {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Minimal little-endian TIFF: header, one IFD with `entries`, then a next-IFD of 0.
    fn tiff(entries: &[(u16, u16, u32, [u8; 4])]) -> Vec<u8> {
        let mut b = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
        b.extend((entries.len() as u16).to_le_bytes());
        for (tag, dtype, count, val) in entries {
            b.extend(tag.to_le_bytes());
            b.extend(dtype.to_le_bytes());
            b.extend(count.to_le_bytes());
            b.extend(val);
        }
        b.extend(0u32.to_le_bytes());
        b
    }

    #[test]
    fn reads_entries_with_offsets_and_inline_flag() {
        let buf = tiff(&[(271, 2, 3, *b"HB\0\0")]);
        let len = buf.len() as u64;
        let d = read_from(&mut Cursor::new(&buf), len).unwrap();
        assert_eq!(d.ifds.len(), 1);
        let e = &d.ifds[0].entries[0];
        assert_eq!(e.tag, 271);
        assert!(e.inline);
        assert_eq!(e.bytes, b"HB\0");
        assert_eq!(e.declared_len(), 3);
    }

    /// An entry we cannot decode must still be reported. Dropping it would make the
    /// output silently incomplete, which is worse than saying "this one is broken".
    #[test]
    fn unknown_type_is_reported_not_dropped() {
        let buf = tiff(&[(999, 77, 1, [0; 4])]);
        let len = buf.len() as u64;
        let d = read_from(&mut Cursor::new(&buf), len).unwrap();
        let e = &d.ifds[0].entries[0];
        assert_eq!(e.tag, 999);
        assert!(e.unreadable.is_some());
        assert!(e.bytes.is_empty());
    }

    /// An out-of-line value pointing past EOF must not be fatal.
    #[test]
    fn value_pointer_past_eof_is_survivable() {
        let buf = tiff(&[(700, 1, 64, 0xFFFF_0000u32.to_le_bytes())]);
        let len = buf.len() as u64;
        let d = read_from(&mut Cursor::new(&buf), len).unwrap();
        assert!(d.ifds[0].entries[0].unreadable.is_some());
    }

    #[test]
    fn non_tiff_is_rejected() {
        let buf = vec![0u8; 32];
        assert!(read_from(&mut Cursor::new(&buf), 32).is_err());
    }

    /// A directory that points at itself must terminate.
    #[test]
    fn self_referential_subifd_terminates() {
        let buf = tiff(&[(330, 4, 1, 8u32.to_le_bytes())]);
        let len = buf.len() as u64;
        let d = read_from(&mut Cursor::new(&buf), len).unwrap();
        assert_eq!(d.ifds.len(), 1);
    }
}
