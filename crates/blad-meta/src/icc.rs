//! Just enough ICC to answer "what colour is this, and is it HDR?".
//!
//! Not a colour engine — transforms belong to `moxcms`. This reads the profile header,
//! its description, and the **`cicp`** tag added in ICC v4.4, which carries the same
//! colour-primaries / transfer-function / matrix triple that video has signalled for
//! years.
//!
//! `cicp` is the reason this module exists. Nothing in TIFF or Exif says "this file is
//! HDR": a PQ image and an sRGB image are both 16-bit RGB with identical tags. The
//! distinction lives in the embedded profile, so a reader that treats the profile as an
//! opaque blob will confidently call a BT.2100 PQ master "no HDR transfer signalled" —
//! which is what this one did until a real file proved otherwise.
//!
//! It is also the specific gap in the incumbent: lcms2, which nearly every open-source
//! image tool uses, has no concept of CICP, PQ or HLG at all.

/// ICC CICP transfer characteristics, from ITU-T H.273.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cicp {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

impl Cicp {
    /// True for the two transfer functions that mean high dynamic range.
    pub fn is_hdr(&self) -> bool {
        matches!(self.transfer, 16 | 18)
    }

    pub fn transfer_name(&self) -> &'static str {
        match self.transfer {
            1 | 6 | 14 | 15 => "BT.709",
            4 => "gamma 2.2",
            5 => "gamma 2.8",
            8 => "linear",
            13 => "sRGB",
            16 => "PQ (SMPTE ST 2084)",
            18 => "HLG (ARIB STD-B67)",
            _ => "unknown transfer",
        }
    }

    pub fn primaries_name(&self) -> &'static str {
        match self.primaries {
            1 => "BT.709 / sRGB",
            5 => "BT.601 625",
            6 | 7 => "BT.601 525",
            9 => "BT.2020 / BT.2100",
            11 => "DCI-P3",
            12 => "Display P3",
            _ => "unknown primaries",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// The profile's own description, e.g. "Rec. ITU-R BT.2100 PQ".
    pub description: Option<String>,
    pub cicp: Option<Cicp>,
    /// Four-character class code, e.g. `mntr` for a display profile.
    pub class: Option<String>,
    pub version: Option<String>,
    pub size: usize,
}

impl Profile {
    /// HDR according to the profile, where "according to" is the operative phrase: this
    /// reports what the file declares, not what the pixels contain.
    pub fn is_hdr(&self) -> bool {
        self.cicp.map(|c| c.is_hdr()).unwrap_or(false)
            || self
                .description
                .as_deref()
                .map(looks_like_hdr_description)
                .unwrap_or(false)
    }
}

/// Some writers emit a PQ or HLG profile without the `cicp` tag, leaving the description
/// as the only signal. Matching on it is a fallback, and deliberately narrow — these
/// strings are standard names, not free text.
fn looks_like_hdr_description(d: &str) -> bool {
    let d = d.to_ascii_lowercase();
    (d.contains("2100") || d.contains("2084") || d.contains("hlg") || d.contains(" pq"))
        || d.ends_with("pq")
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn sig(b: &[u8], at: usize) -> Option<String> {
    let s = b.get(at..at + 4)?;
    Some(String::from_utf8_lossy(s).trim().to_string())
}

/// Parse the parts we care about. Returns `None` if this is not an ICC profile at all.
pub fn parse(b: &[u8]) -> Option<Profile> {
    // 128-byte header, then a tag table. 'acsp' at offset 36 is the file signature.
    if b.len() < 132 || &b[36..40] != b"acsp" {
        return None;
    }

    let mut p = Profile {
        size: b.len(),
        class: sig(b, 12),
        ..Default::default()
    };
    if let Some(v) = b.get(8..10) {
        p.version = Some(format!("{}.{}", v[0], v[1] >> 4));
    }

    let count = be32(b, 128)? as usize;
    // A hostile profile can claim any tag count; the table has to fit in the buffer.
    if count > 1024 || 132 + count * 12 > b.len() {
        return None;
    }

    for i in 0..count {
        let e = 132 + i * 12;
        let tag = sig(b, e)?;
        let off = be32(b, e + 4)? as usize;
        let len = be32(b, e + 8)? as usize;
        let Some(data) = b.get(off..off.checked_add(len)?) else {
            continue; // A tag pointing outside the profile is skipped, not fatal.
        };

        match tag.as_str() {
            "cicp" if data.len() >= 12 => {
                // Type signature (4) + reserved (4), then the four CICP bytes.
                p.cicp = Some(Cicp {
                    primaries: data[8],
                    transfer: data[9],
                    matrix: data[10],
                    full_range: data[11] != 0,
                });
            }
            "desc" => p.description = read_text(data),
            _ => {}
        }
    }

    Some(p)
}

/// ICC text comes in two flavours: v2 `desc` (ASCII with a length prefix) and v4 `mluc`
/// (UTF-16BE, multiple localised records).
fn read_text(data: &[u8]) -> Option<String> {
    match sig(data, 0)?.as_str() {
        "desc" => {
            let n = be32(data, 8)? as usize;
            let s = data.get(12..12 + n.min(data.len().saturating_sub(12)))?;
            let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
            Some(String::from_utf8_lossy(&s[..end]).trim().to_string()).filter(|s| !s.is_empty())
        }
        "mluc" => {
            let records = be32(data, 8)? as usize;
            if records == 0 {
                return None;
            }
            // First record: language(2) country(2) length(4) offset(4), from byte 16.
            let len = be32(data, 16 + 4)? as usize;
            let off = be32(data, 16 + 8)? as usize;
            let raw = data.get(off..off.checked_add(len)?)?;
            let utf16: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16(&utf16)
                .ok()
                .map(|s| s.trim().trim_end_matches('\0').to_string())
                .filter(|s| !s.is_empty())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal profile: 128-byte header, one tag.
    fn profile(tag: &[u8; 4], body: Vec<u8>) -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[8] = 4; // version 4
        b[9] = 0x40;
        b[12..16].copy_from_slice(b"mntr");
        b[36..40].copy_from_slice(b"acsp");
        let tag_off = 132 + 12;
        b.extend((1u32).to_be_bytes()); // tag count
        b.extend(tag);
        b.extend((tag_off as u32).to_be_bytes());
        b.extend((body.len() as u32).to_be_bytes());
        b.extend(body);
        b
    }

    fn cicp_body(primaries: u8, transfer: u8) -> Vec<u8> {
        let mut v = b"cicp".to_vec();
        v.extend([0, 0, 0, 0]);
        v.extend([primaries, transfer, 0, 1]);
        v
    }

    #[test]
    fn reads_cicp_and_recognises_pq() {
        let p = parse(&profile(b"cicp", cicp_body(9, 16))).unwrap();
        let c = p.cicp.unwrap();
        assert_eq!(c.transfer_name(), "PQ (SMPTE ST 2084)");
        assert_eq!(c.primaries_name(), "BT.2020 / BT.2100");
        assert!(c.is_hdr() && p.is_hdr());
    }

    #[test]
    fn hlg_is_hdr_and_srgb_is_not() {
        assert!(parse(&profile(b"cicp", cicp_body(9, 18))).unwrap().is_hdr());
        assert!(!parse(&profile(b"cicp", cicp_body(1, 13))).unwrap().is_hdr());
    }

    #[test]
    fn reads_a_v4_mluc_description() {
        let text: Vec<u8> = "Display P3"
            .encode_utf16()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        let mut body = b"mluc".to_vec();
        body.extend([0, 0, 0, 0]);
        body.extend((1u32).to_be_bytes()); // one record
        body.extend((12u32).to_be_bytes()); // record size
        body.extend(b"enUS");
        body.extend((text.len() as u32).to_be_bytes());
        body.extend((28u32).to_be_bytes()); // offset within the tag
        body.extend(text);
        let p = parse(&profile(b"desc", body)).unwrap();
        assert_eq!(p.description.as_deref(), Some("Display P3"));
    }

    /// A profile naming BT.2100 but omitting `cicp` must still read as HDR.
    #[test]
    fn description_is_a_fallback_signal() {
        assert!(looks_like_hdr_description("Rec. ITU-R BT.2100 PQ"));
        assert!(looks_like_hdr_description("HLG Display"));
        assert!(!looks_like_hdr_description("sRGB IEC61966-2.1"));
        assert!(!looks_like_hdr_description("Adobe RGB (1998)"));
    }

    #[test]
    fn rejects_non_profiles_and_survives_hostile_ones() {
        assert!(parse(&[0u8; 64]).is_none());
        assert!(parse(&vec![0u8; 400]).is_none()); // no 'acsp'

        // Tag count claiming far more than the buffer holds.
        let mut b = vec![0u8; 132];
        b[36..40].copy_from_slice(b"acsp");
        b[128..132].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse(&b).is_none());
    }

    /// A tag pointing past the end must be skipped, not fatal — the rest of the profile
    /// is still worth reading.
    #[test]
    fn tag_pointing_outside_the_profile_is_skipped() {
        let mut b = vec![0u8; 128];
        b[36..40].copy_from_slice(b"acsp");
        b.extend((1u32).to_be_bytes());
        b.extend(b"cicp");
        b.extend((0xFFFF_0000u32).to_be_bytes());
        b.extend((64u32).to_be_bytes());
        let p = parse(&b).unwrap();
        assert!(p.cicp.is_none());
    }
}
