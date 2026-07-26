//! Byte-exact archival.
//!
//! # Guarantee
//!
//! `restore(archive(f)) == f`, byte for byte, including every header, IFD, ICC profile,
//! Exif/XMP/IPTC field and embedded preview. Not "the pixels match" — the *file* matches.
//! That distinction is why this exists: converting a raw to lossless DNG, or a TIFF to
//! JXL via PNM, preserves image data while discarding the container and its metadata.
//! Neither can give you your original file back.
//!
//! # Scope, honestly
//!
//! blad only recompresses pixel data stored **uncompressed**. Regions that are already
//! compressed — an LZW TIFF, a vendor-compressed raw — are kept verbatim, so such files
//! archive at roughly 1.0. Modelling an existing compressed bitstream well enough to
//! reproduce it byte-for-byte is the Lepton-class problem, solved so far only for JPEG
//! (by libjxl). blad trades that depth for breadth: partial knowledge of any container
//! is still safe, because whatever we do not model is copied.
//!
//! # Verification is not optional
//!
//! [`archive`] reconstructs from what it just wrote and compares SHA-256 against the
//! original *before* reporting success. A claim of byte-exactness is worth nothing if it
//! is not checked on every write.
//!
//! # Format
//!
//! ```text
//! thumbnail (JPEG)         FIRST, so the file *is* a valid JPEG
//! "BLAD" 0x04              magic + format version, after the thumbnail
//! body                     segments in file order; verbatim bytes inline,
//!                          image segments as their encoded parts
//! manifest (JSON, UTF-8)   self-describing on purpose: archives should stay
//!                          readable in twenty years without this source code
//! u32-le manifest_len
//! u32-le thumb_len         locates the body without decoding the JPEG
//! 8 bytes                  SHA-256 prefix over the manifest
//! ```
//!
//! # The thumbnail comes first on purpose
//!
//! JPEG decoders stop at the `FFD9` end marker and ignore trailing bytes, so putting a
//! JPEG at offset 0 makes the whole archive a valid JPEG that happens to carry 56 MB
//! after it. Declare the file type as conforming to `public.jpeg` and macOS renders
//! previews with its own decoder — no Quick Look extension, no Swift, no code signing,
//! nothing for us to maintain. Verified end to end with `qlmanage`.
//!
//! The cost is that `file` reports "JPEG image data" and Preview opens the thumbnail
//! rather than erroring. For a preview-carrying archive that seems a reasonable thing
//! to show.
//!
//! The manifest is a **footer** because blob sizes are only known after encoding.
//! Putting it last lets the body stream straight to the output file instead of being
//! accumulated in memory first. It carries its own digest because a single flipped bit
//! in a few hundred bytes would otherwise make an entire multi-gigabyte archive
//! unreadable, with no way to tell that the payload was fine.

use blad_cfa::Planes;
use blad_codec::{Channels, Codec, Depth, Frame};
use blad_container::{ImageSpec, Layout, PixelLayout, SegmentKind};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const MAGIC: &[u8; 5] = b"BLAD\x04";
/// `u32` manifest length + `u32` thumbnail length + an 8-byte digest over the manifest.
const FOOTER_LEN: u64 = 16;
/// Bytes of SHA-256 kept to detect manifest corruption. Eight is ample: this guards
/// against bit rot, not a forgery attempt.
const MANIFEST_DIGEST_LEN: usize = 8;
const CHUNK: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("container: {0}")]
    Container(#[from] blad_container::Error),
    #[error("cfa: {0}")]
    Cfa(#[from] blad_cfa::Error),
    #[error("codec: {0}")]
    Codec(#[from] blad_codec::Error),
    #[error("manifest: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("not a blad archive")]
    NotAnArchive,
    #[error("archive format version {0} is newer than this build understands (this build writes and reads v{1})")]
    FutureVersion(u8, u8),
    #[error("archive format version {0} predates this build (this build writes and reads v{1}); the format is not yet stable, so old archives must be restored with the version that wrote them")]
    PastVersion(u8, u8),
    #[error("archive is truncated or corrupt: {0}")]
    Corrupt(String),
    /// The failure that must never reach the user silently.
    #[error("VERIFICATION FAILED: reconstruction does not match the original\n  expected sha256 {expected}\n  got      sha256 {actual}")]
    VerificationFailed { expected: String, actual: String },
    #[error("ARCHIVE CORRUPTED: stored bytes no longer match their checksum\n  expected sha256 {expected}\n  got      sha256 {actual}")]
    BodyCorrupted { expected: String, actual: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// How an image segment's samples were handed to the codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// Four CFA sub-planes, quarter size each.
    Cfa4,
    /// Single-channel image, encoded whole.
    Gray,
    /// Interleaved RGB, encoded whole.
    Rgb,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Blob {
    /// Index into `layout.segments`.
    pub segment: usize,
    pub codec: String,
    pub encoding: Encoding,
    /// Byte length of each encoded part, in order.
    pub parts: Vec<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Original {
    pub name: String,
    pub len: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub blad: String,
    pub original: Original,
    /// SHA-256 of the archive body. Lets corruption be detected without decoding
    /// anything — the difference between a scan that can run nightly and one that
    /// cannot.
    pub body_sha256: String,
    pub layout: Layout,
    pub blobs: Vec<Blob>,
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Wraps a writer, hashing and counting everything passing through, so reconstruction
/// can be verified without ever being held in memory.
struct Tally<W: Write> {
    inner: W,
    hasher: Sha256,
    len: u64,
}

impl<W: Write> Tally<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            len: 0,
        }
    }
    fn finish(self) -> (W, u64, String) {
        (self.inner, self.len, hex(&self.hasher.finalize()))
    }
}

impl<W: Write> Write for Tally<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Copy exactly `len` bytes, updating a hasher as they pass.
fn copy_hashed<R: Read, W: Write>(
    src: &mut R,
    dst: &mut W,
    len: u64,
    hasher: &mut Sha256,
) -> Result<()> {
    let mut buf = vec![0u8; CHUNK];
    let mut left = len;
    while left > 0 {
        let want = left.min(CHUNK as u64) as usize;
        src.read_exact(&mut buf[..want])
            .map_err(|_| Error::Corrupt(format!("source ended {left} bytes early")))?;
        hasher.update(&buf[..want]);
        dst.write_all(&buf[..want])?;
        left -= want as u64;
    }
    Ok(())
}

fn depth_of(spec: &ImageSpec) -> Result<Depth> {
    Depth::from_bits(spec.bits_per_sample)
        .ok_or_else(|| Error::Corrupt(format!("unsupported bit depth {}", spec.bits_per_sample)))
}

/// Compress one image segment straight into `out`. Returns the encoding and part sizes.
fn encode_segment(
    codec: &dyn Codec,
    spec: &ImageSpec,
    bytes: Vec<u8>,
    out: &mut dyn Write,
) -> Result<(Encoding, Vec<u64>)> {
    let depth = depth_of(spec)?;
    let le = spec.little_endian;

    let frame = |w: u32, h: u32, channels: Channels| Frame {
        width: w,
        height: h,
        channels,
        depth,
        little_endian: le,
    };

    let whole = |ch: Channels, enc: Encoding, b: &[u8], out: &mut dyn Write| -> Result<_> {
        let n = codec.encode(b, frame(spec.width, spec.height, ch), out)?;
        Ok((enc, vec![n]))
    };

    match spec.layout {
        // CFA splitting is defined on 16-bit samples; 8-bit mosaics fall through to
        // whole encoding, which is correct, merely larger.
        PixelLayout::Cfa
            if spec.bits_per_sample == 16
                && spec.width.is_multiple_of(2)
                && spec.height.is_multiple_of(2) =>
        {
            let planes = blad_cfa::split(&bytes, spec.width, spec.height, 2)?;
            // The mosaic is redundant once split; release it before encoding.
            drop(bytes);
            let mut parts = Vec::with_capacity(blad_cfa::PLANE_COUNT);
            for p in &planes.planes {
                parts.push(codec.encode(
                    p,
                    frame(planes.width, planes.height, Channels::Gray),
                    out,
                )?);
            }
            Ok((Encoding::Cfa4, parts))
        }
        PixelLayout::Cfa => whole(Channels::Gray, Encoding::Gray, &bytes, out),
        PixelLayout::Chunky if spec.samples_per_pixel == 3 => {
            whole(Channels::Rgb, Encoding::Rgb, &bytes, out)
        }
        PixelLayout::Chunky => whole(Channels::Gray, Encoding::Gray, &bytes, out),
    }
}

fn decode_segment(
    codec: &dyn Codec,
    spec: &ImageSpec,
    encoding: Encoding,
    parts: &[Vec<u8>],
    out: &mut dyn Write,
) -> Result<u64> {
    let depth = depth_of(spec)?;
    let le = spec.little_endian;
    let frame = |w: u32, h: u32, channels: Channels| Frame {
        width: w,
        height: h,
        channels,
        depth,
        little_endian: le,
    };
    match encoding {
        Encoding::Cfa4 => {
            if parts.len() != blad_cfa::PLANE_COUNT {
                return Err(Error::Corrupt(format!(
                    "cfa4 segment has {} parts, expected {}",
                    parts.len(),
                    blad_cfa::PLANE_COUNT
                )));
            }
            let (hw, hh) = (spec.width / 2, spec.height / 2);
            let mut planes: [Vec<u8>; blad_cfa::PLANE_COUNT] = Default::default();
            for (i, part) in parts.iter().enumerate() {
                let mut buf = Vec::with_capacity((hw as usize) * (hh as usize) * 2);
                codec.decode(part, frame(hw, hh, Channels::Gray), &mut buf)?;
                planes[i] = buf;
            }
            let mosaic = blad_cfa::merge(&Planes {
                width: hw,
                height: hh,
                bytes_per_sample: 2,
                planes,
            });
            out.write_all(&mosaic)?;
            Ok(mosaic.len() as u64)
        }
        Encoding::Gray => Ok(codec.decode(
            &parts[0],
            frame(spec.width, spec.height, Channels::Gray),
            out,
        )?),
        Encoding::Rgb => Ok(codec.decode(
            &parts[0],
            frame(spec.width, spec.height, Channels::Rgb),
            out,
        )?),
    }
}

/// Where the time and memory went, per phase. Recorded on every run, not behind a
/// feature flag: this project makes claims about speed, size, and memory, and claims
/// need to stay measured as the code changes.
///
/// `rss_after` is monotonic across phases, so comparing consecutive values shows which
/// phase actually drove peak memory. Comparing `heap_peak` against it shows how much is
/// our data structures versus everything else (temp-file pages, allocator overhead).
#[derive(Debug, Clone, Copy, Default)]
pub struct Timings {
    /// Parsing the container and building the layout.
    pub analyze: blad_mem::Phase,
    /// Reading segments, encoding, and writing the archive.
    pub encode: blad_mem::Phase,
    /// Reconstructing from disk to prove the archive is good.
    pub verify: blad_mem::Phase,
    pub total: std::time::Duration,
}

/// Pick a segment to build the thumbnail from.
///
/// Prefers the *smallest* RGB segment, which is normally the camera's own embedded
/// preview — already demosaiced and colour-rendered, so it looks like the photographer's
/// intent rather than raw sensor response. A CFA segment cannot be used: turning a
/// mosaic into a viewable image needs demosaicing, which belongs to the pipeline, not
/// here.
fn thumb_source(layout: &Layout) -> Option<(&blad_container::Segment, &ImageSpec)> {
    layout
        .image_segments()
        .filter(|(_, _, spec)| spec.layout == PixelLayout::Chunky && spec.samples_per_pixel == 3)
        .min_by_key(|(_, seg, _)| seg.len)
        .map(|(_, seg, spec)| (seg, spec))
}

/// Build a thumbnail by reading one segment out of the source file.
///
/// Returns an empty vector when there is nothing usable — a raw with no embedded preview,
/// say. A missing thumbnail is a cosmetic loss and must never fail an archive.
fn make_thumbnail(src: &Path, layout: &Layout) -> Vec<u8> {
    let Some((seg, spec)) = thumb_source(layout) else {
        return Vec::new();
    };
    let bps = usize::from(spec.bits_per_sample / 8);
    let read = || -> std::io::Result<Vec<u8>> {
        let mut f = std::fs::File::open(src)?;
        f.seek(SeekFrom::Start(seg.src_offset))?;
        let mut buf = vec![0u8; seg.len as usize];
        f.read_exact(&mut buf)?;
        Ok(buf)
    };
    let Ok(bytes) = read() else {
        return Vec::new();
    };
    blad_thumb::thumbnail(
        &bytes,
        spec.width,
        spec.height,
        bps,
        spec.little_endian,
        layout.orientation,
        blad_thumb::MAX_EDGE,
    )
    .unwrap_or_default()
}

/// Read just the embedded thumbnail.
///
/// The bytes sit at offset 0 and form a complete JPEG, so anything that can decode a
/// JPEG can show a preview of a blad archive without knowing this format exists. This
/// helper exists for our own CLI; the operating system needs nothing from us.
pub fn thumbnail(archive_path: &Path) -> Result<Vec<u8>> {
    let (_, thumb_len) = read_footer(archive_path)?;
    if thumb_len == 0 {
        return Ok(Vec::new());
    }
    let mut f = std::fs::File::open(archive_path)?;
    let mut buf = vec![0u8; thumb_len as usize];
    f.read_exact(&mut buf)
        .map_err(|_| Error::Corrupt("thumbnail truncated".into()))?;
    Ok(buf)
}

/// Parse the trailing footer. Returns `(manifest_len, thumb_len)`.
fn read_footer(path: &Path) -> Result<(u64, u64)> {
    let mut f = std::fs::File::open(path)?;
    let total = f.metadata()?.len();
    if total < MAGIC.len() as u64 + FOOTER_LEN {
        return Err(Error::NotAnArchive);
    }
    f.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
    let mut footer = [0u8; FOOTER_LEN as usize];
    f.read_exact(&mut footer)?;
    let manifest_len = u64::from(u32::from_le_bytes([
        footer[0], footer[1], footer[2], footer[3],
    ]));
    let thumb_len = u64::from(u32::from_le_bytes([
        footer[4], footer[5], footer[6], footer[7],
    ]));
    if thumb_len + MAGIC.len() as u64 + manifest_len + FOOTER_LEN > total {
        return Err(Error::NotAnArchive);
    }
    Ok((manifest_len, thumb_len))
}

/// Result of a successful [`archive`].
#[derive(Debug, Clone)]
pub struct ArchiveReport {
    pub original_len: u64,
    pub stored_len: u64,
    pub skeleton_len: u64,
    pub payload_len: u64,
    pub sha256: String,
    pub verified: bool,
    pub timings: Timings,
}

impl ArchiveReport {
    /// Stored size as a fraction of the original. Lower is better.
    pub fn ratio(&self) -> f64 {
        if self.original_len == 0 {
            return 1.0;
        }
        self.stored_len as f64 / self.original_len as f64
    }
}

/// Predict what [`archive`] would do, without encoding anything.
///
/// Answers the only question worth asking before spending minutes on a large file:
/// is any of it compressible at all?
pub fn plan(src: &Path) -> Result<Layout> {
    let layout = blad_container::analyze(src)?;
    layout.validate()?;
    Ok(layout)
}

/// Archive `src` to `dst`, then verify by reconstructing and comparing hashes.
pub fn archive(src: &Path, dst: &Path, codec: &dyn Codec) -> Result<ArchiveReport> {
    let t0 = std::time::Instant::now();
    let (layout, ph_analyze) = blad_mem::measure(|| plan(src));
    let layout = layout?;

    blad_mem::reset_heap_peak();
    let t_encode = std::time::Instant::now();
    let mut input = std::io::BufReader::with_capacity(CHUNK, std::fs::File::open(src)?);
    let out = std::io::BufWriter::with_capacity(CHUNK, std::fs::File::create(dst)?);
    let mut body = Tally::new(out);
    let mut original = Sha256::new();
    let mut blobs = Vec::new();

    // Written through `body.inner` rather than `body`, so the body checksum covers the
    // segments only and stays independent of the thumbnail.
    //
    // The JPEG goes first so the file is a valid JPEG; the magic follows it.
    let thumb = make_thumbnail(src, &layout);
    {
        let inner = &mut body.inner;
        inner.write_all(&thumb)?;
        inner.write_all(MAGIC)?;
    }

    for (i, seg) in layout.segments.iter().enumerate() {
        match &seg.kind {
            SegmentKind::Verbatim => {
                copy_hashed(&mut input, &mut body, seg.len, &mut original)?;
            }
            SegmentKind::Image(spec) => {
                // One image segment resident at a time, never the whole file.
                let mut bytes = vec![0u8; seg.len as usize];
                input
                    .read_exact(&mut bytes)
                    .map_err(|_| Error::Corrupt(format!("segment {i} truncated in source")))?;
                original.update(&bytes);
                let (encoding, parts) = encode_segment(codec, spec, bytes, &mut body)?;
                blobs.push(Blob {
                    segment: i,
                    codec: codec.id().to_string(),
                    encoding,
                    parts,
                });
            }
        }
    }

    let (mut out, _body_len, body_sha256) = body.finish();
    let digest = hex(&original.finalize());

    let manifest = Manifest {
        blad: env!("CARGO_PKG_VERSION").to_string(),
        original: Original {
            name: src
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            len: layout.total_len,
            sha256: digest.clone(),
        },
        body_sha256,
        layout,
        blobs,
    };

    let json = serde_json::to_vec(&manifest)?;
    out.write_all(&json)?;
    out.write_all(&(json.len() as u32).to_le_bytes())?;
    out.write_all(&(thumb.len() as u32).to_le_bytes())?;
    out.write_all(&Sha256::digest(&json)[..MANIFEST_DIGEST_LEN])?;
    out.flush()?;
    drop(out);
    let ph_encode = blad_mem::Phase {
        time: t_encode.elapsed(),
        heap_peak: blad_mem::heap_peak(),
        rss_after: blad_mem::rss_highwater(),
    };

    // Verify from what is actually on disk, not from memory — this must catch a bad
    // write, not merely a bad encode.
    let (checked, ph_verify) = blad_mem::measure(|| reconstruct(dst, codec, std::io::sink()));
    let (len, actual) = checked?;
    if len != manifest.original.len || actual != digest {
        let _ = std::fs::remove_file(dst);
        return Err(Error::VerificationFailed {
            expected: digest,
            actual,
        });
    }

    Ok(ArchiveReport {
        original_len: manifest.original.len,
        stored_len: std::fs::metadata(dst)?.len(),
        skeleton_len: manifest.layout.skeleton_len(),
        payload_len: manifest.layout.payload_len(),
        sha256: digest,
        verified: true,
        timings: Timings {
            analyze: ph_analyze,
            encode: ph_encode,
            verify: ph_verify,
            total: t0.elapsed(),
        },
    })
}

/// Read an archive's footer manifest without touching pixel data.
/// Returns the manifest and the body's `(offset, length)`.
pub fn read_manifest(path: &Path) -> Result<(Manifest, u64, u64)> {
    let (json_len, thumb_len) = read_footer(path)?;
    let mut f = std::fs::File::open(path)?;
    let total = f.metadata()?.len();

    // The magic follows the thumbnail rather than opening the file, so that offset 0 can
    // hold a JPEG the operating system understands.
    f.seek(SeekFrom::Start(thumb_len))?;
    let mut magic = [0u8; 5];
    f.read_exact(&mut magic).map_err(|_| Error::NotAnArchive)?;
    if magic[0..4] != MAGIC[0..4] {
        return Err(Error::NotAnArchive);
    }
    // Exact match, not "<= current". The format is pre-1.0 and has changed four times;
    // silently accepting an older version would parse it with the wrong offsets, which
    // looks like corruption rather than like the version mismatch it is. Refusing with
    // the actual numbers is the only answer that tells the user what to do next.
    match magic[4].cmp(&MAGIC[4]) {
        std::cmp::Ordering::Greater => return Err(Error::FutureVersion(magic[4], MAGIC[4])),
        std::cmp::Ordering::Less => return Err(Error::PastVersion(magic[4], MAGIC[4])),
        std::cmp::Ordering::Equal => {}
    }
    let body_off = thumb_len + MAGIC.len() as u64;

    let json_off = total
        .checked_sub(FOOTER_LEN + json_len)
        .ok_or_else(|| Error::Corrupt("manifest length exceeds file size".into()))?;
    if json_off < body_off {
        return Err(Error::Corrupt("manifest overlaps body".into()));
    }

    f.seek(SeekFrom::Start(json_off))?;
    let mut json = vec![0u8; json_len as usize];
    f.read_exact(&mut json)
        .map_err(|_| Error::Corrupt("manifest truncated".into()))?;

    // Check the manifest before parsing it. Serde would reject scrambled JSON anyway,
    // but a flipped bit inside a *number* stays valid JSON and would silently give
    // wrong offsets — the failure that looks like a codec bug and is not.
    f.seek(SeekFrom::End(-(FOOTER_LEN as i64) + 8))?;
    let mut want = [0u8; MANIFEST_DIGEST_LEN];
    f.read_exact(&mut want)?;
    if Sha256::digest(&json)[..MANIFEST_DIGEST_LEN] != want {
        return Err(Error::Corrupt(
            "manifest failed its own checksum; the archive index is damaged".into(),
        ));
    }

    let manifest: Manifest = serde_json::from_slice(&json)?;
    manifest.layout.validate()?;

    Ok((manifest, body_off, json_off - body_off))
}

/// Stream the original file out of an archive, returning its length and SHA-256.
fn reconstruct<W: Write>(archive_path: &Path, codec: &dyn Codec, out: W) -> Result<(u64, String)> {
    let (manifest, body_off, _) = read_manifest(archive_path)?;
    let f = std::fs::File::open(archive_path)?;
    let mut reader = std::io::BufReader::with_capacity(CHUNK, f);
    reader.seek(SeekFrom::Start(body_off))?;

    let mut sink = Tally::new(out);
    let mut discard = Sha256::new();

    for (i, seg) in manifest.layout.segments.iter().enumerate() {
        match &seg.kind {
            SegmentKind::Verbatim => {
                copy_hashed(&mut reader, &mut sink, seg.len, &mut discard)?;
            }
            SegmentKind::Image(spec) => {
                let blob = manifest
                    .blobs
                    .iter()
                    .find(|b| b.segment == i)
                    .ok_or_else(|| Error::Corrupt(format!("no blob for image segment {i}")))?;
                let mut parts = Vec::with_capacity(blob.parts.len());
                for len in &blob.parts {
                    let mut p = vec![0u8; *len as usize];
                    reader
                        .read_exact(&mut p)
                        .map_err(|_| Error::Corrupt(format!("blob for segment {i} truncated")))?;
                    parts.push(p);
                }
                let n = decode_segment(codec, spec, blob.encoding, &parts, &mut sink)?;
                if n != seg.len {
                    return Err(Error::Corrupt(format!(
                        "segment {i} decoded to {n} bytes, expected {}",
                        seg.len
                    )));
                }
            }
        }
    }
    sink.flush()?;
    let (_, len, digest) = sink.finish();
    Ok((len, digest))
}

fn check(manifest: &Manifest, len: u64, actual: String) -> Result<()> {
    if len != manifest.original.len {
        return Err(Error::Corrupt(format!(
            "reconstructed {len} bytes, manifest says {}",
            manifest.original.len
        )));
    }
    if actual != manifest.original.sha256 {
        return Err(Error::VerificationFailed {
            expected: manifest.original.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

/// Reconstruct the original file at `dst`, refusing to leave it in place unless the
/// hash matches.
///
/// Writes to a sibling `.part` file and renames only after verification, so a failure or
/// a crash can never leave something that looks like a restored original.
pub fn restore(archive_path: &Path, dst: &Path, codec: &dyn Codec) -> Result<()> {
    let (manifest, _, _) = read_manifest(archive_path)?;

    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = std::path::PathBuf::from(tmp);

    let result = (|| -> Result<()> {
        let f = std::io::BufWriter::with_capacity(CHUNK, std::fs::File::create(&tmp)?);
        let (len, actual) = reconstruct(archive_path, codec, f)?;
        check(&manifest, len, actual)
    })();

    match result {
        Ok(()) => {
            std::fs::rename(&tmp, dst)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Full verification: reconstruct everything and compare against the recorded hash.
/// Catches encoder and format bugs as well as corruption. Slow.
pub fn verify(archive_path: &Path, codec: &dyn Codec) -> Result<Manifest> {
    let (manifest, _, _) = read_manifest(archive_path)?;
    let (len, actual) = reconstruct(archive_path, codec, std::io::sink())?;
    check(&manifest, len, actual)?;
    Ok(manifest)
}

/// Fast verification: checksum the stored bytes without decoding.
///
/// Catches bit rot and media failure — the common way an archive dies — at I/O speed,
/// so it can run on a schedule across a whole library. It cannot catch a codec bug,
/// which is what the full [`verify`] is for.
pub fn verify_quick(archive_path: &Path) -> Result<Manifest> {
    let (manifest, body_off, body_len) = read_manifest(archive_path)?;
    let f = std::fs::File::open(archive_path)?;
    let mut reader = std::io::BufReader::with_capacity(CHUNK, f);
    reader.seek(SeekFrom::Start(body_off))?;

    let mut hasher = Sha256::new();
    copy_hashed(&mut reader, &mut std::io::sink(), body_len, &mut hasher)?;
    let actual = hex(&hasher.finalize());

    if actual != manifest.body_sha256 {
        return Err(Error::BodyCorrupted {
            expected: manifest.body_sha256,
            actual,
        });
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A codec that stores samples verbatim; lets us test archive logic without libjxl.
    struct Identity;

    impl Codec for Identity {
        fn id(&self) -> &'static str {
            "identity"
        }
        fn encode(&self, src: &[u8], _f: Frame, out: &mut dyn Write) -> blad_codec::Result<u64> {
            out.write_all(src)?;
            Ok(src.len() as u64)
        }
        fn decode(&self, data: &[u8], _f: Frame, out: &mut dyn Write) -> blad_codec::Result<u64> {
            out.write_all(data)?;
            Ok(data.len() as u64)
        }
    }

    fn synth_tiff(width: u32, height: u32, samples: u16, photometric: u16, seed: u8) -> Vec<u8> {
        let px = (width * height * u32::from(samples) * 2) as usize;
        let n = 8usize;
        let ifd_off = 8u32;
        let data_off = ifd_off + 2 + (n as u32) * 12 + 4;
        let mut all: Vec<(u16, u16, u32, u32)> = vec![
            (256, 4, 1, width),
            (257, 4, 1, height),
            (258, 3, 1, 16),
            (259, 3, 1, 1),
            (262, 3, 1, u32::from(photometric)),
            (277, 3, 1, u32::from(samples)),
            (279, 4, 1, px as u32),
            (273, 4, 1, data_off),
        ];
        all.sort_by_key(|e| e.0);

        let mut b = Vec::new();
        b.extend_from_slice(b"II");
        b.extend_from_slice(&42u16.to_le_bytes());
        b.extend_from_slice(&ifd_off.to_le_bytes());
        b.extend_from_slice(&(n as u16).to_le_bytes());
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
        let mut s = u64::from(seed) | 1;
        b.extend((0..px).map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 24) as u8
        }));
        b.extend_from_slice(b"trailing-metadata");
        b
    }

    /// Unique per call. A timestamp alone is not enough: tests run in parallel threads
    /// and the clock is coarse enough that two can collide, after which one test's
    /// cleanup deletes another's files mid-run.
    fn tmp() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "blad-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn round_trip(width: u32, height: u32, samples: u16, photometric: u16, seed: u8) {
        let dir = tmp();
        let src = dir.join("a.src");
        let arc = dir.join("a.blad");
        let out = dir.join("a.out");
        let bytes = synth_tiff(width, height, samples, photometric, seed);
        std::fs::write(&src, &bytes).unwrap();

        let rep = archive(&src, &arc, &Identity).unwrap();
        assert!(rep.verified);
        assert_eq!(rep.original_len, bytes.len() as u64);
        restore(&arc, &out, &Identity).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), bytes);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cfa_round_trips_byte_exactly() {
        round_trip(1024, 1024, 1, 32803, 3);
    }

    #[test]
    fn rgb_round_trips_byte_exactly() {
        round_trip(512, 700, 3, 2, 9);
    }

    #[test]
    fn odd_dimension_cfa_falls_back_and_still_round_trips() {
        // 1025 is odd: no clean 2x2 split, must encode whole rather than fail.
        round_trip(1025, 1024, 1, 32803, 21);
    }

    #[test]
    fn trailing_and_leading_bytes_survive() {
        let dir = tmp();
        let src = dir.join("a.3fr");
        let arc = dir.join("a.blad");
        std::fs::write(&src, synth_tiff(1024, 1024, 1, 32803, 5)).unwrap();
        archive(&src, &arc, &Identity).unwrap();

        let (m, _, _) = read_manifest(&arc).unwrap();
        assert!(m.layout.skeleton_len() > 0);
        assert_eq!(m.layout.payload_len(), 1024 * 1024 * 2);
        assert_eq!(m.blobs[0].encoding, Encoding::Cfa4);
        assert_eq!(m.blobs[0].parts.len(), 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn quick_verify_accepts_intact_archive() {
        let dir = tmp();
        let src = dir.join("a.3fr");
        let arc = dir.join("a.blad");
        std::fs::write(&src, synth_tiff(1024, 1024, 1, 32803, 7)).unwrap();
        archive(&src, &arc, &Identity).unwrap();
        verify_quick(&arc).unwrap();
        verify(&arc, &Identity).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn both_verifiers_detect_a_flipped_bit() {
        let dir = tmp();
        let src = dir.join("a.3fr");
        let arc = dir.join("a.blad");
        std::fs::write(&src, synth_tiff(1024, 1024, 1, 32803, 11)).unwrap();
        archive(&src, &arc, &Identity).unwrap();

        let mut data = std::fs::read(&arc).unwrap();
        data[1000] ^= 0xFF; // inside the body
        std::fs::write(&arc, &data).unwrap();

        assert!(matches!(
            verify_quick(&arc),
            Err(Error::BodyCorrupted { .. })
        ));
        assert!(matches!(
            verify(&arc, &Identity),
            Err(Error::VerificationFailed { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_leaves_nothing_behind_on_failure() {
        let dir = tmp();
        let src = dir.join("a.3fr");
        let arc = dir.join("a.blad");
        let out = dir.join("a.out");
        std::fs::write(&src, synth_tiff(1024, 1024, 1, 32803, 17)).unwrap();
        archive(&src, &arc, &Identity).unwrap();

        let mut data = std::fs::read(&arc).unwrap();
        data[2000] ^= 0xFF;
        std::fs::write(&arc, &data).unwrap();

        assert!(restore(&arc, &out, &Identity).is_err());
        assert!(!out.exists(), "a failed restore must not leave a file");
        let part = std::path::PathBuf::from({
            let mut s = out.as_os_str().to_owned();
            s.push(".part");
            s
        });
        assert!(!part.exists(), "temp file must be cleaned up");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rgb_source_gets_a_thumbnail() {
        let dir = tmp();
        let src = dir.join("a.tif");
        let arc = dir.join("a.blad");
        std::fs::write(&src, synth_tiff(512, 700, 3, 2, 23)).unwrap();
        archive(&src, &arc, &Identity).unwrap();

        let t = thumbnail(&arc).unwrap();
        assert!(!t.is_empty(), "an RGB segment should yield a thumbnail");
        assert_eq!(&t[0..2], &[0xFF, 0xD8], "JPEG SOI");
        assert!(
            t.len() < 200_000,
            "thumbnail should be small, got {}",
            t.len()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The property the macOS integration depends on: an archive with a thumbnail is
    /// itself a valid JPEG, so the operating system can render a preview using its own
    /// decoder. Break this and Finder silently stops showing previews.
    #[test]
    fn an_archive_with_a_thumbnail_is_a_valid_jpeg() {
        let dir = tmp();
        let src = dir.join("a.tif");
        let arc = dir.join("a.blad");
        std::fs::write(&src, synth_tiff(512, 700, 3, 2, 41)).unwrap();
        archive(&src, &arc, &Identity).unwrap();

        let data = std::fs::read(&arc).unwrap();
        assert_eq!(
            &data[0..2],
            &[0xFF, 0xD8],
            "file must open with a JPEG SOI marker"
        );

        // The JPEG must terminate before our magic, or a decoder would run into it.
        let thumb = thumbnail(&arc).unwrap();
        assert_eq!(&thumb[thumb.len() - 2..], &[0xFF, 0xD9], "JPEG EOI");
        assert_eq!(
            &data[thumb.len()..thumb.len() + 5],
            MAGIC,
            "magic must follow the thumbnail"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Version mismatches must be reported as version mismatches. Guessing at another
    /// version's layout produces wrong offsets, which surfaces as "corrupt" and sends
    /// the user looking for a bug that is not there.
    #[test]
    fn foreign_format_versions_are_refused_by_version_not_by_corruption() {
        let dir = tmp();
        let src = dir.join("a.tif");
        let arc = dir.join("a.blad");
        std::fs::write(&src, synth_tiff(64, 64, 3, 2, 31)).unwrap();
        archive(&src, &arc, &Identity).unwrap();

        let good = std::fs::read(&arc).unwrap();
        let off = good
            .windows(5)
            .position(|w| w == MAGIC)
            .expect("magic present");

        for (v, want_future) in [(MAGIC[4] + 1, true), (MAGIC[4] - 1, false)] {
            let mut bad = good.clone();
            bad[off + 4] = v;
            std::fs::write(&arc, &bad).unwrap();
            let e = verify_quick(&arc).unwrap_err();
            match (&e, want_future) {
                (Error::FutureVersion(got, cur), true) => {
                    assert_eq!((*got, *cur), (v, MAGIC[4]))
                }
                (Error::PastVersion(got, cur), false) => {
                    assert_eq!((*got, *cur), (v, MAGIC[4]))
                }
                _ => panic!("v{v} gave {e:?}, expected a version error"),
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A raw with no embedded preview has nothing to make a thumbnail from. That must
    /// degrade to an empty thumbnail, never to a failed archive.
    #[test]
    fn cfa_only_source_archives_without_a_thumbnail() {
        let dir = tmp();
        let src = dir.join("a.3fr");
        let arc = dir.join("a.blad");
        let out = dir.join("a.out");
        let bytes = synth_tiff(1024, 1024, 1, 32803, 29);
        std::fs::write(&src, &bytes).unwrap();

        archive(&src, &arc, &Identity).unwrap();
        assert!(thumbnail(&arc).unwrap().is_empty());
        restore(&arc, &out, &Identity).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), bytes);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Reading the thumbnail must not require parsing the manifest — that is the whole
    /// point of putting it at the head. Corrupt the footer and the preview still works.
    #[test]
    fn thumbnail_survives_a_damaged_manifest() {
        let dir = tmp();
        let src = dir.join("a.tif");
        let arc = dir.join("a.blad");
        std::fs::write(&src, synth_tiff(512, 700, 3, 2, 31)).unwrap();
        archive(&src, &arc, &Identity).unwrap();

        let mut data = std::fs::read(&arc).unwrap();
        let n = data.len();
        data[n - 40] ^= 0xFF; // inside the manifest JSON
        std::fs::write(&arc, &data).unwrap();

        assert!(read_manifest(&arc).is_err(), "manifest must be rejected");
        assert!(
            !thumbnail(&arc).unwrap().is_empty(),
            "thumbnail still readable"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A flipped bit inside a manifest *number* stays valid JSON and would silently
    /// yield wrong offsets. The digest is what catches it.
    #[test]
    fn manifest_digest_detects_corruption() {
        let dir = tmp();
        let src = dir.join("a.3fr");
        let arc = dir.join("a.blad");
        std::fs::write(&src, synth_tiff(1024, 1024, 1, 32803, 37)).unwrap();
        archive(&src, &arc, &Identity).unwrap();

        let mut data = std::fs::read(&arc).unwrap();
        let n = data.len();
        data[n - 30] ^= 0x01;
        std::fs::write(&arc, &data).unwrap();

        let e = read_manifest(&arc).unwrap_err();
        assert!(
            format!("{e}").contains("checksum"),
            "expected a checksum failure, got: {e}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_non_archive() {
        let dir = tmp();
        let p = dir.join("nope");
        std::fs::write(&p, b"not an archive at all").unwrap();
        assert!(matches!(read_manifest(&p), Err(Error::NotAnArchive)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncated_archive_is_an_error() {
        let dir = tmp();
        let src = dir.join("a.3fr");
        let arc = dir.join("a.blad");
        std::fs::write(&src, synth_tiff(1024, 1024, 1, 32803, 13)).unwrap();
        archive(&src, &arc, &Identity).unwrap();

        let data = std::fs::read(&arc).unwrap();
        std::fs::write(&arc, &data[..data.len() / 2]).unwrap();
        // Manifest lives in the footer, so a truncated archive fails to parse at all.
        assert!(verify(&arc, &Identity).is_err());
        assert!(verify_quick(&arc).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_predicts_payload_without_encoding() {
        let dir = tmp();
        let src = dir.join("a.3fr");
        std::fs::write(&src, synth_tiff(1024, 1024, 1, 32803, 19)).unwrap();
        let l = plan(&src).unwrap();
        assert_eq!(l.payload_len(), 1024 * 1024 * 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
