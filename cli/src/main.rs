// axmzip CLI v5.2 — single files AND folders
//
// Usage:
//   axmzip compress   <file_or_folder> [output.axm] [--quality N] [--channels N]
//   axmzip decompress <input.axm>      [output_dir]
//   axmzip info       <input.axm>
//   axmzip list       <input.axm>
//   axmzip bench      <file>

use std::{fs, path::{Path, PathBuf}, process, time::Instant};
use axmzip_core::{self as core, ArchiveEntry};

fn usage() {
    eprintln!(r#"
Axmzip v0.5.2 — Axiom-Based Binary Compression

USAGE:
  axmzip compress   <file_or_folder> [output.axm]  [--quality N] [--channels N]
  axmzip decompress <input.axm>      [output_dir]
  axmzip info       <input.axm>
  axmzip list       <input.axm>
  axmzip bench      <file>

OPTIONS:
  --quality N    0-100  (100=lossless default, <100=lossy)
  --channels N   1=mono (default), 3=RGB, 4=RGBA

EXAMPLES:
  axmzip compress  my_project/           # compress whole folder
  axmzip compress  photo.raw             # compress single file
  axmzip compress  noisy.pcm --quality 90
  axmzip decompress my_project.axm       # restores folder
  axmzip list      my_project.axm
"#);
}

fn human(n: usize) -> String {
    if n < 1_024          { format!("{n} B") }
    else if n < 1_048_576 { format!("{:.1} KB", n as f64/1_024.0) }
    else                  { format!("{:.2} MB", n as f64/1_048_576.0) }
}

// ── Walk a directory recursively, collecting ArchiveEntry items ──

fn walk_dir(root: &Path) -> Result<Vec<ArchiveEntry>, String> {
    let mut entries = Vec::new();
    walk_dir_inner(root, root, &mut entries)?;
    // Sort for deterministic archives
    entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(entries)
}

fn walk_dir_inner(
    root:    &Path,
    current: &Path,
    out:     &mut Vec<ArchiveEntry>,
) -> Result<(), String> {
    let read = fs::read_dir(current)
        .map_err(|e| format!("Cannot read dir {}: {e}", current.display()))?;

    for entry in read {
        let entry = entry.map_err(|e| e.to_string())?;
        let path  = entry.path();
        let meta  = fs::metadata(&path).map_err(|e| e.to_string())?;

        if meta.is_dir() {
            walk_dir_inner(root, &path, out)?;
        } else if meta.is_file() {
            // Build relative path with forward slashes
            let rel = path.strip_prefix(root)
                .map_err(|_| format!("Path {} not under root {}", path.display(), root.display()))?;
            let rel_path: String = rel.components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("/");

            let data = fs::read(&path)
                .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;

            out.push(ArchiveEntry { rel_path, data });
        }
    }
    Ok(())
}

// ── compress ─────────────────────────────────────────────────────

fn cmd_compress(args: &[String]) {
    if args.is_empty() { usage(); process::exit(1); }

    let input: PathBuf = args[0].clone().into();
    let mut quality  = 100u8;
    let mut channels = 1u8;
    let mut output: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--quality"  => { i += 1; quality  = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(100); }
            "--channels" => { i += 1; channels = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(1); }
            _            => { output = Some(args[i].clone().into()); }
        }
        i += 1;
    }

    let meta = fs::metadata(&input).unwrap_or_else(|e| {
        eprintln!("Cannot access {}: {e}", input.display()); process::exit(1);
    });

    if meta.is_dir() {
        // ── FOLDER MODE ──────────────────────────────────────────
        let root_name = input.file_name()
            .and_then(|n| n.to_str()).unwrap_or("archive");
        let output = output.unwrap_or_else(|| {
            input.parent().unwrap_or(Path::new(".")).join(format!("{root_name}.axm"))
        });

        println!("Scanning {} …", input.display());
        let entries = walk_dir(&input).unwrap_or_else(|e| {
            eprintln!("Walk error: {e}"); process::exit(1);
        });
        let total: usize = entries.iter().map(|e| e.data.len()).sum();
        println!("  {} files  ({})", entries.len(), human(total));
        println!("Compressing → {} …", output.display());

        let (blob, stats) = core::compress_archive(root_name, &entries, quality, None);
        fs::write(&output, &blob).unwrap_or_else(|e| {
            eprintln!("Cannot write {}: {e}", output.display()); process::exit(1);
        });

        let arrow = if stats.ratio_pct >= 0.0 { "▼" } else { "▲" };
        println!("  Files    : {}", stats.file_count);
        println!("  Original : {}", human(stats.original_bytes));
        println!("  Archive  : {} ({})", output.display(), human(stats.compressed_bytes));
        println!("  Ratio    : {arrow}{:.2}%", stats.ratio_pct.abs());
        println!("  Time     : {} ms", stats.elapsed_ms);

    } else {
        // ── SINGLE FILE MODE ─────────────────────────────────────
        let output = output.unwrap_or_else(|| input.with_extension("axm"));
        let data   = fs::read(&input).unwrap_or_else(|e| {
            eprintln!("Cannot read {}: {e}", input.display()); process::exit(1);
        });
        let fname  = input.file_name().and_then(|n| n.to_str()).unwrap_or("file");

        println!("Compressing {} ({}) …", input.display(), human(data.len()));
        let (blob, stats) = core::compress(&data, quality, channels, fname, None);
        fs::write(&output, &blob).unwrap_or_else(|e| {
            eprintln!("Cannot write {}: {e}", output.display()); process::exit(1);
        });

        let arrow = if stats.ratio_pct >= 0.0 { "▼" } else { "▲" };
        println!("  Input  : {}", human(stats.original_bytes));
        println!("  Output : {} ({})", output.display(), human(stats.compressed_bytes));
        println!("  Ratio  : {arrow}{:.2}%", stats.ratio_pct.abs());
        println!("  Mode   : {}", stats.mode);
        if !stats.lossless {
            println!("  PSNR   : {:.1} dB  max_err ±{}", stats.psnr_db, stats.max_error);
        }
        println!("  Time   : {} ms", stats.elapsed_ms);
    }
}

// ── decompress ───────────────────────────────────────────────────

fn cmd_decompress(args: &[String]) {
    if args.is_empty() { usage(); process::exit(1); }

    let input: PathBuf = args[0].clone().into();
    let explicit_out: Option<PathBuf> = args.get(1).map(|s| PathBuf::from(s));

    let blob = fs::read(&input).unwrap_or_else(|e| {
        eprintln!("Cannot read {}: {e}", input.display()); process::exit(1);
    });
    let t0 = Instant::now();

    if core::is_axmzip_archive(&blob) {
        // ── ARCHIVE DECOMPRESS ───────────────────────────────────
        println!("Decompressing archive {} ({}) …", input.display(), human(blob.len()));
        let (root_name, entries) = core::decompress_archive(&blob).unwrap_or_else(|e| {
            eprintln!("Decompress failed: {e}"); process::exit(1);
        });

        // Output directory
        let out_dir = explicit_out.unwrap_or_else(|| {
            let parent = input.parent().unwrap_or(Path::new("."));
            parent.join(&root_name)
        });

        // Safety: refuse to write outside out_dir
        for entry in &entries {
            if entry.rel_path.contains("..") {
                eprintln!("Refusing path with '..': {}", entry.rel_path);
                process::exit(1);
            }
        }

        let mut total_bytes = 0usize;
        for entry in &entries {
            // Convert "/" path separator to OS separator
            let rel: PathBuf = entry.rel_path.split('/').collect();
            let dest = out_dir.join(&rel);

            // Create parent dirs
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).unwrap_or_else(|e| {
                    eprintln!("Cannot create dir {}: {e}", parent.display()); process::exit(1);
                });
            }

            fs::write(&dest, &entry.data).unwrap_or_else(|e| {
                eprintln!("Cannot write {}: {e}", dest.display()); process::exit(1);
            });
            total_bytes += entry.data.len();
            println!("  + {}", entry.rel_path);
        }

        println!("Restored {} files ({}) in {}",
                 entries.len(), human(total_bytes), out_dir.display());
        println!("Time : {} ms", t0.elapsed().as_millis());

    } else {
        // ── SINGLE-FILE DECOMPRESS ───────────────────────────────
        println!("Decompressing {} ({}) …", input.display(), human(blob.len()));
        let result = core::decompress(&blob).unwrap_or_else(|e| {
            eprintln!("Decompress failed: {e}"); process::exit(1);
        });

        let output = if let Some(p) = explicit_out {
            p
        } else if let Some(ref orig) = result.original_filename {
            input.parent().unwrap_or(Path::new(".")).join(orig)
        } else if input.extension().and_then(|e| e.to_str()) == Some("axm") {
            input.with_extension("")
        } else {
            input.with_extension("decoded")
        };

        let output = if output == input {
            let stem = output.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
            let ext  = output.extension().and_then(|e| e.to_str()).unwrap_or("");
            let name = if ext.is_empty() { format!("{stem}_restored") }
                       else { format!("{stem}_restored.{ext}") };
            output.with_file_name(name)
        } else { output };

        fs::write(&output, &result.data).unwrap_or_else(|e| {
            eprintln!("Cannot write {}: {e}", output.display()); process::exit(1);
        });
        println!("  Output : {} ({})", output.display(), human(result.data.len()));
        println!("  Time   : {} ms", t0.elapsed().as_millis());
        if let Some(ref f) = result.original_filename {
            println!("  Restored filename: {f}");
        }
        println!("Done checksum verified");
    }
}

// ── info ─────────────────────────────────────────────────────────

fn cmd_info(args: &[String]) {
    if args.is_empty() { usage(); process::exit(1); }
    let blob = fs::read(&args[0]).unwrap_or_else(|e| {
        eprintln!("Error: {e}"); process::exit(1);
    });
    println!("File: {}  ({})", args[0], human(blob.len()));
    if core::is_axmzip_archive(&blob) {
        match core::probe_archive(&blob) {
            Some((root, count)) => println!("  Type   : archive\n  Root   : {root}\n  Files  : {count}"),
            None                => println!("  Type   : archive (probe failed)"),
        }
    } else {
        match core::probe(&blob) {
            Some(mode) => println!("  Type : single-file\n  Mode : {mode}"),
            None       => { eprintln!("  Not a valid axmzip file"); process::exit(1); }
        }
    }
}

// ── list ─────────────────────────────────────────────────────────

fn cmd_list(args: &[String]) {
    if args.is_empty() { usage(); process::exit(1); }
    let blob = fs::read(&args[0]).unwrap_or_else(|e| {
        eprintln!("Error: {e}"); process::exit(1);
    });
    if !core::is_axmzip_archive(&blob) {
        eprintln!("Not an axmzip archive (use 'info' for single files)");
        process::exit(1);
    }
    println!("Listing {} …\n", args[0]);
    match core::decompress_archive(&blob) {
        Ok((root, entries)) => {
            println!("  Archive root: {root}");
            println!("  {:<6} {}", "Size", "Path");
            println!("  {}", "─".repeat(48));
            let total: usize = entries.iter().map(|e| e.data.len()).sum();
            for e in &entries {
                println!("  {:>8}  {}", human(e.data.len()), e.rel_path);
            }
            println!("\n  {} files  total {}", entries.len(), human(total));
        }
        Err(e) => { eprintln!("Error: {e}"); process::exit(1); }
    }
}

// ── bench ────────────────────────────────────────────────────────

fn cmd_bench(args: &[String]) {
    if args.is_empty() { usage(); process::exit(1); }
    let data = fs::read(&args[0]).unwrap_or_else(|e| {
        eprintln!("Error: {e}"); process::exit(1);
    });
    println!("Benchmark: {}  ({})\n", args[0], human(data.len()));
    println!("  {:<14} {:>10} {:>8} {:>10} {:>8}", "Quality", "Size", "Ratio%", "PSNR", "Mode");
    println!("  {}", "─".repeat(58));
    for q in [100u8, 95, 90, 80, 75, 50, 25] {
        let (_, s) = core::compress(&data, q, 1, "bench", None);
        let arr    = if s.ratio_pct >= 0.0 { "▼" } else { "▲" };
        let psnr   = if s.lossless { "inf".into() } else { format!("{:.1}", s.psnr_db) };
        let ql     = if q == 100 { "lossless".into() } else { format!("q={q}") };
        println!("  {:<14} {:>10} {:>7.1}%{} {:>10} {}", ql, human(s.compressed_bytes),
                 s.ratio_pct.abs(), arr, psnr, s.mode);
    }
}

// ── main ─────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { usage(); return; }
    match args[1].as_str() {
        "compress"   | "c" => cmd_compress(&args[2..]),
        "decompress" | "d" => cmd_decompress(&args[2..]),
        "info"       | "i" => cmd_info(&args[2..]),
        "list"       | "l" => cmd_list(&args[2..]),
        "bench"      | "b" => cmd_bench(&args[2..]),
        _                  => { usage(); process::exit(1); }
    }
}
