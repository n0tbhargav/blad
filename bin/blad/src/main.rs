//! blad CLI.
//!
//! Deliberately thin: every command is argument parsing, a library call, and formatting.
//! Logic belongs in the crates, because Phase 2 needs the same components and
//! tool-specific code would have to be rewritten.

use anyhow::{bail, Context, Result};
use blad_codec::Jxl;
use blad_container::{Layout, SegmentKind};
use clap::{Parser, Subcommand};
use comfy_table::{
    presets, Attribute, Cell, CellAlignment, Color, ContentArrangement, Table, TableComponent,
};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// Installed so heap usage can be attributed per phase. Counters are relaxed atomics;
/// the cost on the allocation path is negligible and the visibility is worth it for a
/// project whose claims are all measurements.
#[global_allocator]
static ALLOC: blad_mem::Tracking<std::alloc::System> = blad_mem::Tracking(std::alloc::System);

#[derive(Parser)]
#[command(
    name = "blad",
    version,
    about = "Colour-correct, memory-safe image tooling"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compress files to .blad archives, verifying byte-exact reconstruction.
    Archive {
        /// Files to archive.
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Output path. Only valid with a single input; otherwise each file gets
        /// <input>.blad alongside it.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Encoder effort 1-9. Higher is not always smaller: on photographic content
        /// effort 9 measured larger than 7 and 36x slower than 4.
        #[arg(short, long, default_value_t = 4)]
        effort: u8,
        /// Show what would be compressed, without encoding anything.
        #[arg(long)]
        dry_run: bool,
        /// Report per-phase timing, throughput, and peak memory.
        #[arg(long)]
        stats: bool,
        /// Emit one JSON object per file on stdout, for benchmarking and regression
        /// tracking. Implies --stats data; suppresses the tables.
        #[arg(long)]
        json: bool,
    },
    /// Check archives still restore correctly.
    Verify {
        #[arg(required = true)]
        archives: Vec<PathBuf>,
        /// Checksum the stored bytes without decoding. Catches bit rot at I/O speed,
        /// so it can run on a schedule; will not catch a codec bug.
        #[arg(short, long)]
        quick: bool,
    },
    /// Restore the original file from an archive.
    Restore {
        archive: PathBuf,
        /// Output path (default: the original file name, in the current directory).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Extract an archive's embedded preview as a JPEG. Development aid: the archive is
    /// already a valid JPEG, so anything that displays images shows the preview without
    /// this. It exists to check *what was embedded*, which is how the sideways-thumbnail
    /// bug was found.
    #[command(hide = true)]
    Thumb {
        archive: PathBuf,
        /// Output path, or `-` for stdout (default: <archive>.jpg).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Read Exif, TIFF and DNG metadata.
    Exif {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Restrict to directories: tiff, sub, exif, gps, interop. Repeatable.
        #[arg(short, long)]
        group: Vec<String>,
        /// Only tags whose name contains this. Repeatable, case-insensitive.
        #[arg(short, long)]
        tag: Vec<String>,
        /// Include tags with no dictionary entry, shown by number.
        #[arg(short, long)]
        all: bool,
        /// Hide GPS coordinates, serial numbers and owner names.
        #[arg(long)]
        redact: bool,
        /// Values without unit interpretation.
        #[arg(long)]
        raw: bool,
        /// Every directory entry under its standard tag name, as a table.
        #[arg(short, long)]
        full: bool,
        /// Show the file offset and type of every value.
        #[arg(long)]
        offsets: bool,
        /// One JSON object per file.
        #[arg(long)]
        json: bool,
    },
    /// Show how blad decomposes a file into segments. Development aid.
    #[command(hide = true)]
    Layout { input: PathBuf },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Archive {
            inputs,
            output,
            effort,
            dry_run,
            stats,
            json,
        } => cmd_archive(&inputs, output.as_deref(), effort, dry_run, stats, json),
        Command::Verify { archives, quick } => cmd_verify(&archives, quick),
        Command::Restore { archive, output } => cmd_restore(&archive, output.as_deref()),
        Command::Thumb { archive, output } => cmd_thumb(&archive, output.as_deref()),
        Command::Exif {
            files,
            group,
            tag,
            all,
            redact,
            raw,
            full,
            offsets,
            json,
        } => cmd_exif(&files, &group, &tag, all, redact, raw, full, offsets, json),
        Command::Layout { input } => {
            print_layout(&input, &blad_archive::plan(&input)?);
            Ok(())
        }
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Colour only when a human is looking at it: a terminal, and NO_COLOR unset.
/// Piping into a file or `jq` must produce clean text.
fn colour() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// A proportion bar. Filled means "saved" or "share of time" — more filled is more,
/// which is the only reading that needs no legend.
///
/// Eighth-blocks give sub-character resolution, so a 12-wide bar distinguishes ~1%
/// differences instead of rounding to 8% steps.
fn bar(fraction: f64, width: usize) -> String {
    const PARTIAL: [char; 8] = [
        '\u{258f}', '\u{258e}', '\u{258d}', '\u{258c}', '\u{258b}', '\u{258a}', '\u{2589}',
        '\u{2588}',
    ];
    let eighths = (fraction.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let rem = eighths % 8;
    let mut out = String::new();
    for _ in 0..full.min(width) {
        out.push('\u{2588}');
    }
    if rem > 0 && full < width {
        out.push(PARTIAL[rem - 1]);
    }
    while out.chars().count() < width {
        out.push('\u{00b7}');
    }
    out
}

fn green(c: Cell) -> Cell {
    if colour() {
        c.fg(Color::Green)
    } else {
        c
    }
}

fn dim(c: Cell) -> Cell {
    if colour() {
        c.add_attribute(Attribute::Dim)
    } else {
        c
    }
}

/// Borderless, with a single rule under the header.
///
/// A line between every row turns a five-row table into visual noise; the header rule
/// is the only separator that carries information.
fn table() -> Table {
    let mut t = Table::new();
    t.load_preset(presets::NOTHING)
        .set_style(TableComponent::HeaderLines, '─')
        .set_content_arrangement(ContentArrangement::Dynamic);
    if !colour() {
        t.force_no_tty();
    }
    t
}

fn right(s: impl ToString) -> Cell {
    Cell::new(s).set_alignment(CellAlignment::Right)
}

/// Throughput, or an em dash when the interval is too short to divide by.
///
/// Dividing 105 MB by a sub-millisecond parse produced "173.5 GB/s", which is an
/// artifact of the clock rather than a measurement. Refusing to print it is more honest
/// than printing a number nobody should believe.
fn throughput(bytes: u64, d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    if s < 0.005 {
        return "—".into();
    }
    format!("{}/s", human((bytes as f64 / s) as u64))
}

fn codec(effort: u8) -> Jxl {
    Jxl {
        effort: effort.clamp(1, 10),
    }
}

/// `photo.3FR` becomes `photo.blad.3FR`.
///
/// The original extension stays *last* on purpose. An archive opens with a JPEG
/// thumbnail, and ImageIO identifies image formats from content rather than from the
/// name — so any extension the system already routes into its image pipeline gets
/// previews in Finder, Explorer and every file manager, with no plugin, no type
/// declaration and no code signing.
///
/// Keeping the *original* extension rather than a generic `.jpeg` also means the name
/// records what is inside, and it is the safest choice: bulk optimisers and photo
/// organisers rewrite JPEGs constantly and vendor raw files essentially never, so the
/// option that looks most like a disguise is in practice the least likely to get the
/// archive silently overwritten.
fn archive_name(input: &Path) -> PathBuf {
    match (input.file_stem(), input.extension()) {
        (Some(stem), Some(ext)) => {
            let mut name = stem.to_owned();
            name.push(".blad.");
            name.push(ext);
            input.with_file_name(name)
        }
        // No extension to preserve, so there is nothing to route on either.
        _ => {
            let mut p = input.as_os_str().to_owned();
            p.push(".blad");
            PathBuf::from(p)
        }
    }
}

fn print_layout(input: &Path, layout: &Layout) {
    let mut t = table();
    t.set_header(vec![
        Cell::new("#").add_attribute(Attribute::Bold),
        Cell::new("offset").add_attribute(Attribute::Bold),
        Cell::new("size").add_attribute(Attribute::Bold),
        Cell::new("kind").add_attribute(Attribute::Bold),
    ]);
    for (i, seg) in layout.segments.iter().enumerate() {
        let kind = match &seg.kind {
            SegmentKind::Verbatim => "verbatim".to_string(),
            SegmentKind::Image(spec) => format!(
                "image {}×{} {}-bit {} {}",
                spec.width,
                spec.height,
                spec.bits_per_sample,
                match spec.layout {
                    blad_container::PixelLayout::Cfa => "CFA".to_string(),
                    blad_container::PixelLayout::Chunky => format!("{}ch", spec.samples_per_pixel),
                },
                if spec.little_endian { "LE" } else { "BE" }
            ),
        };
        t.add_row(vec![
            right(i.to_string()),
            right(seg.src_offset.to_string()),
            right(human(seg.len)),
            Cell::new(kind),
        ]);
    }

    println!("{}", input.display());
    println!("{t}");
    let pct = 100.0 * layout.payload_len() as f64 / layout.total_len.max(1) as f64;
    if layout.orientation != 1 {
        println!(
            "  orientation {} (pixels are not stored upright)",
            layout.orientation
        );
    }
    println!(
        "  {} total · {} compressible ({pct:.1}%) · {} verbatim",
        human(layout.total_len),
        human(layout.payload_len()),
        human(layout.skeleton_len()),
    );
    if layout.payload_len() == 0 {
        println!("  nothing to compress — pixel data here is already compressed, so blad");
        println!("  would store this file verbatim at roughly its current size.");
    }
}

fn print_stats(input: &Path, r: &blad_archive::ArchiveReport) {
    let t = &r.timings;
    let ms = |d: std::time::Duration| format!("{:.0} ms", d.as_secs_f64() * 1000.0);
    let pct = |d: std::time::Duration| {
        format!(
            "{:.0}%",
            100.0 * d.as_secs_f64() / t.total.as_secs_f64().max(1e-9)
        )
    };

    let share = |d: std::time::Duration| d.as_secs_f64() / t.total.as_secs_f64().max(1e-9);

    let mut tb = table();
    tb.set_header(vec![
        Cell::new("phase").add_attribute(Attribute::Bold),
        Cell::new("time").add_attribute(Attribute::Bold),
        Cell::new("share").add_attribute(Attribute::Bold),
        Cell::new("").add_attribute(Attribute::Bold),
        Cell::new("throughput").add_attribute(Attribute::Bold),
        Cell::new("heap peak").add_attribute(Attribute::Bold),
        Cell::new("RSS after").add_attribute(Attribute::Bold),
    ]);
    for (name, p) in [
        ("analyze", &t.analyze),
        ("encode", &t.encode),
        ("verify", &t.verify),
    ] {
        tb.add_row(vec![
            Cell::new(name),
            right(ms(p.time)),
            right(pct(p.time)),
            dim(Cell::new(bar(share(p.time), 10))),
            right(throughput(r.original_len, p.time)),
            right(human(p.heap_peak)),
            right(human(p.rss_after)),
        ]);
    }
    tb.add_row(vec![
        Cell::new("total").add_attribute(Attribute::Bold),
        right(ms(t.total)).add_attribute(Attribute::Bold),
        Cell::new(""),
        Cell::new(""),
        right(throughput(r.original_len, t.total)),
        Cell::new(""),
        right(human(blad_mem::rss_highwater())).add_attribute(Attribute::Bold),
    ]);

    println!(
        "{}  ({} payload, {} skeleton)",
        input.display(),
        human(r.payload_len),
        human(r.skeleton_len),
    );
    println!("{tb}");
    println!();
}

fn json_line(input: &Path, r: &blad_archive::ArchiveReport, effort: u8) -> String {
    let t = &r.timings;
    serde_json::json!({
        "file": input.to_string_lossy(),
        "effort": effort,
        "original_len": r.original_len,
        "stored_len": r.stored_len,
        "payload_len": r.payload_len,
        "skeleton_len": r.skeleton_len,
        "ratio": r.ratio(),
        "sha256": r.sha256,
        "ms_analyze": t.analyze.time.as_secs_f64() * 1000.0,
        "ms_encode": t.encode.time.as_secs_f64() * 1000.0,
        "ms_verify": t.verify.time.as_secs_f64() * 1000.0,
        "ms_total": t.total.as_secs_f64() * 1000.0,
        "heap_analyze": t.analyze.heap_peak,
        "heap_encode": t.encode.heap_peak,
        "heap_verify": t.verify.heap_peak,
        "rss_after_analyze": t.analyze.rss_after,
        "rss_after_encode": t.encode.rss_after,
        "rss_after_verify": t.verify.rss_after,
        "peak_rss": blad_mem::rss_highwater(),
    })
    .to_string()
}

fn cmd_archive(
    inputs: &[PathBuf],
    output: Option<&Path>,
    effort: u8,
    dry_run: bool,
    stats: bool,
    json: bool,
) -> Result<()> {
    if output.is_some() && inputs.len() > 1 {
        bail!("--output takes a single input; with several files each is archived in place");
    }

    if dry_run {
        for input in inputs {
            let layout = blad_archive::plan(input)
                .with_context(|| format!("reading {}", input.display()))?;
            print_layout(input, &layout);
        }
        return Ok(());
    }

    let c = codec(effort);
    let mut t = table();
    // "ratio 0.5303" and "saved 47.0%" are the same fact twice. Keep the one people
    // actually want, and give it a bar so a column of files is scannable at a glance.
    t.set_header(vec![
        Cell::new("file").add_attribute(Attribute::Bold),
        Cell::new("original").add_attribute(Attribute::Bold),
        Cell::new("stored").add_attribute(Attribute::Bold),
        Cell::new("saved").add_attribute(Attribute::Bold),
        Cell::new("").add_attribute(Attribute::Bold),
    ]);

    let (mut total_in, mut total_out, mut failures) = (0u64, 0u64, 0usize);
    for input in inputs {
        let dst = output
            .map(PathBuf::from)
            .unwrap_or_else(|| archive_name(input));
        match blad_archive::archive(input, &dst, &c) {
            Ok(r) => {
                total_in += r.original_len;
                total_out += r.stored_len;
                if json {
                    println!("{}", json_line(input, &r, effort));
                } else {
                    let saved = 1.0 - r.ratio();
                    t.add_row(vec![
                        Cell::new(input.file_name().unwrap_or_default().to_string_lossy()),
                        dim(right(human(r.original_len))),
                        right(human(r.stored_len)),
                        green(right(format!("{:.1}%", saved * 100.0))),
                        green(Cell::new(bar(saved, 12))),
                    ]);
                    if stats {
                        print_stats(input, &r);
                    }
                }
            }
            Err(e) => {
                failures += 1;
                eprintln!("error: {}: {e}", input.display());
            }
        }
    }

    if total_in > 0 && !json {
        // A total row only says something new when there is more than one file.
        if inputs.len() > 1 {
            let saved = 1.0 - total_out as f64 / total_in as f64;
            t.add_row(vec![
                Cell::new("total").add_attribute(Attribute::Bold),
                dim(right(human(total_in))),
                right(human(total_out)).add_attribute(Attribute::Bold),
                green(right(format!("{:.1}%", saved * 100.0))).add_attribute(Attribute::Bold),
                green(Cell::new(bar(saved, 12))),
            ]);
        }
        println!("{t}");
        println!("  byte-exact reconstruction verified");
        if stats && inputs.len() > 1 {
            println!("  (peak RSS is process-wide; archive one file per run for per-file figures)");
        }
    }
    if failures > 0 {
        bail!("{failures} of {} file(s) failed", inputs.len());
    }
    Ok(())
}

fn cmd_verify(archives: &[PathBuf], quick: bool) -> Result<()> {
    let c = if quick { None } else { Some(codec(4)) };

    // Collect first, render second: the "holds" column is empty whenever the archive is
    // just <original>.blad, which is the default. A header with nothing under it is
    // worse than no column, so it is only emitted when some row needs it.
    struct Row {
        archive: String,
        holds: String,
        size: String,
        ok: bool,
        verdict: String,
    }
    let mut rows = Vec::with_capacity(archives.len());
    let mut failures = 0usize;

    for a in archives {
        let archive = a
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let outcome = match &c {
            Some(codec) => blad_archive::verify(a, codec),
            None => blad_archive::verify_quick(a),
        };
        match outcome {
            Ok(m) => {
                let derived = archive_name(Path::new(&m.original.name));
                let derived = derived.file_name().unwrap_or_default().to_string_lossy();
                rows.push(Row {
                    holds: if derived == archive {
                        String::new()
                    } else {
                        m.original.name.clone()
                    },
                    archive,
                    size: human(m.original.len),
                    ok: true,
                    verdict: if quick { "ok (quick)" } else { "ok (full)" }.into(),
                });
            }
            Err(e) => {
                failures += 1;
                eprintln!("error: {}: {e}", a.display());
                rows.push(Row {
                    archive,
                    holds: String::new(),
                    size: "\u{2014}".into(),
                    ok: false,
                    verdict: "FAILED".into(),
                });
            }
        }
    }

    let show_holds = rows.iter().any(|r| !r.holds.is_empty());
    let mut t = table();
    let mut header = vec![Cell::new("archive").add_attribute(Attribute::Bold)];
    if show_holds {
        header.push(Cell::new("holds").add_attribute(Attribute::Bold));
    }
    header.push(Cell::new("size").add_attribute(Attribute::Bold));
    header.push(Cell::new("result").add_attribute(Attribute::Bold));
    t.set_header(header);

    for r in &rows {
        let mut cells = vec![Cell::new(&r.archive)];
        if show_holds {
            cells.push(dim(Cell::new(&r.holds)));
        }
        cells.push(right(&r.size));
        cells.push(if r.ok {
            green(Cell::new(&r.verdict))
        } else {
            let c = Cell::new(&r.verdict).add_attribute(Attribute::Bold);
            if colour() {
                c.fg(Color::Red)
            } else {
                c
            }
        });
        t.add_row(cells);
    }
    println!("{t}");
    if quick && failures == 0 {
        println!("  stored bytes only; drop --quick to prove the decode path");
    }
    if failures > 0 {
        bail!("{failures} of {} archive(s) failed", archives.len());
    }
    Ok(())
}

fn cmd_thumb(archive: &Path, output: Option<&Path>) -> Result<()> {
    let jpeg = blad_archive::thumbnail(archive)
        .with_context(|| format!("reading {}", archive.display()))?;
    if jpeg.is_empty() {
        bail!(
            "{} has no embedded preview — the source had no RGB image to build one from",
            archive.display()
        );
    }

    match output {
        // `-` writes the JPEG to stdout so it can be piped straight into a viewer.
        Some(p) if p.as_os_str() == "-" => {
            use std::io::Write;
            std::io::stdout().write_all(&jpeg)?;
        }
        _ => {
            let dst = output.map(PathBuf::from).unwrap_or_else(|| {
                let mut p = archive.as_os_str().to_owned();
                p.push(".jpg");
                PathBuf::from(p)
            });
            std::fs::write(&dst, &jpeg)?;
            println!("{}  ({})", dst.display(), human(jpeg.len() as u64));
        }
    }
    Ok(())
}

fn cmd_restore(archive: &Path, output: Option<&Path>) -> Result<()> {
    let c = codec(4);
    let (m, _, _) = blad_archive::read_manifest(archive)?;
    let dst = output
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&m.original.name));

    if dst.exists() {
        bail!(
            "{} already exists; pass --output to write elsewhere",
            dst.display()
        );
    }
    blad_archive::restore(archive, &dst, &c)
        .with_context(|| format!("restoring {}", archive.display()))?;
    println!("{}  ({})", dst.display(), human(m.original.len));
    println!("  sha256 verified: {}", m.original.sha256);
    Ok(())
}

// ---------------------------------------------------------------------------
// exif
// ---------------------------------------------------------------------------

/// Glyphs marking what a row *is*. Colour repeats the same information for people who
/// read colour faster, but the glyph carries it alone — the output stays legible piped
/// to a file, on a monochrome terminal, or with NO_COLOR set.
///
/// **Every glyph here is East-Asian-Width `Narrow`.** Most decorative Unicode is
/// classed `Ambiguous`: terminals configured for CJK render those two columns wide
/// while `unicode-width` — which comfy-table uses to size columns — counts them as one.
/// The table then shifts right by a column on exactly the rows that have a marker,
/// which looks like a bug in the data rather than in the font. Narrow glyphs are one
/// column everywhere, so alignment cannot depend on the reader's terminal.
fn group_glyph(kind: blad_container::ifd::IfdKind) -> (&'static str, Color) {
    use blad_container::ifd::IfdKind::*;
    match kind {
        Main(_) => ("\u{22a1}", Color::Cyan), // ⊡ squared dot — the main directory
        Sub(_) => ("\u{27d0}", Color::Magenta), // ⟐ diamond dot — nested under it
        Exif => ("\u{2731}", Color::Blue),    // ✱
        Gps => ("\u{2316}", Color::Yellow),   // ⌖ crosshair
        Interop => ("\u{21c4}", Color::DarkGrey), // ⇄ exchange
    }
}

/// Only exceptions get a marker.
///
/// An earlier version also marked every value that had been given units, which is the
/// common case — a column of glyphs down the whole table carries no information and
/// reads as clutter. What is worth flagging is what you would otherwise misread.
fn field_glyph(f: &blad_meta::Field) -> (&'static str, Option<Color>) {
    use blad_meta::Kind::*;
    match f.kind {
        _ if f.redacted => ("\u{25ab}", Some(Color::DarkGrey)), // ▫ hollow: value removed
        _ if f.name.is_none() => ("?", Some(Color::DarkGrey)),
        Sensitive => ("!", Some(Color::Yellow)),
        Opaque => ("\u{25aa}", Some(Color::DarkGrey)), // ▪ solid: present, not decoded
        Pointer => ("\u{21b3}", Some(Color::Blue)),    // ↳ leads to another directory
        Matrix3x3 => ("\u{229e}", Some(Color::Magenta)), // ⊞
        _ => (" ", None),
    }
}

fn paint(c: Cell, col: Color) -> Cell {
    if colour() {
        c.fg(col)
    } else {
        c
    }
}

/// A 3x3 colour matrix printed as three rows.
///
/// Nine rationals on one line is technically the same information and practically
/// unreadable — and these are the numbers that define what the camera's colours *mean*,
/// so they are worth the four extra lines.
fn matrix_rows(v: &blad_meta::Value) -> Option<Vec<String>> {
    let blad_meta::Value::Rational(r) = v else {
        return None;
    };
    if r.len() != 9 {
        return None;
    }
    let n: Vec<f64> = r
        .iter()
        .map(|&(a, b)| if b == 0 { 0.0 } else { a as f64 / b as f64 })
        .collect();
    Some(
        n.chunks(3)
            .map(|row| {
                row.iter()
                    .map(|x| format!("{x:>9.6}"))
                    .collect::<Vec<_>>()
                    .join("  ")
            })
            .collect(),
    )
}

#[allow(clippy::too_many_arguments)]
fn cmd_exif(
    files: &[PathBuf],
    groups: &[String],
    tags: &[String],
    all: bool,
    redact: bool,
    raw: bool,
    full: bool,
    offsets: bool,
    json: bool,
) -> Result<()> {
    // Asking for specific tags, directories, offsets or unnamed entries is a request for
    // the detailed view; making the user also pass --full would be pedantry.
    let full = full || offsets || all || !groups.is_empty() || !tags.is_empty();
    let opts = blad_meta::Options {
        all,
        redact,
        raw,
        groups: groups.to_vec(),
        tags: tags.to_vec(),
    };

    for (i, path) in files.iter().enumerate() {
        let report = read_metadata(path, &opts)
            .with_context(|| format!("reading metadata from {}", path.display()))?;

        if json {
            println!("{}", exif_json(path, &report));
            continue;
        }

        if i > 0 {
            println!();
        }
        if full {
            print_exif(path, &report, offsets, true);
        } else {
            print_summary(path, &report);
        }
    }
    Ok(())
}

/// Monochrome glyph and colour for a facet.
///
/// Text glyphs rather than emoji: emoji carry their own colour and a typeface the
/// terminal chose, which fights the palette and looks foreign beside the rest of the
/// output. A monochrome mark takes its colour from the same ANSI palette as everything
/// else, and goes plain under `NO_COLOR` instead of staying stubbornly bright.
///
/// All are East-Asian-Width `Narrow`, so they occupy one column on every terminal —
/// unlike the Ambiguous geometric shapes that misaligned tables earlier.
fn facet_icon(f: blad_meta::summary::Facet) -> (&'static str, Color) {
    use blad_meta::summary::Facet::*;
    match f {
        Camera => ("\u{233E}", Color::Cyan),     // body
        Lens => ("\u{2300}", Color::Cyan),       // diameter
        Shutter => ("\u{25F7}", Color::Green),   // elapsed time
        Aperture => ("\u{229B}", Color::Green),  // iris
        Iso => ("\u{229A}", Color::Green),       // sensitivity
        Flash => ("\u{2301}", Color::Yellow),    // arc
        Taken => ("\u{29D6}", Color::Blue),      // hourglass
        Where => ("\u{2316}", Color::Yellow),    // crosshair
        Format => ("\u{2394}", Color::DarkGrey), // container
        Image => ("\u{25AD}", Color::Magenta),   // frame
        Aspect => ("\u{2B13}", Color::Magenta),  // proportion
        Depth => ("\u{25EB}", Color::Magenta),   // bit planes
        Dynamic => ("\u{25E8}", Color::Magenta), // range
        Colour => ("\u{2726}", Color::Magenta),
        Sensor => ("\u{2317}", Color::Blue), // photosite grid
        Orientation => ("\u{2349}", Color::Blue), // transpose
        Software => ("\u{2318}", Color::DarkGrey),
        Author => ("\u{235F}", Color::Yellow),
    }
}

/// The default view: a dozen facts, named in plain words.
fn print_summary(path: &Path, report: &blad_meta::Report) {
    let items = blad_meta::summary::summarise(report);
    let name = path.file_name().unwrap_or_default().to_string_lossy();

    // For an archive, file_len is the *original's* length — the coordinate space the
    // directories describe. Printing only that would claim 293 MB for a file that
    // occupies 167 MB on disk, so both are named.
    let size = match report.archived {
        Some(on_disk) => format!(
            "{} archive  \u{00b7}  {} original",
            human(on_disk),
            human(report.file_len)
        ),
        None => human(report.file_len),
    };
    println!(
        "{}  {}",
        bold(&format!("\u{25B8} {name}")),
        faint(&format!("{size}  \u{00b7}  {} tags", report.field_count()))
    );

    if items.is_empty() {
        println!("{}", faint("  no recognised metadata \u{2014} try --full"));
        return;
    }

    println!();
    let width = items
        .iter()
        .map(|i| i.facet.key().chars().count())
        .max()
        .unwrap_or(8);

    for it in &items {
        let (icon, col) = facet_icon(it.facet);
        let key = colourise(&format!("{:<width$}", it.facet.key(), width = width), col);
        let value = if it.sensitive && !it.value.starts_with('<') {
            colourise(&it.value, Color::Yellow)
        } else if it.value.starts_with('<') {
            faint(&it.value)
        } else {
            it.value.clone()
        };
        println!("  {}  {key}  {value}", colourise(icon, col));
    }
}

fn bold(s: &str) -> String {
    if colour() {
        format!("\u{1b}[1m{s}\u{1b}[0m")
    } else {
        s.to_string()
    }
}

fn faint(s: &str) -> String {
    if colour() {
        format!("\u{1b}[2m{s}\u{1b}[0m")
    } else {
        s.to_string()
    }
}

/// Metadata from a plain TIFF/raw, a JPEG's APP1 block, or a blad archive.
///
/// The archive case is the interesting one: metadata lives entirely in verbatim
/// segments, so it can be read straight out of the archive in original-file
/// coordinates. `blad exif photo.blad.3FR` costs a few seeks rather than a full
/// restore — you can inspect an archived library without unpacking it.
fn read_metadata(path: &Path, opts: &blad_meta::Options) -> Result<blad_meta::Report> {
    if blad_archive::is_archive(path) {
        let on_disk = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let mut sk = blad_archive::skeleton(path)?;
        let len = sk.original_len();
        let mut report = blad_meta::read_from(&mut sk, len, opts)?;
        report.archived = Some(on_disk);
        return Ok(report);
    }
    Ok(blad_meta::read(path, opts)?)
}

fn print_exif(path: &Path, report: &blad_meta::Report, offsets: bool, _header: bool) {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let order = if report.little_endian {
        "little-endian"
    } else {
        "big-endian"
    };

    let title = Cell::new(format!("▸ {name}"));
    println!(
        "{}  {}",
        if colour() {
            format!("\u{1b}[1m▸ {name}\u{1b}[0m")
        } else {
            format!("▸ {name}")
        },
        if colour() {
            format!(
                "\u{1b}[2m{}  ·  {}  ·  {} tags\u{1b}[0m",
                human(report.file_len),
                order,
                report.field_count()
            )
        } else {
            format!(
                "{}  ·  {}  ·  {} tags",
                human(report.file_len),
                order,
                report.field_count()
            )
        }
    );
    let _ = title;

    if report.groups.is_empty() {
        let msg = "  no tags matched — try --all, or widen --tag/--group";
        println!(
            "{}",
            if colour() {
                format!("\u{1b}[2m{msg}\u{1b}[0m")
            } else {
                msg.to_string()
            }
        );
        return;
    }

    for g in &report.groups {
        let (glyph, col) = group_glyph(g.kind);
        println!();
        let heading = format!("{glyph} {}", g.label);
        if colour() {
            println!(
                "\u{1b}[1m{}\u{1b}[0m \u{1b}[2m@ 0x{:X}  ({} tags)\u{1b}[0m",
                colourise(&heading, col),
                g.offset,
                g.fields.len()
            );
        } else {
            println!("{heading}  @ 0x{:X}  ({} tags)", g.offset, g.fields.len());
        }

        let mut t = table();
        // Content-sized, not terminal-sized. A metadata table stretched to 200 columns
        // puts the value half a screen from its name.
        t.set_content_arrangement(ContentArrangement::Disabled);
        let mut header = vec![
            dim(Cell::new("")),
            dim(Cell::new("tag")),
            dim(Cell::new("value")),
        ];
        if offsets {
            header.push(dim(Cell::new("type")));
            header.push(dim(Cell::new("offset")));
        }
        t.set_header(header);

        for f in &g.fields {
            let (fg, fcol) = field_glyph(f);
            let marker = match fcol {
                Some(c) => paint(Cell::new(fg), c),
                None => Cell::new(fg),
            };

            let label = match f.name {
                Some(_) => Cell::new(f.label()),
                None => dim(Cell::new(f.label())),
            };

            // Matrices get their own multi-line cell; comfy-table keeps the alignment.
            let value_cell = match matrix_rows(&f.value) {
                Some(rows) if f.kind == blad_meta::Kind::Matrix3x3 => {
                    paint(Cell::new(rows.join("\n")), Color::Magenta)
                }
                _ => match f.kind {
                    blad_meta::Kind::Opaque => dim(Cell::new(&f.display)),
                    _ if f.redacted => dim(Cell::new(&f.display)),
                    _ if matches!(f.value, blad_meta::Value::Unreadable(_)) => {
                        paint(Cell::new(&f.display), Color::Red)
                    }
                    blad_meta::Kind::Sensitive => paint(Cell::new(&f.display), Color::Yellow),
                    _ => Cell::new(&f.display),
                },
            };

            let mut row = vec![marker, label, value_cell];
            if offsets {
                row.push(dim(Cell::new(f.type_note())));
                row.push(dim(right(format!("0x{:X}", f.offset))));
            }
            t.add_row(row);
        }
        println!("{t}");
    }

    if report.unknown_count() > 0 {
        let n = report.unknown_count();
        let msg = format!("  {n} tag(s) shown by number — no dictionary entry");
        println!(
            "{}",
            if colour() {
                format!("\u{1b}[2m{msg}\u{1b}[0m")
            } else {
                msg
            }
        );
    }
}

fn colourise(s: &str, c: Color) -> String {
    if !colour() {
        return s.to_string();
    }
    let code = match c {
        Color::Cyan => 36,
        Color::Magenta => 35,
        Color::Blue => 34,
        Color::Yellow => 33,
        Color::Green => 32,
        Color::Red => 31,
        _ => 90,
    };
    format!("\u{1b}[{code}m{s}\u{1b}[0m")
}

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// Exact values for machine consumers.
///
/// The display string rounds — `AsShotNeutral` shows 0.447106 where the file holds
/// 4471066599/10000000000. Rounding is right for a table and wrong for anything
/// computing with the number, and camera characterization is exactly that: these values
/// feed a colour matrix. So JSON carries the file's own representation alongside.
fn json_raw(v: &blad_meta::Value) -> Option<String> {
    const CAP: usize = 64;
    match v {
        blad_meta::Value::Rational(r) if r.len() <= CAP => Some(format!(
            "[{}]",
            r.iter()
                .map(|(n, d)| format!("[{n},{d}]"))
                .collect::<Vec<_>>()
                .join(",")
        )),
        blad_meta::Value::Uint(x) if x.len() <= CAP => Some(format!(
            "[{}]",
            x.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )),
        blad_meta::Value::Int(x) if x.len() <= CAP => Some(format!(
            "[{}]",
            x.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )),
        blad_meta::Value::Real(x) if x.len() <= CAP => Some(format!(
            "[{}]",
            x.iter()
                .map(|v| format!("{v}"))
                .collect::<Vec<_>>()
                .join(",")
        )),
        _ => None,
    }
}

/// One object per file. Offsets are included because blad already knows them and
/// nothing else reporting Exif does — it is what lets you check a claim against bytes.
fn exif_json(path: &Path, r: &blad_meta::Report) -> String {
    let mut s = String::from("{");
    s.push_str(&format!(
        "\"file\":\"{}\",",
        json_escape(&path.display().to_string())
    ));
    s.push_str(&format!("\"file_len\":{},", r.file_len));
    s.push_str(&format!("\"little_endian\":{},", r.little_endian));
    s.push_str("\"directories\":[");
    for (gi, g) in r.groups.iter().enumerate() {
        if gi > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"kind\":\"{}\",\"offset\":{},\"tags\":[",
            json_escape(&g.label),
            g.offset
        ));
        for (fi, f) in g.fields.iter().enumerate() {
            if fi > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"id\":{},\"name\":{},\"type\":\"{}\",\"count\":{},\"offset\":{},\"value\":\"{}\"{}}}",
                f.tag,
                match f.name {
                    Some(n) => format!("\"{n}\""),
                    None => "null".into(),
                },
                blad_meta::value::type_name(f.dtype),
                f.count,
                f.offset,
                json_escape(&f.display),
                {
                    let mut extra = String::new();
                    if !f.redacted {
                        if let Some(raw) = json_raw(&f.value) {
                            extra.push_str(&format!(",\"raw\":{raw}"));
                        }
                    }
                    if f.redacted {
                        extra.push_str(",\"redacted\":true");
                    }
                    extra
                }
            ));
        }
        s.push_str("]}");
    }
    s.push_str("]}");
    s
}
