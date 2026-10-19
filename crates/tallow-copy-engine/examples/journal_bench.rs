//! Phase 2 gate: full scan vs journal rescan.
//!
//!   cargo run --release --example journal_bench -- --files 200000

use std::fs;
use std::path::PathBuf;
use tallow_copy_engine::journal;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let files: usize = args
        .iter()
        .position(|a| a == "--files")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let base = PathBuf::from("/tmp/tcjournalbench");
    let _ = fs::remove_dir_all(&base);
    let src = base.join("src");
    for i in 0..files {
        let d = src.join(format!("d{:03}", i % 500));
        if i < 500 {
            fs::create_dir_all(&d).unwrap();
        }
        fs::write(d.join(format!("f{i:07}.bin")), [i as u8; 64]).unwrap();
    }
    println!("dataset: {files} files");

    std::thread::sleep(std::time::Duration::from_millis(50));
    let t = std::time::Instant::now();
    let j = journal::scan(&src).unwrap();
    let scan_s = t.elapsed().as_secs_f64();
    println!(
        "full scan : {scan_s:.3}s ({} entries, hashed all)",
        j.entries.len()
    );

    let jp = base.join("journal.bin");
    journal::save(&j, &jp, None).unwrap();

    let t = std::time::Instant::now();
    let diff = journal::rescan(&src, &j).unwrap();
    let rescan_s = t.elapsed().as_secs_f64();
    println!(
        "rescan    : {rescan_s:.3}s (unchanged {}, hashed {})",
        diff.unchanged, diff.hashed
    );
    println!("speedup   : {:.1}x", scan_s / rescan_s.max(1e-9));

    // One edit, then rescan again: must find exactly that file.
    std::thread::sleep(std::time::Duration::from_millis(30));
    let victim = src.join("d000").join("f0000000.bin");
    fs::write(&victim, [0xEEu8; 64]).unwrap();
    let t = std::time::Instant::now();
    let diff2 = journal::rescan(&src, &j).unwrap();
    println!(
        "after edit: {:.3}s, changed {} (expected 1), hashed {}",
        t.elapsed().as_secs_f64(),
        diff2.changed.len(),
        diff2.hashed
    );
    println!(
        "gate G2 (rescan <= 5s at 1M files): {rescan_s:.3}s at {files} -> {:.2}s extrapolated -> {}",
        rescan_s * 1_000_000.0 / files as f64,
        if rescan_s * 1_000_000.0 / files as f64 <= 5.0 { "PASS" } else { "FAIL" }
    );
    let _ = fs::remove_dir_all(&base);
}
