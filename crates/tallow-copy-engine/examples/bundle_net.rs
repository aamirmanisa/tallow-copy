//! Phase 3 gate: bundle over N streams across a real network path.
//!
//!   # receiver
//!   cargo run --release --example bundle_net -- --serve <dest_dir> --port 9777 --streams 8
//!   # sender
//!   cargo run --release --example bundle_net -- --send <src_dir> --peer 127.0.0.1:9777 --streams 8

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use tallow_copy_engine::{bundle, transport};

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

fn arg(args: &[String], k: &str) -> Option<String> {
    args.iter()
        .position(|a| a == k)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let streams: usize = arg(&args, "--streams")
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);

    if let Some(dest) = arg(&args, "--serve") {
        let port: u16 = arg(&args, "--port")
            .and_then(|s| s.parse().ok())
            .unwrap_or(9777);
        let listener = TcpListener::bind(("0.0.0.0", port)).unwrap();
        println!("receiving {streams} streams on :{port} -> {dest}");
        let tmp = std::env::temp_dir().join(format!("tc-recv-{}.tcb", std::process::id()));
        let t = std::time::Instant::now();
        let bytes = transport::serve(&listener, &tmp, streams).unwrap();
        let net_s = t.elapsed().as_secs_f64();
        let t2 = std::time::Instant::now();
        let us = bundle::unpack(fs::File::open(&tmp).unwrap(), Path::new(&dest)).unwrap();
        let unpack_s = t2.elapsed().as_secs_f64();
        let _ = fs::remove_file(&tmp);
        println!(
            "received {:.1} MB in {net_s:.3}s ({:.1} MB/s) | unpacked {} entries in {unpack_s:.3}s",
            bytes as f64 / 1e6,
            bytes as f64 / 1e6 / net_s.max(1e-9),
            us.entries
        );
        return;
    }

    if let Some(src) = arg(&args, "--send") {
        let peer = arg(&args, "--peer").unwrap_or_else(|| "127.0.0.1:9777".into());
        let src = PathBuf::from(src);
        let files = all_files(&src);
        let total: u64 = files
            .iter()
            .map(|f| fs::metadata(f).map(|m| m.len()).unwrap_or(0))
            .sum();

        let tmp = std::env::temp_dir().join(format!("tc-send-{}.tcb", std::process::id()));
        let t = std::time::Instant::now();
        let ps = bundle::pack(&src, &files, fs::File::create(&tmp).unwrap()).unwrap();
        let pack_s = t.elapsed().as_secs_f64();

        let t2 = std::time::Instant::now();
        let st = transport::send(&peer, &tmp, streams).unwrap();
        let net_s = t2.elapsed().as_secs_f64();
        let _ = fs::remove_file(&tmp);

        println!(
            "packed {} files ({:.1} MB) in {pack_s:.3}s | sent {} streams in {net_s:.3}s ({:.2} MB/s)",
            ps.entries,
            total as f64 / 1e6,
            st.streams,
            st.mb_per_sec()
        );
        return;
    }

    eprintln!("usage: --serve <dir> --port <p> --streams <n> | --send <dir> --peer <host:port> --streams <n>");
    std::process::exit(2);
}
