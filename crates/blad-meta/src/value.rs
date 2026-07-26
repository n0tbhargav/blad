//! Typed decoding of TIFF entry bytes, and turning those values into text.
//!
//! Two rules govern everything here:
//!
//! 1. **Never guess.** A tag we have no dictionary entry for is shown with its number,
//!    type and count — not with an invented name or a plausible-looking interpretation.
//! 2. **Never dump binary to a terminal.** Large `UNDEFINED` blobs (maker notes, ICC
//!    profiles, embedded previews) are described, not printed.

use blad_container::ifd::RawEntry;

/// TIFF type codes.
pub const BYTE: u16 = 1;
pub const ASCII: u16 = 2;
pub const SHORT: u16 = 3;
pub const LONG: u16 = 4;
pub const RATIONAL: u16 = 5;
pub const SBYTE: u16 = 6;
pub const UNDEFINED: u16 = 7;
pub const SSHORT: u16 = 8;
pub const SLONG: u16 = 9;
pub const SRATIONAL: u16 = 10;
pub const FLOAT: u16 = 11;
pub const DOUBLE: u16 = 12;
pub const IFD: u16 = 13;

pub fn type_name(dtype: u16) -> &'static str {
    match dtype {
        BYTE => "BYTE",
        ASCII => "ASCII",
        SHORT => "SHORT",
        LONG => "LONG",
        RATIONAL => "RATIONAL",
        SBYTE => "SBYTE",
        UNDEFINED => "UNDEFINED",
        SSHORT => "SSHORT",
        SLONG => "SLONG",
        SRATIONAL => "SRATIONAL",
        FLOAT => "FLOAT",
        DOUBLE => "DOUBLE",
        IFD => "IFD",
        _ => "?",
    }
}

/// A decoded entry value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Text(String),
    Uint(Vec<u64>),
    Int(Vec<i64>),
    /// Numerator/denominator pairs, kept unreduced so the file's own representation
    /// survives — `10/300` and `1/30` are the same number but not the same bytes.
    Rational(Vec<(i64, i64)>),
    Real(Vec<f64>),
    /// Bytes we deliberately decline to interpret.
    Binary(usize),
    Unreadable(String),
}

impl Value {
    pub fn len(&self) -> usize {
        match self {
            Value::Text(_) => 1,
            Value::Uint(v) => v.len(),
            Value::Int(v) => v.len(),
            Value::Rational(v) => v.len(),
            Value::Real(v) => v.len(),
            Value::Binary(_) | Value::Unreadable(_) => 1,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// First value as f64, for tags whose formatting needs arithmetic.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Uint(v) => v.first().map(|&x| x as f64),
            Value::Int(v) => v.first().map(|&x| x as f64),
            Value::Real(v) => v.first().copied(),
            Value::Rational(v) => v.first().and_then(|&(n, d)| {
                if d == 0 {
                    None
                } else {
                    Some(n as f64 / d as f64)
                }
            }),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Uint(v) => v.first().copied(),
            Value::Int(v) => v.first().map(|&x| x as u64),
            _ => None,
        }
    }
}

/// Anything larger than this is described rather than decoded. Maker notes and embedded
/// previews run to megabytes; nobody wants them expanded into a terminal.
const BINARY_THRESHOLD: usize = 128;

pub fn decode(e: &RawEntry, little_endian: bool) -> Value {
    if let Some(why) = &e.unreadable {
        return Value::Unreadable(why.clone());
    }
    let b = &e.bytes;
    let rd16 = |c: &[u8]| {
        let x = [c[0], c[1]];
        if little_endian {
            u16::from_le_bytes(x)
        } else {
            u16::from_be_bytes(x)
        }
    };
    let rd32 = |c: &[u8]| {
        let x = [c[0], c[1], c[2], c[3]];
        if little_endian {
            u32::from_le_bytes(x)
        } else {
            u32::from_be_bytes(x)
        }
    };

    match e.dtype {
        ASCII => {
            // Exif strings are NUL-terminated; trailing NULs are padding, not content.
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            Value::Text(String::from_utf8_lossy(&b[..end]).trim_end().to_string())
        }
        BYTE if b.len() > BINARY_THRESHOLD => Value::Binary(b.len()),
        UNDEFINED => {
            if b.len() > BINARY_THRESHOLD {
                Value::Binary(b.len())
            } else if b.iter().all(|&c| c == 0 || (0x20..0x7f).contains(&c)) && !b.is_empty() {
                let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                Value::Text(String::from_utf8_lossy(&b[..end]).trim_end().to_string())
            } else {
                Value::Uint(b.iter().map(|&x| u64::from(x)).collect())
            }
        }
        BYTE => Value::Uint(b.iter().map(|&x| u64::from(x)).collect()),
        SBYTE => Value::Int(b.iter().map(|&x| i64::from(x as i8)).collect()),
        SHORT => Value::Uint(b.chunks_exact(2).map(|c| u64::from(rd16(c))).collect()),
        SSHORT => Value::Int(
            b.chunks_exact(2)
                .map(|c| i64::from(rd16(c) as i16))
                .collect(),
        ),
        LONG | IFD => Value::Uint(b.chunks_exact(4).map(|c| u64::from(rd32(c))).collect()),
        SLONG => Value::Int(
            b.chunks_exact(4)
                .map(|c| i64::from(rd32(c) as i32))
                .collect(),
        ),
        RATIONAL => Value::Rational(
            b.chunks_exact(8)
                .map(|c| (i64::from(rd32(&c[0..4])), i64::from(rd32(&c[4..8]))))
                .collect(),
        ),
        SRATIONAL => Value::Rational(
            b.chunks_exact(8)
                .map(|c| {
                    (
                        i64::from(rd32(&c[0..4]) as i32),
                        i64::from(rd32(&c[4..8]) as i32),
                    )
                })
                .collect(),
        ),
        FLOAT => Value::Real(
            b.chunks_exact(4)
                .map(|c| f64::from(f32::from_bits(rd32(c))))
                .collect(),
        ),
        DOUBLE => Value::Real(
            b.chunks_exact(8)
                .map(|c| {
                    let hi = u64::from(rd32(&c[0..4]));
                    let lo = u64::from(rd32(&c[4..8]));
                    f64::from_bits(if little_endian {
                        (lo << 32) | hi
                    } else {
                        (hi << 32) | lo
                    })
                })
                .collect(),
        ),
        _ => Value::Unreadable(format!("unknown TIFF type {}", e.dtype)),
    }
}

fn fmt_rational(n: i64, d: i64) -> String {
    if d == 0 {
        return format!("{n}/0");
    }
    if n % d == 0 {
        return (n / d).to_string();
    }
    let v = n as f64 / d as f64;
    // Keep the fraction only in the form photographers actually read — 1/125. A value
    // like AsShotNeutral is stored as 447106/1000000 and means 0.447106; printing the
    // stored fraction there is technically faithful and practically unreadable.
    if n == 1 && d > 1 {
        format!("{n}/{d}")
    } else {
        format!("{v:.6}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

/// Render a value as a single line, truncating long arrays.
pub fn render(v: &Value, max_items: usize) -> String {
    fn join<T: std::fmt::Display>(items: &[T], max: usize) -> String {
        let shown: Vec<String> = items.iter().take(max).map(|x| x.to_string()).collect();
        if items.len() > max {
            format!("{} ⋯ ({} values)", shown.join(", "), items.len())
        } else {
            shown.join(", ")
        }
    }

    match v {
        Value::Text(s) => s.clone(),
        Value::Uint(x) => join(x, max_items),
        Value::Int(x) => join(x, max_items),
        Value::Real(x) => {
            let s: Vec<String> = x
                .iter()
                .take(max_items)
                .map(|f| {
                    format!("{f:.6}")
                        .trim_end_matches('0')
                        .trim_end_matches('.')
                        .to_string()
                })
                .collect();
            if x.len() > max_items {
                format!("{} ⋯ ({} values)", s.join(", "), x.len())
            } else {
                s.join(", ")
            }
        }
        Value::Rational(x) => {
            let s: Vec<String> = x
                .iter()
                .take(max_items)
                .map(|&(n, d)| fmt_rational(n, d))
                .collect();
            if x.len() > max_items {
                format!("{} ⋯ ({} values)", s.join(", "), x.len())
            } else {
                s.join(", ")
            }
        }
        Value::Binary(n) => format!("<binary, {}>", human_bytes(*n as u64)),
        Value::Unreadable(why) => format!("<unreadable: {why}>"),
    }
}

pub fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(dtype: u16, count: u32, bytes: &[u8]) -> RawEntry {
        RawEntry {
            tag: 0,
            dtype,
            count,
            value_offset: 0,
            inline: false,
            bytes: bytes.to_vec(),
            unreadable: None,
        }
    }

    #[test]
    fn ascii_stops_at_nul() {
        let v = decode(&entry(ASCII, 12, b"Hasselblad\0\0"), true);
        assert_eq!(v, Value::Text("Hasselblad".into()));
    }

    #[test]
    fn rationals_keep_shutter_speeds_as_fractions() {
        // 1/125 must not become 0.008 — photographers read shutter speeds as fractions.
        let mut b = 1u32.to_le_bytes().to_vec();
        b.extend(125u32.to_le_bytes());
        let v = decode(&entry(RATIONAL, 1, &b), true);
        assert_eq!(render(&v, 8), "1/125");
    }

    /// 447106/1000000 is faithful and unreadable. Only 1/n survives as a fraction.
    #[test]
    fn non_unit_fractions_become_decimals() {
        let mut b = 447106u32.to_le_bytes().to_vec();
        b.extend(1000000u32.to_le_bytes());
        assert_eq!(
            render(&decode(&entry(RATIONAL, 1, &b), true), 8),
            "0.447106"
        );
    }

    #[test]
    fn whole_rationals_lose_the_denominator() {
        let mut b = 300u32.to_le_bytes().to_vec();
        b.extend(1u32.to_le_bytes());
        assert_eq!(render(&decode(&entry(RATIONAL, 1, &b), true), 8), "300");
    }

    /// A megabyte maker note must be described, never expanded.
    #[test]
    fn large_undefined_is_described_not_dumped() {
        let big = vec![0xABu8; 4096];
        let v = decode(&entry(UNDEFINED, 4096, &big), true);
        assert!(matches!(v, Value::Binary(4096)));
        assert_eq!(render(&v, 8), "<binary, 4.0 KB>");
    }

    #[test]
    fn long_arrays_truncate_with_a_count() {
        let b: Vec<u8> = (0..40u8).collect();
        let v = decode(&entry(BYTE, 40, &b), true);
        let s = render(&v, 4);
        assert!(s.ends_with("⋯ (40 values)"), "{s}");
    }

    #[test]
    fn big_endian_shorts_decode_correctly() {
        let v = decode(&entry(SHORT, 1, &[0x01, 0x00]), false);
        assert_eq!(v, Value::Uint(vec![256]));
    }

    #[test]
    fn unreadable_entries_survive_decoding() {
        let mut e = entry(BYTE, 4, &[]);
        e.unreadable = Some("past EOF".into());
        assert!(matches!(decode(&e, true), Value::Unreadable(_)));
    }
}
