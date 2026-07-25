# blad

Byte-exact image archival. Compress a raw or TIFF, get **the same file** back — not
equivalent pixels, the same bytes.

```
$ blad archive x1d-xcd45-03.3FR

 file                   original  stored    ratio   saved
──────────────────────────────────────────────────────────
 x1d-xcd45-03.3FR.blad  105.3 MB   55.8 MB  0.5303  47.0%

  byte-exact reconstruction verified

$ blad restore x1d-xcd45-03.3FR.blad -o out.3FR
$ cmp x1d-xcd45-03.3FR out.3FR && echo identical
identical
```

## Why not just use an existing compressor?

| | ratio on a 105 MB uncompressed 3FR |
|---|---|
| `zstd -19` | 0.71 |
| `xz -9` | 0.91 (on a comparable TIFF) |
| **blad** | **0.53** |

General-purpose compressors look for repeated byte sequences. Photographs don't have
those — they have 2D spatial correlation, which LZ cannot see. `zstd -19` is actually
*worse* than `zstd -3` on this data: more effort spent finding nothing.

And the tools that *can* compress images properly won't give you your file back.
`cjxl` cannot read TIFF at all; converting via PNM discards your ICC profile, Exif,
XMP and GPS, and hands you a PNM instead of a TIFF. Lossless DNG preserves image data
but produces a different file. blad keeps the container.

## How it works

A file is described as an ordered list of byte segments that tile it completely.
Segments we don't model — headers, IFDs, metadata, previews — are stored verbatim.
Segments holding raw pixel data go to a lossless codec. Reassembling in order
reproduces the original exactly.

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
- `verify --quick` checksums the stored bytes without decoding — 4 MB of memory,
  0.17s — so bit-rot scans can run on a schedule across a whole library.
- `verify` reconstructs fully, proving the decode path still works.
- `restore` writes to a `.part` file and renames only after the hash matches, so an
  interrupted restore can never leave something that looks like your original.

## Scope, honestly

blad only recompresses pixel data stored **uncompressed**. Already-compressed regions —
an LZW TIFF, a vendor-compressed CR2 or NEF — are kept verbatim, so those files archive
at roughly 1.000. Modelling an existing compressed bitstream well enough to reproduce it
byte-for-byte is a much harder problem, solved so far only for JPEG (by libjxl). blad
trades that depth for breadth.

If you export TIFFs for archive, export them **uncompressed** and let blad compress
them: you end up smaller than LZW would have made them and keep byte-exact restoration.

Low-ISO files compress better than high-ISO ones. Sensor noise is genuinely random and
therefore incompressible — the same photo at ISO 400 and ISO 1600 gives 0.568 and 0.640.
No encoder can do anything about that.

## Install

```
cargo install --path bin/blad
```

libjxl is vendored and statically linked, so the resulting binary depends only on the
platform C/C++ runtime — nothing to install alongside it. Building from source does
require **cmake** and a C++ toolchain, since libjxl is compiled from source.

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

## Status

Early. `archive` / `verify` / `restore` work and are tested on real Hasselblad files,
but the format will change before 1.0 — the manifest is not yet checksummed, and there
is no error-correcting parity. Don't use it as your only copy of anything.

## License

Apache-2.0 OR MIT
