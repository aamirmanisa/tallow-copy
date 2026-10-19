//! Phase 1 gate: bundle vs rsync on many small files.
//!
//!   cargo run --release --example bundle_bench -- --files 10000 --size 1024

use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tallow_copy_engine::bundle;

fn all_files(root: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                stack.push(p);
            } else {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str, d: usize| -> usize {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(d)
    };
    let files = get("--files", 10_000);
    let size = get("--size", 1024);
    let base = PathBuf::from(
        args.iter()
            .position(|a| a == "--dir")
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| "/tmp/tcbundlebench".into()),
    );
    let _ = fs::remove_dir_all(&base);
    let src = base.join("src");
    fs::create_dir_all(&src).unwrap();

    let mut payload = vec![0u8; size];
    for i in 0..files {
        for (j, b) in payload.iter_mut().enumerate() {
            *b = ((i * 31 + j * 7) % 251) as u8;
        }
        let d = src.join(format!("d{:02}", i % 50));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(format!("f{i:06}.bin")), &payload).unwrap();
    }
    let total = (files * size) as u64;
    println!(
        "dataset: {files} files x {size} B = {:.1} MB",
        total as f64 / 1e6
    );

    // rsync
    let rd = base.join("rsync-dst");
    let t = Instant::now();
    assert!(std::process::Command::new("rsync")
        .args(["-a"])
        .arg(&src)
        .arg(&rd)
        .arg("-q")
        .status()
        .unwrap()
        .success());
    let rsync_s = t.elapsed().as_secs_f64();
    println!(
        "rsync      : {rsync_s:.3}s ({:.1} MB/s)",
        total as f64 / 1e6 / rsync_s
    );

    // bundle pack + unpack
    let list = all_files(&src);
    let t = Instant::now();
    let pack_file = File::create(base.join("bundle.tcb")).unwrap();
    let ps = bundle::pack(&src, &list, pack_file).unwrap();
    let pack_s = t.elapsed().as_secs_f64();
    println!(
        "bundle pack: {pack_s:.3}s ({:.1} MB/s, {} entries)",
        total as f64 / 1e6 / pack_s,
        ps.entries
    );

    let dst = base.join("unpacked");
    let t = Instant::now();
    let us = bundle::unpack(File::open(base.join("bundle.tcb")).unwrap(), &dst).unwrap();
    let unpack_s = t.elapsed().as_secs_f64();
    println!(
        "bundle unpk: {unpack_s:.3}s ({:.1} MB/s, {} entries)",
        total as f64 / 1e6 / unpack_s,
        us.entries
    );

    let bundle_total = pack_s + unpack_s;
    println!();
    println!(
        "bundle end-to-end: {bundle_total:.3}s vs rsync {rsync_s:.3}s -> {:.2}x",
        rsync_s / bundle_total
    );
    // Two different currencies: bytes/s is the right gate for byte-bound trees,
    // files/s is the right gate for 1 KB files where open() dominates. Reporting
    // only MB/s for 1 KB files would be a gate that can never pass.
    let mbps = total as f64 / 1e6 / pack_s;
    let fps = files as f64 / pack_s;
    println!("pack: {mbps:.1} MB/s, {fps:.0} files/s");
    if size >= 65536 {
        println!(
            "byte-bound gate (>=500 MB/s): {}",
            if mbps >= 500.0 { "PASS" } else { "FAIL" }
        );
    } else {
        println!(
            "small-file gate (>=3000 files/s): {}",
            if fps >= 3000.0 { "PASS" } else { "FAIL" }
        );
    }

    let mut mismatches = 0;
    for f in &list {
        let rel = f.strip_prefix(&src).unwrap();
        if fs::read(f).unwrap() != fs::read(dst.join(rel)).unwrap() {
            mismatches += 1;
            if mismatches <= 3 {
                println!("MISMATCH: {}", rel.display());
            }
        }
    }
    println!(
        "byte-identical: {} of {} files ({} mismatches)",
        list.len() - mismatches,
        list.len(),
        mismatches
    );
    let _ = fs::remove_dir_all(&base);
}
