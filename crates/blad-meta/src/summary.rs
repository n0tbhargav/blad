//! The opinionated view: the dozen facts someone actually wants, named in plain words.
//!
//! `blad exif --full` prints every directory entry under its standard tag name, which is
//! the right output for debugging a file and the wrong one for answering "what is this
//! photo?". Nobody thinks in `ExposureTime` and `FNumber`; they think shutter and
//! aperture.
//!
//! Facets are semantic, not visual. This module decides *what* is worth saying and in
//! what order; glyphs and colour are the caller's business, which keeps the library
//! usable from something that is not a terminal.

use crate::{Report, Value};
use blad_container::ifd::IfdKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facet {
    Camera,
    Lens,
    Shutter,
    Aperture,
    Iso,
    Flash,
    Taken,
    Where,
    Format,
    Image,
    Aspect,
    Depth,
    Dynamic,
    Colour,
    Sensor,
    Orientation,
    Software,
    Author,
    /// How a blad archive was produced. Only present when reading one.
    Archived,
}

impl Facet {
    /// A short plain-language key. Not the tag name.
    pub fn key(&self) -> &'static str {
        match self {
            Facet::Camera => "Camera",
            Facet::Lens => "Lens",
            Facet::Shutter => "Shutter",
            Facet::Aperture => "Aperture",
            Facet::Iso => "ISO",
            Facet::Flash => "Flash",
            Facet::Taken => "Taken",
            Facet::Where => "Where",
            Facet::Format => "Format",
            Facet::Image => "Image",
            Facet::Aspect => "Aspect",
            Facet::Depth => "Depth",
            Facet::Dynamic => "Dynamic",
            Facet::Colour => "Colour",
            Facet::Sensor => "Sensor",
            Facet::Orientation => "Rotation",
            Facet::Software => "Software",
            Facet::Author => "Author",
            Facet::Archived => "Archived",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub facet: Facet,
    /// The value, split into a primary and its qualifiers.
    ///
    /// Split rather than pre-joined so the caller can build a visual hierarchy — the
    /// answer at full strength, the qualifications receding — instead of every part
    /// competing at the same weight with punctuation between them. A library that
    /// returned one string would force the separator choice on every consumer.
    pub parts: Vec<String>,
    /// Personally identifying — the caller may want to colour or withhold it.
    pub sensitive: bool,
}

impl Item {
    /// Plain single-line form, for `--json` and non-terminal consumers.
    pub fn value(&self) -> String {
        self.parts.join(", ")
    }
}

/// Look a tag up by name across every directory.
fn find<'a>(r: &'a Report, name: &str) -> Option<&'a crate::Field> {
    r.groups
        .iter()
        .flat_map(|g| &g.fields)
        .find(|f| f.name == Some(name))
}

/// Prefer a value from the full-resolution SubIFD over IFD0, which on a raw file
/// describes the embedded preview rather than the photograph.
fn find_in<'a>(r: &'a Report, kinds: &[IfdKind], name: &str) -> Option<&'a crate::Field> {
    for k in kinds {
        if let Some(f) = r
            .groups
            .iter()
            .filter(|g| std::mem::discriminant(&g.kind) == std::mem::discriminant(k))
            .flat_map(|g| &g.fields)
            .find(|f| f.name == Some(name))
        {
            return Some(f);
        }
    }
    None
}

fn text(r: &Report, name: &str) -> Option<String> {
    let f = find(r, name)?;
    if f.redacted {
        return Some(f.display.clone());
    }
    let s = f.display.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// `2018-04-09T19:23:29` becomes `Monday 9 April 2018, 19:23`.
///
/// Anything not matching that exact shape is returned unchanged — a date we cannot parse
/// is still the file's own answer, and mangling it would be worse than leaving it.
pub fn human_date(iso: &str) -> String {
    let b = iso.as_bytes();
    if b.len() < 16 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return iso.to_string();
    }
    let num = |a: usize, z: usize| iso[a..z].parse::<i64>().ok();
    let (Some(y), Some(m), Some(d)) = (num(0, 4), num(5, 7), num(8, 10)) else {
        return iso.to_string();
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return iso.to_string();
    }
    // Sakamoto's method for the day of the week.
    const T: [i64; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let yy = if m < 3 { y - 1 } else { y };
    let dow = ((yy + yy / 4 - yy / 100 + yy / 400 + T[(m - 1) as usize] + d) % 7) as usize;

    format!(
        "{} {} {} {}, {}",
        DAYS[dow.min(6)],
        d,
        MONTHS[(m - 1) as usize],
        y,
        &iso[11..16]
    )
}

/// Hemisphere letter as stored, e.g. `N` or `W`.
fn hemisphere(r: &Report, ref_tag: &str) -> Option<String> {
    let h = find(r, ref_tag)?.display.trim().to_uppercase();
    ["N", "S", "E", "W"].contains(&h.as_str()).then_some(h)
}

/// Decimal degrees for a GPS coordinate, honouring the hemisphere reference.
fn coord(r: &Report, value_tag: &str, ref_tag: &str) -> Option<f64> {
    let f = find(r, value_tag)?;
    if f.redacted {
        return None;
    }
    let Value::Rational(parts) = &f.value else {
        return None;
    };
    if parts.len() < 3 {
        return None;
    }
    let at = |i: usize| {
        let (n, d) = parts[i];
        if d == 0 {
            0.0
        } else {
            n as f64 / d as f64
        }
    };
    let dec = at(0) + at(1) / 60.0 + at(2) / 3600.0;
    let hemi = find(r, ref_tag).map(|f| f.display.trim().to_uppercase());
    Some(match hemi.as_deref() {
        Some("S") | Some("W") => -dec,
        _ => dec,
    })
}

/// Reduce a pixel count to the ratio a photographer would name.
///
/// A strict gcd gives honest but useless answers — 8384x6304 reduces to 262:197. Sensors
/// are rarely exact, so we snap to a conventional ratio when within half a percent and
/// otherwise report the decimal rather than inventing precision.
pub fn aspect(w: u64, h: u64) -> String {
    if w == 0 || h == 0 {
        return String::new();
    }
    let (long, short) = if w >= h { (w, h) } else { (h, w) };
    let r = long as f64 / short as f64;
    const COMMON: [(u64, u64); 10] = [
        (1, 1),
        (5, 4),
        (4, 3),
        (7, 5),
        (3, 2),
        (16, 10),
        (5, 3),
        (16, 9),
        (2, 1),
        (21, 9),
    ];
    for (a, b) in COMMON {
        let target = a as f64 / b as f64;
        if (r - target).abs() / target < 0.005 {
            return if w >= h {
                format!("{a}:{b}")
            } else {
                format!("{b}:{a}")
            };
        }
    }
    format!("{r:.2}:1")
}

fn megapixels(w: u64, h: u64) -> String {
    let mp = (w * h) as f64 / 1_000_000.0;
    if mp < 1.0 {
        String::new()
    } else {
        format!("{mp:.1} MP")
    }
}

/// Build the compact view. Facets with nothing to say are simply absent.
pub fn summarise(r: &Report) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    let mut push = |facet: Facet, parts: Vec<String>, sensitive: bool| {
        let parts: Vec<String> = parts
            .into_iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if !parts.is_empty() {
            out.push(Item {
                facet,
                parts,
                sensitive,
            });
        }
    };

    // Make and Model are usually redundant: "Hasselblad" + "Hasselblad X1D".
    let make = text(r, "Make");
    let model = text(r, "Model").or_else(|| text(r, "UniqueCameraModel"));
    let camera = match (&make, &model) {
        (Some(mk), Some(md)) if md.to_lowercase().starts_with(&mk.to_lowercase()) => md.clone(),
        (Some(mk), Some(md)) => format!("{mk} {md}"),
        (None, Some(md)) => md.clone(),
        (Some(mk), None) => mk.clone(),
        _ => String::new(),
    };
    push(Facet::Camera, vec![camera], false);

    let mut lens = Vec::new();
    if let Some(m) = text(r, "LensModel") {
        lens.push(m);
    }
    if let Some(f) = text(r, "FocalLength") {
        lens.push(f);
    }
    if let Some(e) = text(r, "FocalLengthIn35mmFilm") {
        // Parentheses were doing the separating; the separator does it now.
        if Some(&e) != lens.last() {
            lens.push(format!("{e} equivalent"));
        }
    }
    push(Facet::Lens, lens, false);

    // Not every camera writes ExposureTime. APEX ShutterSpeedValue is the older
    // encoding — a log2 value where time = 2^-Tv — and is all some files carry.
    let shutter = text(r, "ExposureTime").or_else(|| {
        let v = find(r, "ShutterSpeedValue")?.value.as_f64()?;
        let secs = 2f64.powf(-v);
        Some(if secs >= 1.0 {
            format!("{secs:.1} s")
        } else {
            format!("1/{:.0} s", 1.0 / secs)
        })
    });
    if let Some(s) = shutter {
        let mode = text(r, "ExposureProgram")
            .and_then(|p| p.split(" (").next().map(str::to_string))
            .filter(|p| p != "not defined");
        push(
            Facet::Shutter,
            vec![s].into_iter().chain(mode).collect(),
            false,
        );
    }
    let aperture = text(r, "FNumber").or_else(|| {
        let v = find(r, "ApertureValue")?.value.as_f64()?;
        Some(format!("f/{:.1}", 2f64.powf(v / 2.0)))
    });
    if let Some(a) = aperture {
        push(Facet::Aperture, vec![a], false);
    }
    if let Some(i) = text(r, "ISOSpeedRatings").or_else(|| text(r, "ISOSpeed")) {
        push(
            Facet::Iso,
            vec![i.trim_start_matches("ISO ").to_string()],
            false,
        );
    }
    if let Some(f) = text(r, "Flash") {
        if !f.starts_with("did not fire") {
            push(
                Facet::Flash,
                vec![f.split(" (").next().unwrap_or(&f).to_string()],
                false,
            );
        }
    }

    if let Some(d) = text(r, "DateTimeOriginal").or_else(|| text(r, "DateTime")) {
        push(Facet::Taken, vec![human_date(&d)], false);
    }

    // Location, with a nearest-city estimate. Never network — see the geo module.
    let redacted_gps = find(r, "GPSLatitude").map(|f| f.redacted).unwrap_or(false);
    if redacted_gps {
        push(Facet::Where, vec!["<redacted>".into()], true);
    } else if let (Some(lat), Some(lon)) = (
        coord(r, "GPSLatitude", "GPSLatitudeRef"),
        coord(r, "GPSLongitude", "GPSLongitudeRef"),
    ) {
        // Hemisphere letters rather than signs. A leading minus is easy to lose and easy
        // to misread; "21.9426 W" cannot be mistaken for anything else. The signed
        // decimals stay in --json for anything doing arithmetic with them.
        let fmt_one = |v: f64, r_tag: &str, pos: &str, neg: &str| {
            let letter =
                hemisphere(r, r_tag)
                    .unwrap_or_else(|| if v < 0.0 { neg.into() } else { pos.into() });
            format!("{:.4}\u{b0} {letter}", v.abs())
        };
        let coords = format!(
            "{}, {}",
            fmt_one(lat, "GPSLatitudeRef", "N", "S"),
            fmt_one(lon, "GPSLongitudeRef", "E", "W")
        );
        let place = crate::geo::nearest(lat, lon, 150.0).map(|n| n.to_string());
        push(
            Facet::Where,
            vec![coords].into_iter().chain(place).collect(),
            true,
        );
    }

    // Prefer the SubIFD: on a raw file, IFD0 describes the embedded preview.
    let pick = |name: &str| find_in(r, &[IfdKind::Sub(0), IfdKind::Main(0)], name);
    let enum_word = |f: Option<&crate::Field>| -> Option<String> {
        f.map(|f| f.display.split(" (").next().unwrap_or("").to_string())
            .filter(|s| !s.is_empty())
    };

    let width = pick("ImageWidth")
        .and_then(|f| f.value.as_u64())
        .or_else(|| find(r, "PixelXDimension").and_then(|f| f.value.as_u64()));
    let height = pick("ImageLength")
        .and_then(|f| f.value.as_u64())
        .or_else(|| find(r, "PixelYDimension").and_then(|f| f.value.as_u64()));
    let bits = pick("BitsPerSample").and_then(|f| f.value.as_u64());
    let photometric = enum_word(pick("PhotometricInterpretation"));
    let compression = enum_word(pick("Compression"));
    let sample_format = pick("SampleFormat").and_then(|f| f.value.as_u64());
    let is_raw = photometric
        .as_deref()
        .map(|p| p.starts_with("CFA") || p.starts_with("linear raw"))
        .unwrap_or(false);

    // Container, from evidence rather than from the file name.
    let mut fmt = Vec::new();
    if r.tiff_base > 0 {
        fmt.push("JPEG".to_string());
    } else if is_raw {
        fmt.push("TIFF/EP raw".to_string());
    } else {
        fmt.push("TIFF".to_string());
    }
    if let Some(c) = &compression {
        // "TIFF/EP raw · JPEG" reads as though the container were JPEG. On a mosaic this
        // is lossless JPEG — the LJ92 predictive codec, nothing like a baseline JPEG —
        // and saying so is the difference between confusing and informative.
        fmt.push(match (c.as_str(), is_raw) {
            ("JPEG", true) => "lossless JPEG (LJ92)".to_string(),
            ("uncompressed", _) => c.clone(),
            (other, _) => format!("{other}-compressed"),
        });
    }
    fmt.push(
        if r.little_endian {
            "little-endian"
        } else {
            "big-endian"
        }
        .to_string(),
    );
    // What you are holding is an archive; what it describes is the original. Showing
    // only the inner format would hide which of the two is on disk.
    if r.archived.is_some() {
        fmt.insert(0, "blad archive".to_string());
    }
    push(Facet::Format, fmt, false);

    if let (Some(w), Some(h)) = (width, height) {
        let mut img = vec![format!("{w} \u{d7} {h}")];
        let mp = megapixels(w, h);
        if !mp.is_empty() {
            img.push(mp);
        }
        push(Facet::Image, img, false);

        let a = aspect(w, h);
        if !a.is_empty() {
            let orient = match h.cmp(&w) {
                std::cmp::Ordering::Greater => Some("portrait".to_string()),
                std::cmp::Ordering::Less => Some("landscape".to_string()),
                std::cmp::Ordering::Equal => None,
            };
            push(
                Facet::Aspect,
                vec![a].into_iter().chain(orient).collect(),
                false,
            );
        }
    }

    if let Some(b) = bits {
        let kind = match sample_format {
            Some(3) => "floating point",
            Some(2) => "signed integer",
            _ => "integer",
        };
        let mut d = vec![format!("{b}-bit {kind}")];
        if let Some(p) = &photometric {
            d.push(p.clone());
        }
        push(Facet::Depth, d, false);

        // Dynamic range, stated from evidence only.
        //
        // The ICC profile outranks everything else here. A BT.2100 PQ master and an
        // sRGB export are both 16-bit RGB with identical TIFF and Exif tags; only the
        // profile's `cicp` tag tells them apart, and ignoring it made blad call a real
        // HDR file "no HDR transfer signalled".
        //
        // No TIFF or Exif tag declares "this is HDR". What the file *does* say is its
        // bit depth, sample format and whether the data is sensor-linear, and those
        // imply the available range. Reporting the inference with its reason is honest;
        // printing a bare "HDR" would not be.
        let from_icc = r.icc.as_ref().and_then(|p| {
            let c = p.cicp?;
            c.is_hdr().then(|| {
                vec![
                    "HDR".to_string(),
                    c.transfer_name().to_string(),
                    c.primaries_name().to_string(),
                ]
            })
        });
        let from_icc = from_icc.or_else(|| {
            let p = r.icc.as_ref()?;
            (p.cicp.is_none() && p.is_hdr()).then(|| {
                vec![
                    "HDR".to_string(),
                    p.description
                        .as_deref()
                        .unwrap_or("declared by ICC profile")
                        .to_string(),
                ]
            })
        });

        let dynamic = from_icc
            .map(Some)
            .unwrap_or_else(|| match (sample_format, b, is_raw) {
                (Some(3), _, _) => Some(vec![
                    "floating point".to_string(),
                    "scene-linear, unbounded".to_string(),
                ]),
                // Deliberately no stop count. Bit depth bounds what the file can *encode*;
                // the sensor's actual dynamic range is a property of the hardware that no
                // tag records, and a 16-bit container does not mean 16 stops were captured.
                (_, b, true) if b >= 12 => Some(vec![
                    "high".to_string(),
                    format!("{b}-bit linear sensor data"),
                ]),
                // No facet at all when there is no positive signal. "No HDR transfer
                // signalled" is a statement about our knowledge, not about the file,
                // and a row that only says "nothing here" is worse than no row.
                (_, b, false) if b >= 16 => None,
                (_, 8, _) => Some(vec![
                    "standard".to_string(),
                    "8-bit, display-referred".to_string(),
                ]),
                _ => None,
            });

        if let Some(d) = dynamic {
            push(Facet::Dynamic, d, false);
        }
    }

    let mut colour = Vec::new();
    if let Some(c) = text(r, "ColorSpace") {
        colour.push(c.split(" (").next().unwrap_or("").to_string());
    }
    match r.icc.as_ref() {
        Some(p) => colour.push(match (&p.description, p.cicp) {
            (Some(d), _) => format!("ICC: {d}"),
            (None, Some(c)) => format!("ICC: {}, {}", c.primaries_name(), c.transfer_name()),
            _ => "ICC profile embedded".into(),
        }),
        None if find(r, "InterColorProfile").is_some() => {
            colour.push("ICC profile embedded (unreadable)".into())
        }
        None => {}
    }
    if find(r, "ColorMatrix1").is_some() {
        colour.push("camera matrix present".into());
    }
    push(Facet::Colour, colour, false);

    // Raw-specific: the numbers that define how the sensor data must be read.
    let mut sensor = Vec::new();
    if let Some(b) = text(r, "BlackLevel") {
        sensor.push(format!("black {b}"));
    }
    if let Some(w) = text(r, "WhiteLevel") {
        sensor.push(format!("white {w}"));
    }
    if let Some(c) = text(r, "DefaultCropSize") {
        sensor.push(format!("crop {}", c.replace(", ", " × ")));
    }
    push(Facet::Sensor, sensor, false);

    if let Some(o) = text(r, "Orientation") {
        let o = o.split(" (").next().unwrap_or("").to_string();
        if o != "upright" {
            push(Facet::Orientation, vec![o], false);
        }
    }
    if let Some(s) = text(r, "Software") {
        push(Facet::Software, vec![s], false);
    }

    if !r.archive_note.is_empty() {
        push(Facet::Archived, r.archive_note.clone(), false);
    }

    let author: Vec<String> = ["Artist", "Copyright", "CameraOwnerName"]
        .iter()
        .filter_map(|t| text(r, t))
        .collect();
    if !author.is_empty() {
        push(Facet::Author, author, true);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gps_report(
        lat: [(i64, i64); 3],
        lat_ref: &str,
        lon: [(i64, i64); 3],
        lon_ref: &str,
    ) -> Report {
        let f = |tag: u16, name: &'static str, v: Value, display: &str| crate::Field {
            tag,
            name: Some(name),
            kind: crate::Kind::Sensitive,
            dtype: 5,
            count: 3,
            offset: 0,
            value: v,
            display: display.to_string(),
            redacted: false,
        };
        Report {
            little_endian: true,
            file_len: 1024,
            tiff_base: 0,
            icc: None,
            archive_note: Vec::new(),
            archived: None,
            groups: vec![crate::Group {
                kind: IfdKind::Gps,
                label: "GPS".into(),
                offset: 8,
                fields: vec![
                    f(1, "GPSLatitudeRef", Value::Text(lat_ref.into()), lat_ref),
                    f(2, "GPSLatitude", Value::Rational(lat.to_vec()), ""),
                    f(3, "GPSLongitudeRef", Value::Text(lon_ref.into()), lon_ref),
                    f(4, "GPSLongitude", Value::Rational(lon.to_vec()), ""),
                ],
            }],
        }
    }

    /// A leading minus is easy to lose and easy to misread; a hemisphere letter is not.
    #[test]
    fn coordinates_use_hemisphere_letters_in_every_quadrant() {
        let cases = [
            ("N", "E", "48.8584\u{b0} N", "2.2945\u{b0} E"),
            ("N", "W", "64.1466\u{b0} N", "21.9426\u{b0} W"),
            ("S", "E", "33.8568\u{b0} S", "151.2153\u{b0} E"),
        ];
        for (lr, lnr, want_lat, want_lon) in cases {
            let (lat, lon) = match (lr, lnr) {
                ("N", "E") => (
                    [(48, 1), (51, 1), (3024, 100)],
                    [(2, 1), (17, 1), (402, 10)],
                ),
                ("N", "W") => (
                    [(64, 1), (8, 1), (4776, 100)],
                    [(21, 1), (56, 1), (3336, 100)],
                ),
                _ => (
                    [(33, 1), (51, 1), (2448, 100)],
                    [(151, 1), (12, 1), (5508, 100)],
                ),
            };
            let r = gps_report(lat, lr, lon, lnr);
            let items = summarise(&r);
            let w = items
                .iter()
                .find(|i| i.facet == Facet::Where)
                .expect("no Where");
            assert!(
                w.value().contains(want_lat),
                "{} lacks {want_lat}",
                w.value()
            );
            assert!(
                w.value().contains(want_lon),
                "{} lacks {want_lon}",
                w.value()
            );
            assert!(
                !w.value().contains('-'),
                "signed value leaked: {}",
                w.value()
            );
        }
    }

    /// Reading an archive must say so: the directories describe the original, but the
    /// file on disk is the archive, and confusing the two misreports the size.
    #[test]
    fn archives_are_named_in_the_format_facet() {
        let mut r = gps_report([(0, 1); 3], "N", [(0, 1); 3], "E");
        r.groups.clear();
        r.archived = Some(1000);
        let items = summarise(&r);
        let f = items
            .iter()
            .find(|i| i.facet == Facet::Format)
            .expect("no Format");
        assert_eq!(f.parts.first().map(String::as_str), Some("blad archive"));
    }

    #[test]
    fn aspect_snaps_to_conventional_ratios() {
        assert_eq!(aspect(8384, 6304), "4:3"); // not the honest-but-useless 262:197
        assert_eq!(aspect(6000, 4000), "3:2");
        assert_eq!(aspect(4000, 6000), "2:3"); // portrait keeps its orientation
        assert_eq!(aspect(1000, 1000), "1:1");
        assert_eq!(aspect(1920, 1080), "16:9");
    }

    /// Anything unconventional gets a decimal rather than an invented ratio.
    #[test]
    fn unusual_ratios_are_reported_as_decimals() {
        assert_eq!(aspect(1000, 337), "2.97:1");
    }

    #[test]
    fn dates_become_human() {
        assert_eq!(
            human_date("2018-04-09T19:23:29"),
            "Monday 9 April 2018, 19:23"
        );
        assert_eq!(
            human_date("2007-11-07T13:43:13"),
            "Wednesday 7 November 2007, 13:43"
        );
    }

    /// A date we cannot parse is still the file's own answer.
    #[test]
    fn unparseable_dates_pass_through() {
        assert_eq!(human_date("not a date"), "not a date");
        assert_eq!(human_date("2018-13-45T99:99"), "2018-13-45T99:99");
    }
}
