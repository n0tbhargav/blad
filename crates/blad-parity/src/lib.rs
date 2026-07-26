//! Reed-Solomon parity for archives at rest.
//!
//! # Why erasures, not errors
//!
//! Reed-Solomon corrects *t* errors of unknown position with `2t` parity symbols, but
//! *t* erasures of known position with only `t`. Locating the damage therefore halves
//! the cost of the same protection, which is a bigger win than any choice of code — so
//! every shard carries a CRC32, and repair is an erasure decode.
//!
//! # Layout, and why it is shaped this way
//!
//! The protected region is cut into stripes of `data_shards` contiguous shards; each
//! stripe gets `parity_shards` recovery shards. Contiguous rather than interleaved is a
//! deliberate trade: interleaving would survive larger bursts but requires either the
//! whole file in memory or a strided second pass, and blad's peak RSS is a number we
//! publish. Stripes let parity be built in a single sequential pass holding
//! `(data + parity) × shard_size` bytes — about 2 MB at the defaults.
//!
//! The consequence is worth stating plainly, because it bounds what this can promise:
//! **any damage confined to `parity_shards` shards of a stripe is repairable, so a
//! contiguous burst up to roughly `parity_shards × shard_size` survives, and scattered
//! sector-sized errors survive as long as no stripe takes more than `parity_shards` of
//! them.** A burst larger than that inside one stripe is not recoverable, and neither is
//! a dead drive — parity protects a copy, it does not replace one.
//!
//! # Self-describing on purpose
//!
//! The header and CRC table are written **twice**, and repair never needs the archive
//! manifest. A parity scheme whose own metadata lives in the structure it protects is
//! circular: the manifest is exactly what you need parity for.

use reed_solomon_erasure::galois_8::ReedSolomon;
use std::io::{Read, Seek, SeekFrom, Write};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("reed-solomon: {0}")]
    Rs(String),
    #[error("parity section is malformed: {0}")]
    Malformed(String),
    #[error("damage exceeds what the parity can repair: {0}")]
    Unrepairable(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub const SECTION_MAGIC: &[u8; 4] = b"PRTY";
pub const SECTION_VERSION: u8 = 1;

/// Fixed part of the section header, before the CRC table.
const HEADER_LEN: usize = 4 + 1 + 1 + 2 + 2 + 4 + 8 + 4;

/// Default shard size. Large enough that a lost disk sector is one shard, small enough
/// that the working set stays a couple of megabytes.
pub const DEFAULT_SHARD_SIZE: usize = 64 * 1024;

/// Default stripe width. With two parity shards this is 6.25% overhead.
pub const DEFAULT_DATA_SHARDS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub data_shards: usize,
    pub parity_shards: usize,
    pub shard_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_shards: DEFAULT_DATA_SHARDS,
            parity_shards: 2,
            shard_size: DEFAULT_SHARD_SIZE,
        }
    }
}

impl Config {
    /// Config approximating a percentage overhead, clamped to what GF(2^8) allows.
    ///
    /// Returns `None` for a percentage so small it would buy no shards — better to
    /// refuse than to write a parity section that cannot repair anything.
    pub fn for_percent(percent: u32) -> Option<Self> {
        if percent == 0 {
            return None;
        }
        let data = DEFAULT_DATA_SHARDS;
        // round up, so "5%" is never silently rounded down to nothing
        let parity = ((data as u32 * percent).div_ceil(100)).max(1) as usize;
        if data + parity > 255 {
            return None;
        }
        Some(Self {
            data_shards: data,
            parity_shards: parity,
            shard_size: DEFAULT_SHARD_SIZE,
        })
    }

    pub fn overhead_percent(&self) -> f64 {
        self.parity_shards as f64 / self.data_shards as f64 * 100.0
    }

    fn validate(&self) -> Result<()> {
        if self.data_shards == 0 || self.parity_shards == 0 {
            return Err(Error::Malformed("zero shards".into()));
        }
        if self.data_shards + self.parity_shards > 255 {
            return Err(Error::Malformed(format!(
                "{} shards exceeds the 255 the field can address",
                self.data_shards + self.parity_shards
            )));
        }
        if self.shard_size == 0 || !self.shard_size.is_multiple_of(64) {
            return Err(Error::Malformed(
                "shard size must be a multiple of 64".into(),
            ));
        }
        Ok(())
    }
}

/// Shard and stripe counts implied by a length and config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub shard_count: usize,
    pub stripes: usize,
    pub parity_bytes: usize,
    pub section_len: usize,
}

pub fn plan(protected_len: u64, cfg: &Config) -> Result<Plan> {
    cfg.validate()?;
    let shard_count = (protected_len as usize).div_ceil(cfg.shard_size).max(1);
    let stripes = shard_count.div_ceil(cfg.data_shards);
    let parity_bytes = stripes * cfg.parity_shards * cfg.shard_size;
    // Header and CRC tables are stored twice; see the module note on circularity.
    // Both data *and* parity shards are checksummed — see `Section::parity_crcs`.
    let meta = HEADER_LEN + (shard_count + stripes * cfg.parity_shards) * 4;
    Ok(Plan {
        shard_count,
        stripes,
        parity_bytes,
        section_len: meta * 2 + parity_bytes,
    })
}

// --- CRC32 (IEEE), table-driven. A dependency for twenty lines would be silly. ---

fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

pub fn crc32(data: &[u8]) -> u32 {
    let t = crc_table();
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn rs(cfg: &Config) -> Result<ReedSolomon> {
    ReedSolomon::new(cfg.data_shards, cfg.parity_shards).map_err(|e| Error::Rs(e.to_string()))
}

/// Build the parity section for `protected_len` bytes read from the start of `src`.
///
/// Streams: memory is one stripe, not the file.
pub fn encode<R: Read + Seek>(src: &mut R, protected_len: u64, cfg: &Config) -> Result<Vec<u8>> {
    let p = plan(protected_len, cfg)?;
    let r = rs(cfg)?;
    src.seek(SeekFrom::Start(0))?;

    let mut crcs: Vec<u32> = Vec::with_capacity(p.shard_count);
    let mut parity_crcs: Vec<u32> = Vec::with_capacity(p.stripes * cfg.parity_shards);
    let mut parity_out: Vec<u8> = Vec::with_capacity(p.parity_bytes);
    let mut remaining = protected_len as usize;

    for _ in 0..p.stripes {
        let mut shards: Vec<Vec<u8>> = Vec::with_capacity(cfg.data_shards + cfg.parity_shards);
        for _ in 0..cfg.data_shards {
            let mut buf = vec![0u8; cfg.shard_size];
            if remaining > 0 {
                let want = remaining.min(cfg.shard_size);
                src.read_exact(&mut buf[..want])?;
                remaining -= want;
                // The tail shard is zero-padded; the CRC covers the padded shard so that
                // verification does not need to special-case the last one.
                crcs.push(crc32(&buf));
            } else if crcs.len() < p.shard_count {
                crcs.push(crc32(&buf));
            }
            shards.push(buf);
        }
        for _ in 0..cfg.parity_shards {
            shards.push(vec![0u8; cfg.shard_size]);
        }
        r.encode(&mut shards)
            .map_err(|e| Error::Rs(e.to_string()))?;
        for s in shards.iter().skip(cfg.data_shards) {
            parity_crcs.push(crc32(s));
            parity_out.extend_from_slice(s);
        }
    }
    crcs.truncate(p.shard_count);

    let mut meta = Vec::with_capacity(HEADER_LEN + (crcs.len() + parity_crcs.len()) * 4);
    meta.extend_from_slice(SECTION_MAGIC);
    meta.push(SECTION_VERSION);
    meta.push(0);
    meta.extend_from_slice(&(cfg.data_shards as u16).to_le_bytes());
    meta.extend_from_slice(&(cfg.parity_shards as u16).to_le_bytes());
    meta.extend_from_slice(&(cfg.shard_size as u32).to_le_bytes());
    meta.extend_from_slice(&protected_len.to_le_bytes());
    meta.extend_from_slice(&(p.shard_count as u32).to_le_bytes());
    for c in crcs.iter().chain(parity_crcs.iter()) {
        meta.extend_from_slice(&c.to_le_bytes());
    }

    let mut out = Vec::with_capacity(p.section_len);
    out.extend_from_slice(&meta);
    out.extend_from_slice(&meta); // second copy
    out.extend_from_slice(&parity_out);
    debug_assert_eq!(out.len(), p.section_len);
    Ok(out)
}

/// Parsed section metadata.
#[derive(Debug, Clone)]
pub struct Section {
    pub cfg: Config,
    pub protected_len: u64,
    pub crcs: Vec<u32>,
    /// One per parity shard, stripe-major.
    ///
    /// Without these a corrupted recovery shard is fed to the decoder as though it were
    /// sound, and reconstruction produces confident rubbish. Checksumming them turns
    /// that into one more erasure, which is what it actually is.
    pub parity_crcs: Vec<u32>,
    /// Offset of the parity shards within the section.
    pub parity_offset: usize,
}

impl Section {
    pub fn stripes(&self) -> usize {
        self.crcs.len().div_ceil(self.cfg.data_shards)
    }
}

/// Parse the section, falling back to the second copy of the metadata if the first is
/// damaged. Two copies exist precisely so that losing one is survivable.
///
/// Layout is `[meta][meta][parity]`, so a copy parsed at offset `at` with length `n`
/// puts the parity shards at `at + n` — which is `2n` for the first copy and, for the
/// second, exactly where it already sits.
pub fn parse_section(section: &[u8]) -> Result<Section> {
    if let Ok((s, n)) = parse_meta(section, 0) {
        return Ok(Section {
            parity_offset: n * 2,
            ..s
        });
    }
    // Find the second copy by its magic rather than by trusting a length we could not
    // read from the damaged first copy.
    let at = section
        .windows(4)
        .enumerate()
        .skip(1)
        .find(|(_, w)| *w == SECTION_MAGIC)
        .map(|(i, _)| i)
        .ok_or_else(|| Error::Malformed("no readable parity header".into()))?;
    let (s, n) = parse_meta(section, at)?;
    Ok(Section {
        parity_offset: at + n,
        ..s
    })
}

fn parse_meta(b: &[u8], at: usize) -> Result<(Section, usize)> {
    let b = b
        .get(at..)
        .ok_or_else(|| Error::Malformed("section truncated".into()))?;
    if b.len() < HEADER_LEN || &b[0..4] != SECTION_MAGIC {
        return Err(Error::Malformed("bad parity magic".into()));
    }
    if b[4] != SECTION_VERSION {
        return Err(Error::Malformed(format!("parity version {}", b[4])));
    }
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as usize;
    let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let cfg = Config {
        data_shards: u16at(6),
        parity_shards: u16at(8),
        shard_size: u32at(10) as usize,
    };
    cfg.validate()?;
    let protected_len = u64::from_le_bytes(b[14..22].try_into().unwrap());
    let shard_count = u32at(22) as usize;

    let want = HEADER_LEN + shard_count * 4;
    if b.len() < want {
        return Err(Error::Malformed("crc table truncated".into()));
    }
    if shard_count != (protected_len as usize).div_ceil(cfg.shard_size).max(1) {
        return Err(Error::Malformed("shard count disagrees with length".into()));
    }
    let stripes = shard_count.div_ceil(cfg.data_shards);
    let parity_count = stripes * cfg.parity_shards;
    let want_all = want + parity_count * 4;
    if b.len() < want_all {
        return Err(Error::Malformed("parity crc table truncated".into()));
    }
    let read = |from: usize, to: usize| -> Vec<u32> {
        b[from..to]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    };
    Ok((
        Section {
            cfg,
            protected_len,
            crcs: read(HEADER_LEN, want),
            parity_crcs: read(want, want_all),
            parity_offset: 0,
        },
        want_all,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scan {
    /// Data shards whose CRC did not match, in file order.
    pub damaged: Vec<usize>,
    /// Parity shards whose CRC did not match, stripe-major.
    pub damaged_parity: Vec<usize>,
    /// Stripes holding more damage than the parity can undo.
    pub unrepairable_stripes: Vec<usize>,
}

impl Scan {
    pub fn is_clean(&self) -> bool {
        self.damaged.is_empty() && self.damaged_parity.is_empty()
    }
    pub fn is_repairable(&self) -> bool {
        self.unrepairable_stripes.is_empty()
    }
}

/// Read every shard and compare it against its stored CRC.
///
/// Never writes. Separated from repair so that a dry run can answer the question that
/// actually matters — *can* this be fixed — rather than merely reporting that something
/// is wrong and leaving the user to find out by trying.
pub fn scan<F: Read + Seek>(file: &mut F, section: &Section, parity: &[u8]) -> Result<Scan> {
    let cfg = &section.cfg;
    let mut out = Scan::default();

    for stripe in 0..section.stripes() {
        let base = stripe * cfg.data_shards;
        let mut bad = 0usize;
        for j in 0..cfg.data_shards {
            let idx = base + j;
            if idx >= section.crcs.len() {
                continue;
            }
            let off = (idx * cfg.shard_size) as u64;
            let want = ((section.protected_len - off) as usize).min(cfg.shard_size);
            let mut buf = vec![0u8; cfg.shard_size];
            file.seek(SeekFrom::Start(off))?;
            let read_ok = file.read_exact(&mut buf[..want]).is_ok();
            if !read_ok || crc32(&buf) != section.crcs[idx] {
                out.damaged.push(idx);
                bad += 1;
            }
        }
        // A damaged recovery shard is a lost shard like any other: it cannot be used to
        // rebuild, and it counts against the same budget.
        let mut bad_parity = 0usize;
        for s in 0..cfg.parity_shards {
            let n = stripe * cfg.parity_shards + s;
            let at = section.parity_offset + n * cfg.shard_size;
            let ok = match (
                parity.get(at..at + cfg.shard_size),
                section.parity_crcs.get(n),
            ) {
                (Some(p), Some(&want)) => crc32(p) == want,
                _ => false,
            };
            if !ok {
                out.damaged_parity.push(n);
                bad_parity += 1;
            }
        }

        // k of the k+m shards must survive, so total losses may not exceed m.
        if bad + bad_parity > cfg.parity_shards {
            out.unrepairable_stripes.push(stripe);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub damaged: Vec<usize>,
    pub damaged_parity: Vec<usize>,
    pub repaired: Vec<usize>,
    pub repaired_parity: Vec<usize>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.damaged.is_empty() && self.damaged_parity.is_empty()
    }

    pub fn total_damaged(&self) -> usize {
        self.damaged.len() + self.damaged_parity.len()
    }
}

/// Scan, and if `repair` is set, rewrite the damaged shards in place.
///
/// Nothing is written until the whole file has been scanned and every stripe is known
/// to be within the parity's capacity, so a partially-repaired archive is not a state
/// this can produce. Each reconstructed shard is checked against its stored CRC before
/// it is written.
pub fn check<F: Read + Seek + Write>(
    file: &mut F,
    section: &Section,
    parity: &[u8],
    repair: bool,
    section_at: Option<u64>,
) -> Result<Report> {
    let cfg = &section.cfg;
    let found = scan(file, section, parity)?;
    let mut report = Report {
        damaged: found.damaged.clone(),
        damaged_parity: found.damaged_parity.clone(),
        repaired: Vec::new(),
        repaired_parity: Vec::new(),
    };
    if found.is_clean() {
        return Ok(report);
    }
    if !found.is_repairable() {
        return Err(Error::Unrepairable(format!(
            "{} stripe(s) lost more than the {} shard(s) parity covers{}",
            found.unrepairable_stripes.len(),
            cfg.parity_shards,
            if found.damaged_parity.is_empty() {
                String::new()
            } else {
                format!(
                    " ({} recovery shard(s) were themselves damaged)",
                    found.damaged_parity.len()
                )
            }
        )));
    }
    if !repair {
        return Ok(report);
    }

    let r = rs(cfg)?;
    let damaged: std::collections::BTreeSet<usize> = found.damaged.iter().copied().collect();

    let damaged_par: std::collections::BTreeSet<usize> =
        found.damaged_parity.iter().copied().collect();

    for stripe in 0..section.stripes() {
        let base = stripe * cfg.data_shards;
        let bad_in_stripe: Vec<usize> = (0..cfg.data_shards)
            .filter(|j| damaged.contains(&(base + j)))
            .collect();
        let bad_parity: Vec<usize> = (0..cfg.parity_shards)
            .filter(|s| damaged_par.contains(&(stripe * cfg.parity_shards + s)))
            .collect();
        if bad_in_stripe.is_empty() && bad_parity.is_empty() {
            continue;
        }

        let mut shards: Vec<Option<Vec<u8>>> =
            Vec::with_capacity(cfg.data_shards + cfg.parity_shards);
        for j in 0..cfg.data_shards {
            let idx = base + j;
            if idx >= section.crcs.len() {
                shards.push(Some(vec![0u8; cfg.shard_size]));
                continue;
            }
            if bad_in_stripe.contains(&j) {
                shards.push(None);
                continue;
            }
            let off = (idx * cfg.shard_size) as u64;
            let want = ((section.protected_len - off) as usize).min(cfg.shard_size);
            let mut buf = vec![0u8; cfg.shard_size];
            file.seek(SeekFrom::Start(off))?;
            file.read_exact(&mut buf[..want])?;
            shards.push(Some(buf));
        }
        for s in 0..cfg.parity_shards {
            let n = stripe * cfg.parity_shards + s;
            let at = section.parity_offset + n * cfg.shard_size;
            // A recovery shard is used only if it proves itself, exactly like a data
            // shard. Trusting one that failed its CRC is how a decode that "succeeds"
            // produces bytes that are wrong.
            let good = match (
                parity.get(at..at + cfg.shard_size),
                section.parity_crcs.get(n),
            ) {
                (Some(p), Some(&want)) if crc32(p) == want => Some(p.to_vec()),
                _ => None,
            };
            shards.push(good);
        }

        r.reconstruct(&mut shards)
            .map_err(|e| Error::Rs(e.to_string()))?;

        for j in bad_in_stripe {
            let idx = base + j;
            let off = (idx * cfg.shard_size) as u64;
            let want = ((section.protected_len - off) as usize).min(cfg.shard_size);
            let data = shards[j].as_ref().expect("reconstructed");
            if crc32(data) != section.crcs[idx] {
                return Err(Error::Unrepairable(format!(
                    "shard {idx} still fails its checksum after reconstruction"
                )));
            }
            file.seek(SeekFrom::Start(off))?;
            file.write_all(&data[..want])?;
            report.repaired.push(idx);
        }

        // Rebuild damaged recovery shards as well. Leaving them broken would let
        // protection erode silently: the data reads fine today, while the margin that
        // would save it next time has quietly gone.
        if let Some(at) = section_at {
            for s in bad_parity {
                let n = stripe * cfg.parity_shards + s;
                let data = shards[cfg.data_shards + s].as_ref().expect("reconstructed");
                if section.parity_crcs.get(n) != Some(&crc32(data)) {
                    continue;
                }
                let off = at + (section.parity_offset + n * cfg.shard_size) as u64;
                file.seek(SeekFrom::Start(off))?;
                file.write_all(data)?;
                report.repaired_parity.push(n);
            }
        }
    }
    file.flush()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn data(n: usize) -> Vec<u8> {
        let mut s = 0x12345678u32;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s >> 24) as u8
            })
            .collect()
    }

    fn cfg() -> Config {
        Config {
            data_shards: 4,
            parity_shards: 2,
            shard_size: 128,
        }
    }

    #[test]
    fn crc_matches_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn plan_accounts_for_padding_and_two_metadata_copies() {
        let c = cfg();
        // 300 bytes over 128-byte shards is 3 shards, one stripe.
        let p = plan(300, &c).unwrap();
        assert_eq!(p.shard_count, 3);
        assert_eq!(p.stripes, 1);
        assert_eq!(p.parity_bytes, 2 * 128);
        // Three data CRCs plus two parity CRCs, and the metadata is stored twice.
        assert_eq!(p.section_len, (HEADER_LEN + (3 + 2) * 4) * 2 + 256);
    }

    #[test]
    fn round_trips_without_damage() {
        let c = cfg();
        let d = data(1000);
        let sec = encode(&mut Cursor::new(&d), d.len() as u64, &c).unwrap();
        let parsed = parse_section(&sec).unwrap();
        assert_eq!(parsed.protected_len, 1000);

        let mut f = Cursor::new(d.clone());
        let rep = check(&mut f, &parsed, &sec, false, None).unwrap();
        assert!(rep.is_clean(), "clean data reported damage: {rep:?}");
    }

    /// The point of the whole exercise: a corrupted shard is put back exactly.
    #[test]
    fn repairs_a_damaged_shard() {
        let c = cfg();
        let d = data(1000);
        let sec = encode(&mut Cursor::new(&d), d.len() as u64, &c).unwrap();
        let parsed = parse_section(&sec).unwrap();

        let mut broken = d.clone();
        broken[200] ^= 0xFF;
        broken[201] ^= 0x0F;

        let mut f = Cursor::new(broken);
        let rep = check(&mut f, &parsed, &sec, true, None).unwrap();
        assert_eq!(rep.damaged, vec![1]);
        assert_eq!(rep.repaired, vec![1]);
        assert_eq!(
            f.into_inner(),
            d,
            "repair did not restore the original bytes"
        );
    }

    /// Two lost shards in a stripe is exactly the limit with two parity shards.
    #[test]
    fn repairs_up_to_the_parity_limit() {
        let c = cfg();
        let d = data(500); // 4 shards, one stripe
        let sec = encode(&mut Cursor::new(&d), d.len() as u64, &c).unwrap();
        let parsed = parse_section(&sec).unwrap();

        let mut broken = d.clone();
        broken[10] ^= 0xFF; // shard 0
        broken[140] ^= 0xFF; // shard 1
        let mut f = Cursor::new(broken);
        let rep = check(&mut f, &parsed, &sec, true, None).unwrap();
        assert_eq!(rep.repaired.len(), 2);
        assert_eq!(f.into_inner(), d);
    }

    /// …and one more than that must fail loudly rather than write plausible rubbish.
    #[test]
    fn refuses_when_damage_exceeds_parity() {
        let c = cfg();
        let d = data(500);
        let sec = encode(&mut Cursor::new(&d), d.len() as u64, &c).unwrap();
        let parsed = parse_section(&sec).unwrap();

        let mut broken = d.clone();
        for off in [10, 140, 270] {
            broken[off] ^= 0xFF;
        }
        let mut f = Cursor::new(broken);
        let e = check(&mut f, &parsed, &sec, true, None).unwrap_err();
        assert!(matches!(e, Error::Unrepairable(_)), "{e:?}");
    }

    /// A burst that would kill a whole stripe if shards were adjacent in one codeword.
    #[test]
    fn survives_a_burst_spanning_two_shards() {
        let c = cfg();
        let d = data(2000);
        let sec = encode(&mut Cursor::new(&d), d.len() as u64, &c).unwrap();
        let parsed = parse_section(&sec).unwrap();

        let mut broken = d.clone();
        // 200 contiguous bytes straddling the boundary between shards 0 and 1.
        for b in broken.iter_mut().take(220).skip(20) {
            *b = 0;
        }
        let mut f = Cursor::new(broken);
        let rep = check(&mut f, &parsed, &sec, true, None).unwrap();
        assert_eq!(rep.repaired, vec![0, 1]);
        assert_eq!(f.into_inner(), d);
    }

    /// A damaged recovery shard must be treated as an erasure, not fed to the decoder.
    /// Trusting it produces a reconstruction that "succeeds" and is wrong — caught here
    /// only because the rebuilt shard then failed its own CRC.
    #[test]
    fn damaged_parity_shards_are_not_trusted() {
        let c = cfg();
        let d = data(500); // one stripe, 4 data + 2 parity
        let mut sec = encode(&mut Cursor::new(&d), d.len() as u64, &c).unwrap();
        let parsed = parse_section(&sec).unwrap();
        assert_eq!(parsed.parity_crcs.len(), 2);

        // Break one data shard and one recovery shard: still within capacity.
        let mut broken = d.clone();
        broken[10] ^= 0xFF;
        sec[parsed.parity_offset + 5] ^= 0xFF;

        let mut f = Cursor::new(broken);
        let rep = check(&mut f, &parsed, &sec, true, None).unwrap();
        assert_eq!(rep.repaired, vec![0]);
        assert_eq!(f.into_inner(), d, "repair was not byte-exact");
    }

    /// …and losing a data shard *and* both recovery shards exceeds capacity, even though
    /// only one data shard is missing.
    #[test]
    fn damaged_parity_counts_against_the_budget() {
        let c = cfg();
        let d = data(500);
        let mut sec = encode(&mut Cursor::new(&d), d.len() as u64, &c).unwrap();
        let parsed = parse_section(&sec).unwrap();

        let mut broken = d.clone();
        broken[10] ^= 0xFF;
        sec[parsed.parity_offset + 5] ^= 0xFF;
        sec[parsed.parity_offset + c.shard_size + 5] ^= 0xFF;

        let mut f = Cursor::new(broken);
        let e = check(&mut f, &parsed, &sec, true, None).unwrap_err();
        assert!(matches!(e, Error::Unrepairable(_)), "{e:?}");
        assert!(e.to_string().contains("recovery shard"), "{e}");
    }

    /// The metadata is stored twice so losing one copy is survivable.
    #[test]
    fn falls_back_to_the_second_metadata_copy() {
        let c = cfg();
        let d = data(600);
        let mut sec = encode(&mut Cursor::new(&d), d.len() as u64, &c).unwrap();
        sec[0] ^= 0xFF; // destroy the first magic
        let parsed = parse_section(&sec).expect("second copy should have been used");
        assert_eq!(parsed.protected_len, 600);
    }

    #[test]
    fn percentage_config_never_rounds_down_to_nothing() {
        assert!(Config::for_percent(0).is_none());
        let c = Config::for_percent(1).unwrap();
        assert!(c.parity_shards >= 1);
        assert!(Config::for_percent(20).unwrap().parity_shards > c.parity_shards);
    }
}
