# blad

[![crates.io](https://img.shields.io/crates/v/blad.svg)](https://crates.io/crates/blad)
[![license](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue.svg)](#license)

Byte-exact image archival. Compress a raw or TIFF, get **the same file** back — not
equivalent pixels, the same bytes.

```console
$ blad archive x1d-xcd45-03.3FR

 file              original  stored   saved
──────────────────────────────────────────────────────────
 x1d-xcd45-03.3FR  105.3 MB  55.8 MB  47.0%  █████▋······
  byte-exact reconstruction verified

$ blad restore x1d-xcd45-03.blad.3FR -o out.3FR
$ cmp x1d-xcd45-03.3FR out.3FR && echo identical
identical
```

A 105 MB Hasselblad raw becomes 56 MB and comes back bit-for-bit. Not "visually
lossless", not "same pixels, new container" — the same SHA-256.

> [!WARNING]
> **Pre-release. Don't use blad as your only copy of anything.**
> The archive format is not frozen — it has changed four times — and blad refuses to
> read archives written by a different format version. There is no error-correcting
> parity, so corruption is detected but not repaired. See [Stability](#stability).

## Install

### Prebuilt binaries — macOS only

Download from [Releases](https://github.com/n0tbhargav/blad/releases). Verify before use:

```console
$ shasum -a 256 -c SHA256SUMS --ignore-missing
$ tar -xzf blad-v0.0.2-aarch64-apple-darwin.tar.gz
$ xattr -d com.apple.quarantine blad-*/blad     # unsigned; Gatekeeper quarantines it
$ ./blad-*/blad --version
```

libjxl is vendored and statically linked, so the binary depends only on `libSystem`,
`libc++` and `libiconv` — nothing to install alongside it.

**Linux and Windows: build from source.** Those binaries are not published, because they
would be cross-compiled on a Mac and shipped without ever having been executed. For a
tool whose whole claim is that bytes are what they say they are, an untested binary is
the wrong thing to hand someone. The build works on both — it just has to be your
machine that proves it.

The `aarch64` build is tested on real Hasselblad files before release. The `x86_64`
build is cross-compiled from Apple Silicon, so it is the weaker artifact — prefer
building from source on an Intel Mac if that matters to you.

### From source

```console
$ cargo install blad
```

Requires **cmake** and a C++ toolchain: libjxl is compiled from source, which takes
about a minute on the first build and is then cached.

## Why not just use an existing compressor?

| | ratio on a 105 MB uncompressed 3FR |
|---|---|
| `zstd -19` | 0.71 |
| `xz -9` | 0.91 (on a comparable TIFF) |
| **blad** | **0.53** |

General-purpose compressors look for repeated byte sequences. Photographs don't have
those — they have 2D spatial correlation, which LZ cannot see. `zstd -19` is actually
*worse* than `zstd -3` on this data: more effort spent finding nothing.

And the tools that *can* compress images properly won't give you your file back. `cjxl`
cannot read TIFF at all; converting via PNM discards your ICC profile, Exif, XMP and
GPS, and hands you a PNM instead of a TIFF. Lossless DNG preserves image data but
produces a different file.

blad keeps the container. That is the whole point — everything else is downstream of it.

## How it works

A file is described as an ordered list of byte segments that tile it completely.
Segments we don't model — headers, IFDs, metadata, previews — are stored verbatim.
Segments holding raw pixel data go to a lossless codec (JPEG XL, via vendored libjxl).
Reassembling in order reproduces the original exactly.

That means partial knowledge of a format is still safe: whatever isn't understood is
copied, never reinterpreted.

### CFA decorrelation

Bayer mosaics get split into four half-resolution planes before encoding.

Horizontally adjacent samples in a mosaic measure **different colours**, which violates
the correlation assumption every predictive coder depends on. Splitting by CFA position
restores it — within a plane, every neighbour measures the same colour.

| approach | ratio |
|---|---|
| mosaic encoded as one grayscale image | 0.613 |
| four CFA sub-planes | **0.534** |

12.8% smaller, and *faster*, since the planes are quarter-size. No CFA pattern knowledge
is needed (RGGB vs BGGR vs anything else) because samples are only moved, never read —
which keeps the transform exactly reversible for any sensor.

## Verification

An archival format that can't prove itself isn't archival.

- `archive` reconstructs from what it just wrote and compares SHA-256 **before**
  reporting success. A file that can't be reproduced is never left on disk.
- `verify --quick` checksums the stored bytes without decoding — 4 MB of memory, 0.17s —
  so bit-rot scans can run on a schedule across a whole library.
- `verify` reconstructs fully, proving the decode path still works.
- `restore` writes to a `.part` file and renames only after the hash matches, so an
  interrupted restore can never leave something that looks like your original.
- The manifest carries its own digest, checked *before* the JSON is parsed. A flipped bit
  inside a manifest number stays valid JSON and would otherwise yield silently wrong
  offsets.

## Usage

```
blad archive <files>...      compress, verified on write
  --dry-run                  show what would be compressed, encode nothing
  --effort <1-10>            default 4; see the note below
  --stats                    per-phase time, throughput, heap and RSS
  --json                     one JSON object per file, for benchmarking

blad verify <archives>...    prove an archive still restores
  --quick                    checksum stored bytes only, no decode

blad restore <archive>       write the original back out
```

**Effort is non-monotonic.** Higher is not reliably smaller: on Bayer planes effort 7
encoded *larger* than effort 4 and took 3.8× longer; on a 51MP RGB frame effort 9 was
larger than effort 7 and 36× slower than 4. The default of 4 was chosen by measurement.
Effort 1 costs about 5% ratio for a 3× speedup, which is reasonable for bulk work.

## Previews, for free

`photo.3FR` archives to `photo.blad.3FR` — the original extension stays last.

An archive opens with a complete JPEG thumbnail, and decoders stop at the end marker and
ignore the archive behind it. Operating systems route files into their image pipeline by
extension but identify the actual format from *content*, so keeping any image extension
means Finder, Explorer and Linux file managers all show a preview. No plugin, no type
declaration, no code signing.

Keeping the **original** extension rather than a generic `.jpeg` does two things: the
name records what is inside, and it is the safer choice. Bulk optimisers and photo
organisers rewrite JPEGs constantly and vendor raw files essentially never — so the
option that looks most like a disguise is in practice the least likely to get your
archive silently overwritten.

The thumbnail comes from the camera's embedded preview where one exists, downscaled in
**linear light** (averaging gamma-encoded values darkens detail) and rotated per the
orientation tag, so portrait frames are not shown sideways.

## Scope, honestly

blad only recompresses pixel data stored **uncompressed**. Already-compressed regions —
an LZW TIFF, a vendor-compressed CR2 or NEF — are kept verbatim, so those files archive
at roughly 1.000. Modelling an existing compressed bitstream well enough to reproduce it
byte-for-byte is a much harder problem, solved so far only for JPEG (by libjxl). blad
trades that depth for breadth.

If you export TIFFs for archive, export them **uncompressed** and let blad compress them:
you end up smaller than LZW would have made them and keep byte-exact restoration.

Low-ISO files compress better than high-ISO ones. Sensor noise is genuinely random and
therefore incompressible — the same photo at ISO 400 and ISO 1600 gives 0.568 and 0.640.
No encoder can do anything about that.

## Stability

| | status |
|---|---|
| byte-exactness | verified on every archive, before it is written |
| archive format | **not frozen** — v4, changed four times |
| CLI surface | expect changes |
| cross-version reads | refused, with the version numbers in the message |

blad refuses archives written by any other format version rather than guessing at their
layout, because a misparsed archive looks like corruption instead of like the version
mismatch it is. In practice: **keep the binary that wrote an archive, or keep the
original file, until 0.1.0.**

At 0.1.0 the format gets a written compatibility guarantee and blad gains the ability to
read older versions. Until then, treat archives as reproducible outputs rather than as
masters.

Tested on macOS ARM64 against real Hasselblad files. Linux is expected to work but is
not currently verified; Windows compiles but is unproven.

## Status

`archive` / `verify` / `restore` work and are tested against real Hasselblad X1D files. Next: `blad exif`, batch parallelism, and lossless-JPEG recompression to
unlock CR2 and compressed DNG.

blad is the archival front end of a larger project — a colour-correct, memory-safe image
pipeline, in the spirit of what FFmpeg is for video.

## License

Apache-2.0 OR MIT, at your option.
