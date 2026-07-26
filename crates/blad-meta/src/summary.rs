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
        }
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub facet: Facet,
    pub value: String,
    /// Personally identifying — the caller may want to colour or withhold it.
    pub sensitive: bool,
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
    let mut push = |facet: Facet, value: String, sensitive: bool| {
        if !value.trim().is_empty() {
            out.push(Item {
                facet,
                value,
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
    push(Facet::Camera, camera, false);

    let mut lens = Vec::new();
    if let Some(m) = text(r, "LensModel") {
        lens.push(m);
    }
    if let Some(f) = text(r, "FocalLength") {
        lens.push(f);
    }
    if let Some(e) = text(r, "FocalLengthIn35mmFilm") {
        if Some(&e) != lens.last() {
            lens.push(format!("({e} equivalent)"));
        }
    }
    push(Facet::Lens, lens.join("  "), false);

    if let Some(s) = text(r, "ExposureTime") {
        let mode = text(r, "ExposureProgram")
            .and_then(|p| p.split(" (").next().map(str::to_string))
            .filter(|p| p != "not defined");
        push(
            Facet::Shutter,
            match mode {
                Some(m) => format!("{s}  ·  {m}"),
                None => s,
            },
            false,
        );
    }
    if let Some(a) = text(r, "FNumber") {
        push(Facet::Aperture, a, false);
    }
    if let Some(i) = text(r, "ISOSpeedRatings").or_else(|| text(r, "ISOSpeed")) {
        push(Facet::Iso, i.trim_start_matches("ISO ").to_string(), false);
    }
    if let Some(f) = text(r, "Flash") {
        if !f.starts_with("did not fire") {
            push(
                Facet::Flash,
                f.split(" (").next().unwrap_or(&f).to_string(),
                false,
            );
        }
    }

    if let Some(d) = text(r, "DateTimeOriginal").or_else(|| text(r, "DateTime")) {
        push(Facet::Taken, human_date(&d), false);
    }

    // Location, with a nearest-city estimate. Never network — see the geo module.
    let redacted_gps = find(r, "GPSLatitude").map(|f| f.redacted).unwrap_or(false);
    if redacted_gps {
        push(Facet::Where, "<redacted>".into(), true);
    } else if let (Some(lat), Some(lon)) = (
        coord(r, "GPSLatitude", "GPSLatitudeRef"),
        coord(r, "GPSLongitude", "GPSLongitudeRef"),
    ) {
        let mut s = format!("{lat:.4}, {lon:.4}");
        if let Some(n) = crate::geo::nearest(lat, lon, 150.0) {
            s.push_str(&format!("  ·  {n}"));
        }
        push(Facet::Where, s, true);
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
    push(Facet::Format, fmt.join("  \u{b7}  "), false);

    if let (Some(w), Some(h)) = (width, height) {
        let mut img = vec![format!("{w} \u{d7} {h}")];
        let mp = megapixels(w, h);
        if !mp.is_empty() {
            img.push(mp);
        }
        push(Facet::Image, img.join("  \u{b7}  "), false);

        let a = aspect(w, h);
        if !a.is_empty() {
            push(
                Facet::Aspect,
                if h > w {
                    format!("{a}  \u{b7}  portrait")
                } else if w > h {
                    format!("{a}  \u{b7}  landscape")
                } else {
                    a
                },
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
        push(Facet::Depth, d.join("  \u{b7}  "), false);

        // Dynamic range, stated from evidence only.
        //
        // No TIFF or Exif tag declares "this is HDR". What the file *does* say is its
        // bit depth, sample format and whether the data is sensor-linear, and those
        // imply the available range. Reporting the inference with its reason is honest;
        // printing a bare "HDR" would not be.
        let dynamic = match (sample_format, b, is_raw) {
            (Some(3), _, _) => Some("floating point \u{2014} scene-linear, unbounded".to_string()),
            // Deliberately no stop count. Bit depth bounds what the file can *encode*;
            // the sensor's actual dynamic range is a property of the hardware that no
            // tag records, and a 16-bit container does not mean 16 stops were captured.
            (_, b, true) if b >= 12 => Some(format!("high \u{2014} {b}-bit linear sensor data")),
            (_, b, false) if b >= 16 => {
                Some(format!("wide \u{2014} {b}-bit, no HDR transfer signalled"))
            }
            (_, 8, _) => Some("standard \u{2014} 8-bit, display-referred".to_string()),
            _ => None,
        };
        if let Some(d) = dynamic {
            push(Facet::Dynamic, d, false);
        }
    }

    let mut colour = Vec::new();
    if let Some(c) = text(r, "ColorSpace") {
        colour.push(c.split(" (").next().unwrap_or("").to_string());
    }
    if find(r, "InterColorProfile").is_some() {
        colour.push("ICC profile embedded".into());
    }
    if find(r, "ColorMatrix1").is_some() {
        colour.push("camera matrix present".into());
    }
    push(
        Facet::Colour,
        colour
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("  ·  "),
        false,
    );

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
    push(Facet::Sensor, sensor.join("  ·  "), false);

    if let Some(o) = text(r, "Orientation") {
        let o = o.split(" (").next().unwrap_or("").to_string();
        if o != "upright" {
            push(Facet::Orientation, o, false);
        }
    }
    if let Some(s) = text(r, "Software") {
        push(Facet::Software, s, false);
    }

    let author: Vec<String> = ["Artist", "Copyright", "CameraOwnerName"]
        .iter()
        .filter_map(|t| text(r, t))
        .collect();
    if !author.is_empty() {
        push(Facet::Author, author.join("  ·  "), true);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
