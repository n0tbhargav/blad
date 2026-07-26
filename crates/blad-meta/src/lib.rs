//! Metadata reading: names, types, and values for every directory entry in a file.
//!
//! # What this is not
//!
//! Not an ExifTool replacement. ExifTool has twenty-five years of vendor-specific
//! accumulation and remains the reference for anything exotic. This crate covers the
//! standard directories — TIFF baseline, Exif, GPS, Interop — plus the DNG/TIFF-EP
//! characterization tags that matter for a colour pipeline, and it says plainly when it
//! does not recognise something.
//!
//! # The rule
//!
//! Anything not in the dictionary is reported with its number, type and count rather
//! than dropped or guessed at. That is the same rule the archival path follows: what we
//! do not understand is passed through intact, never reinterpreted. A metadata tool that
//! quietly omits what it cannot name is worse than one that admits the gap, because you
//! cannot tell the difference between "absent" and "unsupported".

use blad_container::ifd::{self, IfdKind};

pub mod geo;
pub mod icc;
pub mod summary;
pub mod tags;
pub mod value;

pub use blad_container::Error;
pub use tags::Kind;
pub use value::Value;

pub type Result<T> = std::result::Result<T, Error>;

/// How many array elements to show before truncating.
const MAX_ITEMS: usize = 8;

/// `InterColorProfile`.
const TAG_ICC_PROFILE: u16 = 34675;

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Include entries with no dictionary entry.
    pub all: bool,
    /// Suppress GPS, serial numbers and owner names.
    pub redact: bool,
    /// Show values without unit interpretation.
    pub raw: bool,
    /// Restrict to these directories (empty means all).
    pub groups: Vec<String>,
    /// Restrict to tags whose name contains one of these, case-insensitively.
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub tag: u16,
    /// `None` when the tag is not in the dictionary.
    pub name: Option<&'static str>,
    pub kind: Kind,
    pub dtype: u16,
    pub count: u32,
    /// Where the value bytes live in the file.
    pub offset: u64,
    pub value: Value,
    /// Formatted for display, units applied.
    pub display: String,
    /// Set when the value was withheld by `--redact`.
    pub redacted: bool,
}

impl Field {
    /// The name to show: the dictionary's, or a hex tag number.
    pub fn label(&self) -> String {
        match self.name {
            Some(n) => n.to_string(),
            None => format!("Tag(0x{:04X})", self.tag),
        }
    }

    pub fn type_note(&self) -> String {
        format!("{} × {}", value::type_name(self.dtype), self.count)
    }
}

#[derive(Debug, Clone)]
pub struct Group {
    pub kind: IfdKind,
    pub label: String,
    pub offset: u64,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub little_endian: bool,
    pub file_len: u64,
    /// Offset of the TIFF header. Non-zero when the metadata came from a JPEG's APP1.
    pub tiff_base: u64,
    /// The embedded ICC profile, parsed. Present only when the file carries one.
    ///
    /// Kept on the report because nothing in TIFF or Exif distinguishes a BT.2100 PQ
    /// master from an sRGB one — both are 16-bit RGB with identical tags. The answer is
    /// inside the profile, so treating it as an opaque blob loses it.
    pub icc: Option<icc::Profile>,
    /// Size on disk of the archive this was read out of, when it came from one.
    ///
    /// `file_len` is then the length of the *original* file, since that is the
    /// coordinate space the directories describe. Reporting only that would claim a
    /// 293 MB file where 167 MB sits on disk, so both are kept.
    pub archived: Option<u64>,
    pub groups: Vec<Group>,
}

impl Report {
    pub fn field_count(&self) -> usize {
        self.groups.iter().map(|g| g.fields.len()).sum()
    }

    /// Entries present in the file but absent from our dictionary. Reported rather than
    /// hidden, so the size of the gap is visible instead of implied.
    pub fn unknown_count(&self) -> usize {
        self.groups
            .iter()
            .flat_map(|g| &g.fields)
            .filter(|f| f.name.is_none())
            .count()
    }
}

pub fn read(path: &std::path::Path, opts: &Options) -> Result<Report> {
    let dirs = ifd::read(path)?;
    Ok(build(&dirs, opts))
}

/// Read metadata from anything seekable — a file, a JPEG's Exif block, or a blad
/// archive's skeleton.
pub fn read_from<R: std::io::Read + std::io::Seek>(
    src: &mut R,
    len: u64,
    opts: &Options,
) -> Result<Report> {
    let dirs = ifd::read_from(src, len)?;
    Ok(build(&dirs, opts))
}

fn build(dirs: &ifd::Directories, opts: &Options) -> Report {
    let mut groups = Vec::new();

    // Parse the ICC profile from the raw entry: `Value` deliberately reduces large
    // blobs to a byte count, which is right for display and useless here.
    let icc_profile = dirs
        .ifds
        .iter()
        .flat_map(|i| &i.entries)
        .find(|e| e.tag == TAG_ICC_PROFILE && e.unreadable.is_none())
        .and_then(|e| icc::parse(&e.bytes));

    for raw in &dirs.ifds {
        let label = raw.kind.label();
        if !opts.groups.is_empty()
            && !opts
                .groups
                .iter()
                .any(|g| label.to_lowercase().starts_with(&g.to_lowercase()))
        {
            continue;
        }

        let mut fields = Vec::new();
        for e in &raw.entries {
            let tag = tags::lookup(raw.kind, e.tag);
            let name = tag.as_ref().map(|t| t.name);
            let kind = tag.as_ref().map(|t| t.kind).unwrap_or(Kind::Plain);

            if name.is_none() && !opts.all {
                continue;
            }
            if !opts.tags.is_empty() {
                let hay = name.unwrap_or_default().to_lowercase();
                if !opts.tags.iter().any(|t| hay.contains(&t.to_lowercase())) {
                    continue;
                }
            }

            let v = value::decode(e, dirs.little_endian);
            let redacted = opts.redact && kind == Kind::Sensitive;
            let display = if redacted {
                "<redacted>".to_string()
            } else if opts.raw {
                value::render(&v, MAX_ITEMS)
            } else {
                format_value(name.unwrap_or_default(), kind, &v)
            };

            fields.push(Field {
                tag: e.tag,
                name,
                kind,
                dtype: e.dtype,
                count: e.count,
                offset: e.value_offset,
                value: v,
                display,
                redacted,
            });
        }

        if fields.is_empty() {
            continue;
        }
        groups.push(Group {
            kind: raw.kind,
            label,
            offset: raw.offset,
            fields,
        });
    }

    // GPS coordinates are three rationals plus a hemisphere in a neighbouring tag, so
    // they can only be rendered once the whole directory is in hand.
    if !opts.redact && !opts.raw {
        for g in groups.iter_mut().filter(|g| g.kind == IfdKind::Gps) {
            decorate_gps(g);
        }
    }

    Report {
        little_endian: dirs.little_endian,
        file_len: dirs.file_len,
        tiff_base: dirs.tiff_base,
        icc: icc_profile,
        archived: None,
        groups,
    }
}

/// Apply units and enumerations.
fn format_value(name: &str, kind: Kind, v: &Value) -> String {
    // A value we could not read is reported as such whatever the tag claims to be.
    // Checked first because the Opaque arm would otherwise render an unreadable entry
    // as "<opaque, 1 values>", which reads like a successful parse of vendor data and
    // hides the failure — the one thing this crate promises not to do.
    if let Value::Unreadable(why) = v {
        return format!("<unreadable: {why}>");
    }
    match kind {
        // Shutter speeds are read as 1/125 regardless of how the camera stored them —
        // some write 10/1250 for the same exposure.
        Kind::Seconds => match v.as_f64() {
            Some(f) if f > 0.0 && f < 1.0 => format!("1/{:.0} s", 1.0 / f),
            Some(f) => format!("{} s", trim(f)),
            None => value::render(v, MAX_ITEMS),
        },
        Kind::FNumber => match v.as_f64() {
            Some(f) => format!("f/{}", trim(f)),
            None => value::render(v, MAX_ITEMS),
        },
        Kind::Millimetres => match v.as_f64() {
            Some(f) => format!("{} mm", trim(f)),
            None => value::render(v, MAX_ITEMS),
        },
        Kind::Iso => match v.as_u64() {
            Some(n) => format!("ISO {n}"),
            None => value::render(v, MAX_ITEMS),
        },
        Kind::Enum => match v.as_u64() {
            // An unrecognised enumerant shows its number. Inventing a label for a value
            // we do not know is exactly the failure this crate refuses to commit.
            Some(n) => match tags::enum_text(name, n) {
                Some(t) => format!("{t} ({n})"),
                None => n.to_string(),
            },
            None => value::render(v, MAX_ITEMS),
        },
        Kind::Matrix3x3 => value::render(v, 9),
        // Opaque means opaque. IPTC happens to be typed LONG, so without this it
        // decodes into a list of meaningless integers that looks like data.
        Kind::Opaque => match v {
            Value::Binary(n) => format!("<opaque, {}>", value::human_bytes(*n as u64)),
            Value::Text(t) if t.len() <= 32 => t.clone(),
            other => format!("<opaque, {} values>", other.len()),
        },
        Kind::DateTime => match v {
            Value::Text(t) => iso_date(t),
            other => value::render(other, MAX_ITEMS),
        },
        _ => value::render(v, MAX_ITEMS),
    }
}

/// Exif writes `YYYY:MM:DD HH:MM:SS`. The colons in the date half are unique to Exif and
/// sort wrong everywhere else, so they become ISO-8601. Anything that does not match the
/// expected shape is passed through untouched rather than mangled.
fn iso_date(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 19 && b[4] == b':' && b[7] == b':' && b[10] == b' ' {
        format!("{}-{}-{}T{}", &s[0..4], &s[5..7], &s[8..10], &s[11..19])
    } else {
        s.to_string()
    }
}

fn trim(f: f64) -> String {
    format!("{f:.4}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

/// Render GPS latitude/longitude as degrees-minutes-seconds plus a decimal degree.
fn decorate_gps(g: &mut Group) {
    let dms = |v: &Value| -> Option<(f64, String)> {
        let Value::Rational(r) = v else { return None };
        if r.len() < 3 {
            return None;
        }
        let part = |i: usize| {
            let (n, d) = r[i];
            if d == 0 {
                0.0
            } else {
                n as f64 / d as f64
            }
        };
        let (deg, min, sec) = (part(0), part(1), part(2));
        Some((
            deg + min / 60.0 + sec / 3600.0,
            format!("{deg:.0}°{min:.0}'{sec:.2}\""),
        ))
    };

    let refs: Vec<(u16, String)> = g
        .fields
        .iter()
        .filter(|f| matches!(f.tag, 1 | 3))
        .map(|f| (f.tag, f.display.clone()))
        .collect();

    for f in g.fields.iter_mut() {
        let hemi = match f.tag {
            2 => refs.iter().find(|(t, _)| *t == 1).map(|(_, s)| s.clone()),
            4 => refs.iter().find(|(t, _)| *t == 3).map(|(_, s)| s.clone()),
            _ => continue,
        };
        if let Some((dec, text)) = dms(&f.value) {
            let h = hemi.unwrap_or_default();
            let signed = if h == "S" || h == "W" { -dec } else { dec };
            f.display = format!("{text} {h}  ({signed:.6})");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blad_container::ifd::RawEntry;

    fn dirs(kind: IfdKind, entries: Vec<RawEntry>) -> ifd::Directories {
        ifd::Directories {
            little_endian: true,
            file_len: 1024,
            tiff_base: 0,
            ifds: vec![ifd::RawIfd {
                kind,
                offset: 8,
                entries,
            }],
        }
    }

    fn entry(tag: u16, dtype: u16, count: u32, bytes: Vec<u8>) -> RawEntry {
        RawEntry {
            tag,
            dtype,
            count,
            value_offset: 100,
            inline: false,
            bytes,
            unreadable: None,
        }
    }

    fn rational(n: u32, d: u32) -> Vec<u8> {
        let mut b = n.to_le_bytes().to_vec();
        b.extend(d.to_le_bytes());
        b
    }

    #[test]
    fn applies_units() {
        let d = dirs(
            IfdKind::Main(0),
            vec![
                entry(33434, value::RATIONAL, 1, rational(1, 125)),
                entry(33437, value::RATIONAL, 1, rational(28, 10)),
                entry(37386, value::RATIONAL, 1, rational(45, 1)),
            ],
        );
        let r = build(&d, &Options::default());
        let shown: Vec<&str> = r.groups[0]
            .fields
            .iter()
            .map(|f| f.display.as_str())
            .collect();
        assert_eq!(shown, vec!["1/125 s", "f/2.8", "45 mm"]);
    }

    /// Cameras disagree about how to store the same exposure; readers should not.
    #[test]
    fn shutter_speed_normalises_regardless_of_stored_fraction() {
        let d = dirs(
            IfdKind::Main(0),
            vec![entry(33434, value::RATIONAL, 1, rational(10, 1250))],
        );
        let r = build(&d, &Options::default());
        assert_eq!(r.groups[0].fields[0].display, "1/125 s");
    }

    #[test]
    fn enums_show_text_and_number() {
        let d = dirs(
            IfdKind::Main(0),
            vec![entry(262, value::SHORT, 1, 32803u16.to_le_bytes().to_vec())],
        );
        let r = build(&d, &Options::default());
        assert_eq!(r.groups[0].fields[0].display, "CFA (Bayer mosaic) (32803)");
    }

    /// The core promise: an unknown enumerant is shown as a number, never invented.
    #[test]
    fn unknown_enum_shows_the_raw_number() {
        let d = dirs(
            IfdKind::Main(0),
            vec![entry(262, value::SHORT, 1, 4242u16.to_le_bytes().to_vec())],
        );
        let r = build(&d, &Options::default());
        assert_eq!(r.groups[0].fields[0].display, "4242");
    }

    /// Unknown tags are hidden by default but must be *counted*, so the gap is visible.
    #[test]
    fn unknown_tags_appear_only_with_all() {
        let d = dirs(
            IfdKind::Main(0),
            vec![entry(64321, value::SHORT, 1, vec![1, 0])],
        );
        assert_eq!(build(&d, &Options::default()).field_count(), 0);

        let all = Options {
            all: true,
            ..Default::default()
        };
        let r = build(&d, &all);
        assert_eq!(r.unknown_count(), 1);
        assert_eq!(r.groups[0].fields[0].label(), "Tag(0xFB41)");
    }

    #[test]
    fn redaction_hides_sensitive_values_but_keeps_the_row() {
        let d = dirs(
            IfdKind::Exif,
            vec![entry(42033, value::ASCII, 6, b"AB1234".to_vec())],
        );
        let opts = Options {
            redact: true,
            ..Default::default()
        };
        let r = build(&d, &opts);
        let f = &r.groups[0].fields[0];
        assert_eq!(f.display, "<redacted>");
        assert!(f.redacted);
        assert_eq!(f.label(), "BodySerialNumber");
    }

    /// An unreadable value must not be disguised as successfully-read opaque data.
    #[test]
    fn unreadable_beats_opaque_in_display() {
        let mut e = entry(700, value::BYTE, 0xFFFF_FFFF, vec![]);
        e.unreadable = Some("exceeds cap".into());
        let d = dirs(IfdKind::Main(0), vec![e]);
        let r = build(&d, &Options::default());
        let f = &r.groups[0].fields[0];
        assert_eq!(f.label(), "XMP");
        assert!(f.display.starts_with("<unreadable:"), "{}", f.display);
    }

    #[test]
    fn gps_renders_dms_and_decimal() {
        let mut lat = rational(51, 1);
        lat.extend(rational(30, 1));
        lat.extend(rational(0, 1));
        let d = dirs(
            IfdKind::Gps,
            vec![
                entry(1, value::ASCII, 2, b"N\0".to_vec()),
                entry(2, value::RATIONAL, 3, lat),
            ],
        );
        let r = build(&d, &Options::default());
        let s = &r.groups[0].fields[1].display;
        assert!(s.contains("51°30'0.00\""), "{s}");
        assert!(s.contains("51.500000"), "{s}");
    }

    #[test]
    fn tag_filter_matches_by_substring() {
        let d = dirs(
            IfdKind::Main(0),
            vec![
                entry(271, value::ASCII, 4, b"Hass".to_vec()),
                entry(272, value::ASCII, 4, b"X1D\0".to_vec()),
            ],
        );
        let opts = Options {
            tags: vec!["model".into()],
            ..Default::default()
        };
        let r = build(&d, &opts);
        assert_eq!(r.field_count(), 1);
        assert_eq!(r.groups[0].fields[0].label(), "Model");
    }
}
