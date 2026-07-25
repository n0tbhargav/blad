//! blad CLI.
//!
//! Deliberately thin: every command is argument parsing, a library call, and formatting.
//! Logic belongs in the crates, because Phase 2 needs the same components and
//! tool-specific code would have to be rewritten.

use anyhow::{bail, Context, Result};
use blad_codec::Jxl;
use blad_container::{Layout, SegmentKind};
use clap::{Parser, Subcommand};
use comfy_table::{presets, Attribute, Cell, CellAlignment, ContentArrangement, Table, TableComponent};
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

/// Borderless, with a single rule under the header.
///
/// A line between every row turns a five-row table into visual noise; the header rule
/// is the only separator that carries information.
fn table() -> Table {
    let mut t = Table::new();
    t.load_preset(presets::NOTHING)
        .set_style(TableComponent::HeaderLines, '─')
        .set_content_arrangement(ContentArrangement::Dynamic);
    t
}

fn right(s: impl ToString) -> Cell {
    Cell::new(s).set_alignment(CellAlignment::Right)
}

fn throughput(bytes: u64, d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    if s <= 0.0 {
        return "-".into();
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

    let mut tb = table();
    tb.set_header(vec![
        Cell::new("phase").add_attribute(Attribute::Bold),
        Cell::new("time").add_attribute(Attribute::Bold),
        Cell::new("share").add_attribute(Attribute::Bold),
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
            right(throughput(r.original_len, p.time)),
            right(human(p.heap_peak)),
            right(human(p.rss_after)),
        ]);
    }
    tb.add_row(vec![
        Cell::new("total").add_attribute(Attribute::Bold),
        right(ms(t.total)).add_attribute(Attribute::Bold),
        right(""),
        right(throughput(r.original_len, t.total)),
        right(""),
        right(human(blad_mem::rss_highwater())).add_attribute(Attribute::Bold),
    ]);

    println!("{}", input.display());
    println!("{tb}");
    println!(
        "  payload {} · skeleton {}",
        human(r.payload_len),
        human(r.skeleton_len),
    );
    // The heap counter sees our allocations; RSS sees everything. The gap is temp-file
    // pages, mapped libraries, and allocator overhead — currently dominated by the
    // shell-out codec's netpbm round trip.
    let peak_heap = t.encode.heap_peak.max(t.verify.heap_peak);
    let rss = blad_mem::rss_highwater();
    println!(
        "  heap {} · RSS {} · non-heap {} ({:.0}% of RSS)",
        human(peak_heap),
        human(rss),
        human(rss.saturating_sub(peak_heap)),
        100.0 * rss.saturating_sub(peak_heap) as f64 / rss.max(1) as f64,
    );
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
    t.set_header(vec![
        Cell::new("file").add_attribute(Attribute::Bold),
        Cell::new("original").add_attribute(Attribute::Bold),
        Cell::new("stored").add_attribute(Attribute::Bold),
        Cell::new("ratio").add_attribute(Attribute::Bold),
        Cell::new("saved").add_attribute(Attribute::Bold),
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
                    t.add_row(vec![
                        Cell::new(dst.file_name().unwrap_or_default().to_string_lossy()),
                        right(human(r.original_len)),
                        right(human(r.stored_len)),
                        right(format!("{:.4}", r.ratio())),
                        right(format!("{:.1}%", (1.0 - r.ratio()) * 100.0)),
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
        println!("{t}");
        let ratio = total_out as f64 / total_in as f64;
        println!(
            "  {} → {}   x = {ratio:.4}   ({:.1}% saved)   byte-exact reconstruction verified",
            human(total_in),
            human(total_out),
            (1.0 - ratio) * 100.0
        );
        if stats && inputs.len() > 1 {
            println!("  note: peak RSS is a process-wide high-water mark; for per-file");
            println!("  figures archive one file per invocation.");
        }
    }
    if failures > 0 {
        bail!("{failures} of {} file(s) failed", inputs.len());
    }
    Ok(())
}

fn cmd_verify(archives: &[PathBuf], quick: bool) -> Result<()> {
    let c = if quick { None } else { Some(codec(4)) };
    let mut t = table();
    t.set_header(vec![
        Cell::new("archive").add_attribute(Attribute::Bold),
        Cell::new("original").add_attribute(Attribute::Bold),
        Cell::new("size").add_attribute(Attribute::Bold),
        Cell::new("result").add_attribute(Attribute::Bold),
    ]);

    let mut failures = 0usize;
    for a in archives {
        let outcome = match &c {
            Some(codec) => blad_archive::verify(a, codec),
            None => blad_archive::verify_quick(a),
        };
        match outcome {
            Ok(m) => t.add_row(vec![
                Cell::new(a.file_name().unwrap_or_default().to_string_lossy()),
                Cell::new(&m.original.name),
                right(human(m.original.len)),
                Cell::new(if quick { "ok (quick)" } else { "ok (full)" }),
            ]),
            Err(e) => {
                failures += 1;
                t.add_row(vec![
                    Cell::new(a.file_name().unwrap_or_default().to_string_lossy()),
                    Cell::new("-"),
                    right("-"),
                    Cell::new("FAILED"),
                ]);
                eprintln!("error: {}: {e}", a.display());
                &mut t
            }
        };
    }
    println!("{t}");
    if quick {
        println!("  quick mode checks stored bytes only; run without --quick to prove decode");
    }
    if failures > 0 {
        bail!("{failures} of {} archive(s) failed", archives.len());
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
