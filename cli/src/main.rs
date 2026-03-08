// axmzip CLI
//
// Usage:
//   axmzip compress   <input> <output.axm> [--quality 100] [--channels 1]
//   axmzip decompress <input.axm> <output>
//   axmzip info       <input.axm>
//   axmzip bench      <input>           (try all quality levels, print table)

use std::{fs, process, time::Instant, path::Path};
use axmzip_core as core;

fn print_usage() {
    eprintln!("
Axmzip v0.5 — Axiom-Based Binary Compression

USAGE:
  axmzip compress   <input> <output>  [--quality N] [--channels N]
  axmzip decompress <input.axm> <output>
  axmzip info       <input.axm>
  axmzip bench      <input>

OPTIONS:
  --quality N    Compression quality (default: 100 = lossless, 0-99 = lossy)
  --channels N   Channel count hint  (default: 1 | use 3 for RGB, 4 for RGBA)

EXAMPLES:
  axmzip compress  photo.raw photo.axm --channels 3
  axmzip compress  audio.pcm audio.axm --quality 90
  axmzip decompress photo.axm photo_out.raw
  axmzip info      photo.axm
  axmzip bench     sensor_data.bin
");
}

fn human_bytes(n: usize) -> String {
    if n < 1024 { format!("{n} B") }
    else if n < 1024*1024 { format!("{:.1} KB", n as f64/1024.0) }
    else { format!("{:.2} MB", n as f64/1024.0/1024.0) }
}

fn cmd_compress(args: &[String]) {
    if args.len() < 2 { print_usage(); process::exit(1); }
    let input  = &args[0];
    let output = &args[1];

    let mut quality  = 100u8;
    let mut channels = 1u8;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--quality"  => { quality  = args.get(i+1).and_then(|s| s.parse().ok()).unwrap_or(100); i+=2; }
            "--channels" => { channels = args.get(i+1).and_then(|s| s.parse().ok()).unwrap_or(1);   i+=2; }
            _ => i+=1,
        }
    }

    let data = fs::read(input).unwrap_or_else(|e| { eprintln!("Error reading {input}: {e}"); process::exit(1); });
    println!("Compressing {} ({}) …", input, human_bytes(data.len()));

    let (blob, stats) = core::compress(&data, quality, channels);
    fs::write(output, &blob).unwrap_or_else(|e| { eprintln!("Error writing {output}: {e}"); process::exit(1); });

    let arrow = if stats.ratio_pct >= 0.0 { "▼" } else { "▲" };
    println!("  Input    : {}", human_bytes(stats.original_bytes));
    println!("  Output   : {}", human_bytes(stats.compressed_bytes));
    println!("  Ratio    : {arrow}{:.2}%", stats.ratio_pct.abs());
    println!("  Mode     : {}", stats.mode);
    if !stats.lossless {
        println!("  PSNR     : {:.1} dB", stats.psnr_db);
        println!("  Max error: ±{}", stats.max_error);
    }
    println!("  Time     : {} ms", stats.elapsed_ms);
    println!("Done → {output}");
}

fn cmd_decompress(args: &[String]) {
    if args.len() < 2 { print_usage(); process::exit(1); }
    let input  = &args[0];
    let output = &args[1];

    let blob = fs::read(input).unwrap_or_else(|e| { eprintln!("Error reading {input}: {e}"); process::exit(1); });
    let t0   = Instant::now();
    println!("Decompressing {} ({}) …", input, human_bytes(blob.len()));

    match core::decompress(&blob) {
        Ok(data) => {
            fs::write(output, &data).unwrap_or_else(|e| { eprintln!("Error writing {output}: {e}"); process::exit(1); });
            println!("  Output : {} ({})", output, human_bytes(data.len()));
            println!("  Time   : {} ms", t0.elapsed().as_millis());
            println!("Done ✓  checksum verified");
        }
        Err(e) => { eprintln!("Decompression failed: {e}"); process::exit(1); }
    }
}

fn cmd_info(args: &[String]) {
    if args.is_empty() { print_usage(); process::exit(1); }
    let path = &args[0];
    let blob = fs::read(path).unwrap_or_else(|e| { eprintln!("Error reading {path}: {e}"); process::exit(1); });

    println!("File: {path}  ({})", human_bytes(blob.len()));
    match core::probe(&blob) {
        Some(mode) => println!("  Mode : {mode}"),
        None       => { eprintln!("  Not a valid axmzip file"); process::exit(1); }
    }
}

fn cmd_bench(args: &[String]) {
    if args.is_empty() { print_usage(); process::exit(1); }
    let path = &args[0];
    let data = fs::read(path).unwrap_or_else(|e| { eprintln!("Error reading {path}: {e}"); process::exit(1); });

    println!("Benchmark: {} ({})\n", path, human_bytes(data.len()));
    println!("  {:<12} {:>10} {:>8} {:>10} {:>8}",
             "Quality", "Size", "Ratio", "PSNR(dB)", "Mode");
    println!("  {}", "─".repeat(58));

    for q in [100u8, 95, 90, 80, 75, 60, 50, 25] {
        let (_, stats) = core::compress(&data, q, 1);
        let arrow = if stats.ratio_pct >= 0.0 { "▼" } else { "▲" };
        let psnr_str = if stats.lossless { "∞".into() }
                       else { format!("{:.1}", stats.psnr_db) };
        println!("  {:<12} {:>10} {:>7.1}%{} {:>10} {}",
                 if q == 100 { "lossless".into() } else { format!("q={q}") },
                 human_bytes(stats.compressed_bytes),
                 stats.ratio_pct.abs(), arrow,
                 psnr_str,
                 stats.mode);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { print_usage(); return; }

    match args[1].as_str() {
        "compress"   | "c" => cmd_compress(&args[2..].to_vec()),
        "decompress" | "d" => cmd_decompress(&args[2..].to_vec()),
        "info"       | "i" => cmd_info(&args[2..].to_vec()),
        "bench"      | "b" => cmd_bench(&args[2..].to_vec()),
        _ => { print_usage(); process::exit(1); }
    }
}
