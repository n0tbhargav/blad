//! blad CLI.
//!
//! Deliberately thin: every command is argument parsing, a library call, and formatting.
//! Logic belongs in the crates, because Phase 2 needs the same components and
//! tool-specific code would have to be rewritten.

use anyhow::{bail, Context, Result};
use blad_codec::Jxl;
use blad_container::{Layout, SegmentKind};
use clap::{Parser, Subcommand};
use comfy_table::{presets, Attribute, Cell, CellAlignment, Color, ContentArrangement, Table, TableComponent};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// Installed so heap usage can be attributed per phase. Counters are relaxed atomics;
/// the cost on the allocation path is negligible and the visibility is worth it for a
/// project whose claims are all measurements.
#[global_allocator]
static ALLOC: blad_mem::Tracking<std::alloc::System> = blad_mem::Tracking(std::alloc::System);

#[derive(Parser)]
#[command(name = "blad", version, about = "Colour-correct, memory-safe image tooling")]
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
    /// Extract an archive's embedded preview as a JPEG.
    Thumb {
        archive: PathBuf,
        /// Output path, or `-` for stdout (default: <archive>.jpg).
        #[arg(short, long)]
        output: Option<PathBuf>,
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
    const PARTIAL: [char; 8] = ['\u{258f}', '\u{258e}', '\u{258d}', '\u{258c}',
                                '\u{258b}', '\u{258a}', '\u{2589}', '\u{2588}'];
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
    if colour() { c.fg(Color::Green) } else { c }
}

fn dim(c: Cell) -> Cell {
    if colour() { c.add_attribute(Attribute::Dim) } else { c }
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

fn default_output(input: &Path) -> PathBuf {
    let mut p = input.as_os_str().to_owned();
    p.push(".blad");
    PathBuf::from(p)
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
        println!("  orientation {} (pixels are not stored upright)", layout.orientation);
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
        format!("{:.0}%", 100.0 * d.as_secs_f64() / t.total.as_secs_f64().max(1e-9))
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
        let dst = output.map(PathBuf::from).unwrap_or_else(|| default_output(input));
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
        let archive = a.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let outcome = match &c {
            Some(codec) => blad_archive::verify(a, codec),
            None => blad_archive::verify_quick(a),
        };
        match outcome {
            Ok(m) => {
                let derived = format!("{}.blad", m.original.name);
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
