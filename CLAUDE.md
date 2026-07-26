# blad

A color-correct, memory-safe image pipeline. "FFmpeg for photos."

## Thesis

Photo tooling is split across a dozen aging single-purpose libraries — ImageMagick,
libvips, LibRaw, lcms2, ExifTool, and one decoder per format — most of them C parsing
hostile input, none of them sharing a coherent color model. There is no single layer
that ingests anything, processes it correctly at full precision, and emits anything.

blad is that layer.

**Core principle: separate mechanism from policy.**

- **Mechanism is ours** — decode, color transforms, precision, pipeline architecture,
  performance, safety. All of it objectively verifiable: bit-exactness, ΔE against
  references, PSNR/SSIM on demosaic, throughput, fuzz resistance.
- **Policy is not ours** — the "look." Tone curves, color rendering, taste. Those stay
  pluggable data (DCP, ICC, LUTs) that anyone can supply.

FFmpeg has no taste. Neither does blad. That is the entire positioning, and it is what
keeps us out of a fight with Adobe and Hasselblad that we would lose.

## Non-goals

- Not a competitor to Lightroom/Capture One/Phocus. No UI, no catalog, no opinions
  about how images should look.
- Not a new image format. Novel codecs have no adoption path.
- Not video. Multi-frame stills only (bursts, brackets, focus stacks); shell out to
  FFmpeg where the boundary is genuinely video.
- Not named after Hasselblad in any public-facing material. `blad` is a short
  available string; the derivation stays private. Never build product identity out of
  camera-brand trademarks.

## The wedge: color and HDR

Chosen because it is the one gap caused by a *live, unsolved* problem rather than by
decades of unglamorous compatibility work:

- **lcms2** is the entire open ecosystem's color engine (Firefox, GIMP, darktable,
  Krita, ImageMagick, Pillow, Ghostscript). Old C, hard to parallelize, and has **no
  concept of HDR at all** — no CICP, no PQ/HLG, no gain maps.
- **skcms/qcms** are fast and safe but deliberately handle only the ICC subset the web
  uses. Not general-purpose, not adoptable outside their host.
- **OpenColorIO** solves a different problem (scene-referred VFX pipelines, ACES); it
  does not do ICC device profiles.
- **libplacebo** has the best HDR tone mapping in open source, but it is video-oriented
  and GPU-bound. Nobody pulls it into a photo pipeline.
- **Gain maps are fragmented** — Apple, Google Ultra HDR, Adobe, and ISO 21496-1, each
  with at most a single vendor library that handles only itself.

The union — ICC v2/v4 **and** CICP/PQ/HLG **and** all gain-map variants **and** modern
gamut mapping, parallel, memory-safe, behind a C ABI — exists nowhere.

### REVISED 2026-07-25 after prior-art check: moxcms occupies much of this

`moxcms` (crates.io, BSD-3/Apache-2.0, github.com/awxkee/moxcms) is further along than
its README suggests. v0.9.0, created 2025-02-26, **52M downloads**, last commit three
days before this check. 117 source files including:

```
src/cicp.rs      CICP signaling (PQ/HLG/Rec.2020)
src/gamut.rs     gamut mapping
src/chad.rs      chromatic adaptation
src/ictcp.rs     ICtCp — Dolby HDR color space
src/jzazbz.rs    Jzazbz — HDR-capable perceptual space
src/trc.rs  src/oklab.rs  src/oklch.rs  src/dt_ucs.rs  src/srlab2.rs  src/lab.rs
conversions/{avx,sse,neon}/    SIMD across all three ISAs
```

**Do not compete — depend on it.** The license permits it, and it converts our biggest
risk into an asset: a mature color engine on day one, plus a distribution path (moxcms
users are our users).

Not present in its tree: gain-map handling (Apple / Ultra HDR / ISO 21496-1), tone
mapping operators (HDR→SDR display mapping, the libplacebo capability), and DCP camera
profiles (see below).

### The two color regimes — and where the real gap is

Most color libraries only acknowledge one of these. The pipeline crosses a boundary:

```
raw sensor data (3FR/CR3/NEF)
    ↓  CAMERA CHARACTERIZATION   ← DCP model. NOT ICC. Not in moxcms.
wide linear working space           ColorMatrix1/2, ForwardMatrix1/2,
    ↓  ICC TRANSFORMS               HueSatMap, LookTable, ProfileToneCurve
output space (sRGB / AdobeRGB / P3)  ← moxcms handles this well
```

A raw file has **no ICC profile and cannot have one** — it is undefined,
camera-specific sensor response. ICC does not cover characterization at all. The Rust
ecosystem has no DCP support; it exists only inside Adobe's tools and darktable /
RawTherapee's C++.

**So blad owns: camera characterization (DCP) + the pipeline**, with gain maps and tone
mapping as the HDR extension. moxcms owns ICC. That is a sharper and more defensible
split than the original Phase 1 scope.

## Status (2026-07-25)

Phase 0 works end to end. `archive` / `verify` / `restore` implemented, 34 tests green,
validated on real files:

| file | original | stored | x | time |
|---|---|---|---|---|
| `x1d-xcd45-03.3FR` (X1D, CFA 16-bit) | 105.3 MB | 55.9 MB | **0.531** | 3.8s |
| `x1d-xcd45-03.tif` (ISO 400 render) | 293.5 MB | 166.6 MB | **0.568** | 8.2s |
| `test.tif` (ISO 1600 render) | 293.5 MB | 187.7 MB | **0.640** | 8.3s |

All byte-exact, confirmed by `cmp` and independent `shasum` against the originals.

The 3FR beats the shell prototype's 0.554 because `blad-container` also finds the 4.4MB
embedded 8-bit preview as a compressible image segment, which the manual version left in
the skeleton. The TIFF figures match the prototype to four decimal places — two
independent implementations agreeing.

### Streaming rewrite (done)

Format v2 moves the manifest to a **footer** (`body … manifest … u32 len`), because blob
sizes are only known after encoding. That lets the body stream straight to the output
file instead of accumulating in memory. Alongside it: `blad-cfa` and `blad-codec` became
byte-oriented (no `Vec<u16>` materialisation — that was a full extra copy of the frame
purely to change endianness), and reconstruction streams through a hashing writer rather
than being built in memory.

| | before | after |
|---|---|---|
| 3FR archive | ~700 MB | **317 MB** |
| 3FR verify (full) | ~1.8 GB | **263 MB** |
| TIFF archive | 1.8 GB | 1.60 GB |
| TIFF verify (full) | ~1.8 GB | **789 MB** |
| **verify --quick** | n/a | **4 MB, 0.17s** |

### CORRECTION: the memory was never ours

An earlier reading of these numbers blamed temp-file page cache. That was **wrong**, and
the mistake was methodological: `/usr/bin/time -l` reports peak RSS **including child
processes**, and the child dominates.

Measured directly:

| | peak RSS |
|---|---|
| `cjxl` alone, 294 MB 16-bit RGB | **1602 MB** |
| `djxl` alone, same image | **783 MB** |
| `blad archive` (self-measured, `getrusage(RUSAGE_SELF)`) | **300 MB** |
| `blad verify` (self-measured) | 300 MB |

1602 and 783 match the 1604 and 789 previously attributed to blad. **blad's own peak is
~300 MB on a 294 MB file — about 1.02x, essentially optimal for a design that holds one
image segment resident.** The streaming work is done; there is no remaining blad-side
memory problem to solve.

Three consequences:

1. **In-process libjxl will not deliver a large memory win.** The encoder needs that
   working set wherever it runs. FFI is still worth doing — no process spawn, no disk
   round trip, no system dependency — but justify it on **speed and packaging, not
   memory**.
2. **The CFA split has a third benefit we had not identified: it caps codec memory.**
   Four quarter-size planes mean each `cjxl` invocation sees 25 MB instead of 294 MB,
   which is why the 3FR path peaks at 318 MB total while the single-blob TIFF path
   drives the codec to 1.6 GB. **Tiling large RGB images would bound encoder memory the
   same way** — a stronger argument for tiling than compression ratio alone.
3. **`--jobs` must budget for the codec's memory, not ours.** Eight concurrent TIFF
   encodes is ~13 GB of *cjxl*, regardless of how lean blad is.

**Lesson for future benchmarking: always measure the process you mean.** `--stats` now
reports self-measured per-phase heap and RSS for exactly this reason.

### Vendored libjxl (done)

`jpegxl-rs` with `features = ["vendored"]` pulls in `jpegxl-src`, which builds libjxl
0.12.0 from source via cmake. **53s cold build** on 8 cores, cached after. Produces
static archives (`libjxl.a`, `libhwy.a`, `libbrotli*.a`, `libjxl_cms.a`,
`libjxl_threads.a`) and the linked binary's *entire* dynamic dependency list is:

```
/usr/lib/libiconv.2.dylib
/usr/lib/libSystem.B.dylib
```

No libjxl dylib. Nothing preinstalled needed. It also pins the codec version, so
benchmarks stay comparable instead of shifting under someone's `brew upgrade`.

Both backends ship: `--backend native` (default) and `--backend cli`. Keeping the CLI
path costs little and buys A/B measurement plus an escape hatch if a libjxl version
misbehaves. A test asserts **streams from one backend decode in the other** — if that
ever broke, archives would become build-specific, the worst failure an archival format
can have.

| | ratio | encode CLI | encode native | speedup |
|---|---|---|---|---|
| 3FR (CFA) | 0.5313 → 0.5303 | 2812 ms | **1536 ms** | **1.83×** |
| TIFF (RGB) | 0.5679 (identical) | 6258 ms | **3826 ms** | **1.64×** |

libjxl 0.12 did not regress ratios versus the 0.11.1 CLI. Byte-exactness confirmed on
the real 3FR, both directions across backends.

**Two API traps, both found by measuring rather than reasoning:**

1. **`parallel_runner` defaults to `None` — single-threaded.** The first native
   implementation was *51-79% slower than spawning a subprocess*. Passing
   `ThreadsRunner::default()` to both encoder and decoder is not optional.
2. **`lossless(true)` requires `uses_original_profile(true)`**, and the colour encoding
   must match the channel count (`SrgbLuma` for gray, `Srgb` for RGB) or libjxl rejects
   the configuration with "The encoder API is used in an incorrect way". The profile is
   metadata only — lossless recovers every sample exactly regardless, and what the
   samples *mean* is defined by the container we preserve verbatim.

### Measurement is built in

This project makes claims about size, speed, and memory, so measurement is part of the
binary rather than a side script:

- `blad archive --stats` — per-phase timing (analyze / encode / verify), throughput, and
  self-measured peak RSS via `getrusage`. Note `ru_maxrss` is **bytes on macOS,
  kilobytes on Linux**; getting that wrong yields silently 1024×-wrong numbers.
- `blad archive --json` — one JSON object per file on stdout. A benchmark harness is
  therefore a shell loop plus `jq`, not a subsystem, and the output is diffable across
  commits for regression tracking.

Where time goes today (X1D 3FR, 105 MB, effort 4): encode 75%, verify 25%, analyze ~0%.
Verify runs at ~110 MB/s, encode at ~37 MB/s.

**Encoder effort is non-monotonic, and worse than previously recorded.** Measured on the
3FR's CFA planes:

| effort | ratio | encode |
|---|---|---|
| 1 | 0.5599 | 0.9s |
| **4** | **0.5313** | 2.9s |
| 7 | 0.5357 | 10.9s |

Effort 7 is *larger* than 4 and 3.8× slower — the same reversal seen at effort 9 on RGB,
but it arrives earlier on mosaics. Effort 1 costs only ~5% ratio for a 3× speedup, which
is a reasonable `--effort 1` recommendation for bulk work. **Do not raise the default
without measuring on both CFA and RGB.**

### Known limitations

1. **Codec shells out to `cjxl`/`djxl`** via temp files: process spawn per plane (four
   per 3FR) plus a full disk round trip on every encode *and* every verify. Encode is
   ~75% of runtime. The [`Codec`] trait isolates it. **Top priority — for speed and to
   drop the system dependency, not for memory** (see the correction above).
   Plan: vendor libjxl so the binary is self-contained and the codec version is pinned
   (keeping benchmarks comparable over time); consider `jxl-oxide` (pure Rust) for the
   *decode* path specifically, since decode is where hostile input lands and that is
   where the memory-safety claim actually matters.
2. **Compressed TIFF/raw is left verbatim.** Only uncompressed strips are modelled, so
   an LZW TIFF archives at 1.000 — measured. Modelling an existing compressed bitstream
   byte-exactly is the Lepton-class problem, solved so far only for JPEG (by libjxl).
   blad trades that depth for breadth.
3. **No parity/error correction.** `verify --quick` detects bit rot at I/O speed, but
   corruption can be neither located nor repaired.
4. **The manifest itself is not checksummed, and exists in one copy.** Both hashes
   (`original.sha256`, `body_sha256`) live in the JSON footer, so a single flipped bit
   there makes the whole archive unreadable even when the body is perfect. Real archival
   formats replicate or checksum their directory. Cheap to fix now, painful once
   archives exist in the wild.
5. **No parallelism across files.** `archive` takes multiple inputs but processes them
   sequentially. Batch `--jobs` must be bounded by a **memory budget**, not core count:
   8 concurrent TIFFs at present usage would want ~13 GB.

### CLI surface

```
blad archive [--dry-run] [--effort N]   main verb, verified on write, batch-capable
blad verify  [--quick]                  does it still restore?
blad restore                            get the original back
blad layout                             hidden; development aid
```

`inspect` was dropped: it read as "tell me about this file," which is what `exif` will
be for, and its one real user question ("is this worth archiving?") is better answered by
`archive --dry-run` in the place you would actually ask it. Output uses `comfy-table`.

## Where blad sits

```
┌─ APPLICATIONS ────────────── Phocus, Lightroom, darktable, your app
│  the look: tone curves, HNCS, presets, sliders, UI          ← NOT US
├─ blad ────────────────────────────────────────────────────────────
│  ingest      any container → sensor data or pixels + all metadata
│  mechanical  black level, WB apply, demosaic, highlight recon,
│              lens correction from recorded profiles
│  transport   execute whatever characterization is handed to us
│              (DCP / ICC / matrix) — correctly, at full precision
│  pipeline    tiled, streaming, deterministic, parallel, GPU
│  output      encode, resample in linear light, metadata fidelity
│  archival    blad archive — byte-exact, verified
├─ moxcms ─────────────────── ICC transforms                  ← DEPEND
└─ libjpeg-turbo / libpng / libavif / LibRaw ── decode (initially)
```

**One line: blad gets you from *file* to *correct linear pixels* and back, with no
opinion about what happens in between.**

Hasselblad, Adobe, Capture One, darktable, and every phone camera team each rebuilt
this mechanical layer from scratch because there was nothing to build on. blad is that
layer, so the next person doesn't have to. We are not competing with Phocus; we are
what Phocus would be built on if it were written today.

**The tell that we are on the right side of the line: every task here has a number
attached** — ΔE, PSNR, compression ratio, throughput, bit-exactness. The moment a
task's success criterion becomes "does it look good," it belongs to someone else's
layer.

## Roadmap

### Phase 0 — `blad archive` (CURRENT)

Byte-exact archival, shipped as blad subcommands rather than a separate binary:
`blad archive`, `blad verify`, `blad restore`.

Decompose a file into container skeleton + pixel payload, compress the payload
(CFA-split for Bayer), store both, reconstruct **byte-exactly** on demand, with
round-trip verification at write time.

**This is not a detour — it is blad's ingest layer with a CLI on top.** Writing it
requires container parsing, perfect metadata fidelity, CFA layout handling, and
byte-exact reconstruction, all of which Phase 2 needs anyway.

Chosen as first build because it is the only part of the plan that is already
validated with real numbers (see Measurements), it demonstrably does not exist, it is
finishable in weeks, and we are its user.

**Discipline: no logic in the CLI.** Every piece lands in a library crate; the binary
is a thin shell. `blad-container` and `blad-meta` are the same components Phase 2
needs, and tool-specific code would have to be rewritten.

**v1 success criterion:** `blad archive` the Desktop 3FR, `blad verify` reports a
matching hash, `blad restore` yields a byte-identical file. Everything after that is
format coverage.

### Phase 1 — color/HDR engine, standalone
Useful and adoptable on its own; small enough to actually finish. ICC v2/v4 parsing and
transforms, chromatic adaptation, CICP/PQ/HLG, gain-map ingestion across all variants,
CSS Color 4-style gamut mapping. SIMD, parallel, C ABI.

**Success signal:** another project (darktable, a Rust image crate, anyone) adopts it
within a year. If nobody does, the FFmpeg-scale version is not going to happen, and we
learn that cheaply.

### Phase 2 — pipeline + formats
Demand-driven operation graph, lazily evaluated, tiled so a 100MP 16-bit image never
fully materializes. Streaming for files larger than RAM. Color engine wired through the
middle. Wrap libjpeg-turbo / libpng / libavif / LibRaw initially so the thing is useful
immediately; replace decoders selectively with safe implementations, JPEG and PNG first
(highest volume, highest attack surface).

**Hard requirement:** output must be bit-identical regardless of tile size, thread
count, or evaluation order. Determinism is a testable property and we test it.

### Phase 3 — raw, metadata, GPU
Full raw pipeline (demosaic, highlight reconstruction, lens correction), complete
metadata fidelity, GPU backend proven to match the CPU path within a stated tolerance.

### Backlog (ordered)

1. ~~Vendored libjxl via FFI.~~ **Done** — see above. 1.6-1.8x faster encode,
   self-contained binary.
2. ~~Format v3.~~ **Done.** Head thumbnail (`u32` length + JPEG after the magic) plus an
   8-byte SHA-256 prefix over the manifest in the footer. The digest is checked *before*
   the JSON is parsed: a flipped bit inside a manifest *number* stays valid JSON and
   would silently yield wrong offsets — the failure that looks like a codec bug and is
   not.
3. ~~`blad thumb`.~~ **Done.** 512px JPEG, **0.050%** of a 56 MB archive, read with one
   seek and no JXL decode. Source is the smallest RGB segment — normally the camera's
   own embedded preview, already demosaiced and colour-rendered. CFA-only sources get an
   empty thumbnail rather than a failed archive.
   - Downscaling runs in **linear light**. A test averages a black/white checkerboard and
     asserts ~188 (the photometric mean), not 128 (what averaging gamma-encoded values
     gives). Getting this wrong is what makes most software's thumbnails too dark.
   - **Orientation is applied.** The X1D writes tag 274 = 8 and stores its preview
     landscape; without this every portrait photo shows up sideways in Finder. Caught by
     looking at the output image, not by a passing test.
4. **OS thumbnail integration — attempted, then dropped.**

   Format v4 makes an archive a valid JPEG (thumbnail at offset 0; decoders stop at
   `FFD9`), which *does* let Apple's ImageIO decode it: `sips` reads a 56 MB archive as
   384x512 without complaint. Quick Look renders it at every icon size.

   But macOS resolves file types by **extension**, not content, so it still needs a UTI
   declaration saying `.blad` conforms to `public.jpeg` — and UTI declarations must live
   in an app bundle. We built one (plist + stub, no code), and hit two walls:
   `CFBundleDocumentTypes` was required as well or Finder drew its "no handler" icon, and
   `lsregister` still marked the declaration **untrusted** because ad-hoc signing is not
   a Developer ID. Finder icons stayed generic.

   **Removed.** The plist scaffolding was not worth carrying for an icon. The format
   change stays — it costs nothing, `blad thumb` works, and it leaves the door open.

   Worth knowing: **Hasselblad does nothing here.** Phocus ships no Quick Look plugin;
   macOS has *built-in* RAW support and lists Hasselblad X1D-50c among its known cameras.
   That path is Apple's and is not open to third parties at any price.

5. **`blad exif`** — read standard TIFF/Exif/GPS IFDs. The IFD walker already exists;
   what is missing is a tag dictionary and type-aware formatting. Explicitly excluded
   from v1: maker notes (pass through opaquely) and *writing* metadata.
6. **`indicatif` progress.** 8s per file, minutes per library. Progress must go to
   **stderr** so `--json` stays parseable on stdout.
7. **Batch `--jobs`**, bounded by a memory budget rather than core count — and the
   budget must account for the codec's working set, not blad's.
8. **LJ92 (lossless JPEG) byte-exact recompression.** The highest-leverage format work:
   unlocks CR2 and compressed DNG, which today archive at ~1.000 because their pixel
   data is already compressed. More tractable than general JPEG — predictive coding plus
   Huffman, no DCT or quantisation tables.

### Crate layout

```
blad/
├── crates/
│   ├── blad-container/   TIFF/EP, 3FR parsing; skeleton/payload split
│   ├── blad-meta/        Exif, XMP, IPTC, ICC — lossless round-trip
│   ├── blad-cfa/         Bayer layout, sub-plane split/merge  ← proven 12.8%
│   ├── blad-codec/       JXL bindings now, own decoders later
│   ├── blad-mem/         tracking allocator + RSS; per-phase memory attribution
│   └── blad-core/        pipeline, color, the rest (later)
└── bin/blad/             CLI: archive · verify · restore, later convert etc.
```

## Measurements (established 2026-07-25)

Test file: Hasselblad X1D II 50C, 8272×6200, 16-bit RGB, uncompressed TIFF,
ISO 1600, Adobe RGB (1998). 307,718,400 bytes of pixel data. 8-core Apple Silicon.

### Lossless compression

| Method | Size | x | Time |
|---|---|---|---|
| original | 307.7 MB | 1.000 | — |
| `zstd -3` | 293.9 MB | 0.954 | 0.3s |
| `zstd -19 --long` | 294.0 MB | 0.955 | 28s |
| `xz -9 -T8` | 279.1 MB | 0.906 | 98s |
| **`cjxl -d0 -e4`** | **196.8 MB** | **0.639** | **5.0s** |
| `cjxl -d0 -e7` | 195.8 MB | 0.636 | 25s |
| `cjxl -d0 -e9` | 198.1 MB | 0.643 | 182s |

### Lossy (SSIMULACRA2; 90+ = visually lossless threshold)

| Setting | Size | x | SSIM2 | Time |
|---|---|---|---|---|
| `cjxl -d 0.5` | 15.6 MB | 0.051 | 89.2 | 4.9s |
| `cjxl -d 1.0` | 9.2 MB | 0.030 | 85.6 | 6.2s |
| `cjxl -d 2.0` | 4.9 MB | 0.016 | 78.4 | 5.1s |

### Raw (3FR) compression

Test file: `x1d-xcd45-03.3FR` — Hasselblad X1D, XCD 45, ISO 400, 8384×6304 16-bit
Bayer CFA, **stored uncompressed** (105,705,472 bytes of mosaic = exactly W×H×2).

| Method | Size | x (vs mosaic) |
|---|---|---|
| `zstd -19` | 75.9 MB | 0.718 |
| `cjxl -d0 -e4`, mosaic as one grayscale image | 64.8 MB | 0.613 |
| **`cjxl -d0 -e4`, split into 4 CFA sub-planes** | **56.5 MB** | **0.534** |

**CFA decorrelation is worth 12.8%** over the naive approach, and is *faster* (smaller
images, better parallelism). Round trip verified bit-identical after re-interleaving.
General-purpose compressors structurally cannot do this — they don't know the data is
a Bayer mosaic.

Whole-file `fixer` math:

```
original 3FR                          110,379,008
  skeleton (verbatim, incl. preview)    4,673,536
  compressed payload                   56,488,351
  total stored                         61,161,887     x = 0.554
```

**45% off, fully reversible.** Better than the TIFF case because 3FR is uncompressed to
begin with. 4.6MB of the 4.67MB skeleton is the embedded JPEG preview, itself
compressible — lossless JPEG recompression would take total x to ~0.53.

### What Hasselblad actually writes into a 3FR

TIFF/EP container, little-endian. Characterization tags present:

- `ColorMatrix1` (single illuminant, camera RGB → XYZ)
- `AsShotNeutral`, `BlackLevel` 256, `WhiteLevel` 65535
- `DefaultCropOrigin` 50 100, `DefaultCropSize` 8272 6200
- `UniqueCameraModel`, `LensModel`, firmware version
- 4.6MB embedded `PreviewTIFF`

Absent: `ColorMatrix2`, `ForwardMatrix1/2`, `HueSatMap`, `LookTable`, `ProfileToneCurve`.
Hasselblad ships the bare minimum characterization and keeps HNCS entirely inside
Phocus. Confirms that matching their look requires our own calibration, not extraction.

### Findings that shaped the plan

1. **Generic compressors are useless on photographic data.** zstd recovers 4.6%,
   xz 9.4%. LZ finds repeated byte sequences; images have 2D correlation instead.
   `zstd -19` is *worse* than `-3`. Prediction + context-modeled entropy coding is the
   only approach that works.
2. **`cjxl` cannot read TIFF.** Inputs are PNG/PPM/PNM/PFM/PAM/PGX/APNG/GIF/JPEG/EXR.
   The best lossless image compressor in existence cannot ingest the format
   photographers archive in. Real, unfilled usability gap.
3. **The PPM workaround destroys all metadata** — ICC, EXIF, XMP, GPS. No CLI does
   TIFF→JXL with metadata preserved. Disqualifying for archival.
4. **AVIF/HEIC cannot compete for masters.** AV1 tops out at 12-bit; these files are
   16-bit. JPEG XL is the *only* modern codec that can hold a 16-bit master losslessly.
5. **`cjxl -e9` is a regression** — 1.2% larger than `-e7` and 36× slower than `-e4`.
   Effort 9 switches strategy and loses on this content. Only measurement reveals this.
6. **Byte-exact reconstruction works.** For uncompressed single-strip TIFF, storing an
   8,351-byte container skeleton (4,664-byte header/IFD + 3,687 trailing bytes)
   alongside the JXL payload reconstructs the original file with an identical MD5.
   Total x = 0.6396, overhead 0.004%. This is `fixer`'s v1 in miniature. Generalizing
   to compressed/tiled TIFF and to raw formats is much harder — reproducing a vendor's
   exact compressed bitstream is the Lepton-class problem, and that difficulty is the
   moat.
7. **Noise is the floor.** At ISO 1600 a large fraction of the data is genuine sensor
   randomness, which is information-theoretically incompressible. ~0.64 is near the
   practical limit; lossless will never deliver an order of magnitude.

## Technical decisions

**Language: Rust.** The thesis is memory-safe decoding of hostile input, a stable C
ABI, and a clean wasm build. Rust is the obvious fit for all three. (Revisitable, but
it would take a strong argument.)

**Precision.** Everything internal is high bit depth in a wide, roughly linear working
space. No premature quantization, no intermediate 8-bit, ever. Error accumulation
through a long pipeline is a defect class we actively test for.

**Resampling in linear light.** Most software resizes in gamma-encoded space, which is
mathematically wrong and visibly darkens fine detail. We do it correctly and we
demonstrate the difference.

**Security is a feature, not hygiene.** Resource limits against decompression bombs,
timeouts, memory caps, sandboxed decoders, continuous fuzzing. Anyone accepting user
uploads has this problem today and has no good answer.

**Test infrastructure is the product's credibility.** For infrastructure meant to be
trusted, the test rig is what lets one person credibly claim to replace a library with
25 years of accumulated bug fixes. Required: reference corpora, continuous fuzzing,
differential testing against ImageMagick/libvips/browsers, golden-image regression,
perceptual metrics (SSIMULACRA2, butteraugli — *not* PSNR) and determinism tests across
tile sizes and thread counts. Benchmarks in CI.

## Explicitly out of scope: matching HNCS

Decided and closed. Reproducing Hasselblad's (or Adobe's, or anyone's) rendering is
per-camera taste work — the exact thing the mechanism/policy split puts on the other
side of the line.

For the record, since it will come up again: HNCS is not extractable from a 3FR. The
file carries only `ColorMatrix1` + `AsShotNeutral`; the look lives in Phocus as
proprietary profiles and LUTs. It *is* characterizable as a black box (synthesize raw
sweeping the input cube, run through Phocus, fit a 3D LUT) — but Phocus's EULA
prohibits reverse engineering, a copied LUT caps our ceiling at "slightly worse
Phocus," and shipping one contradicts the positioning. **We execute whatever profile
the user supplies; we do not author looks.**

Measured Phocus rendering signatures are retained above purely as evidence of what an
application-layer render does, not as a target to hit.

## Open questions

- **Prior art: partially checked (2026-07-25).** `moxcms` resolved — see the REVISED
  section above; conclusion is depend, don't compete. **Still unchecked:** whether
  anyone has built the gain-map / tone-mapping layer, whether any Rust crate does DCP
  camera profiles, and where ISO 21496-1 adoption actually landed.
- Name availability beyond crates.io. `blad` is free on crates.io; `blade` is taken by
  a graphics library; `bradford` (candidate name for the color module) is free;
  `fixer` is taken on crates.io by a FIX-protocol library. npm/PyPI/GitHub/domains
  unchecked.
- ΔE2000 map between neutralized Phocus and Lightroom exports of one 3FR — cheap
  experiment that reveals which pipeline stages actually matter and which can be
  implemented naively.
