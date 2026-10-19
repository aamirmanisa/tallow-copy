//! Streaming vs delta vs multi-stream, measured back to back.
//!
//!   cargo run --release --example bench_all -- <src> <dst_dir> [rounds]
//!   cargo run --release --example bench_all -- --loopback <src> [streams]

use std::net::TcpListener;
use std::path::Path;
use std::time::Instant;
use tallow_copy_engine::{chunk, stream, transport};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() >= 2 && args[0] == "--loopback" {
        loopback(
            Path::new(&args[1]),
            args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8),
        );
        return;
    }
    if args.len() < 2 {
        eprintln!("usage: bench_all <src> <dst_dir> [rounds] | --loopback <src> [streams]");
        std::process::exit(2);
    }
    let src = Path::new(&args[0]);
    let dst_dir = Path::new(&args[1]);
    let rounds: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    std::fs::create_dir_all(dst_dir).unwrap();
    let size = std::fs::metadata(src).unwrap().len();
    println!(
        "source: {} ({:.1} MB), {} rounds",
        src.display(),
        size as f64 / 1e6,
        rounds
    );

    let dst = dst_dir.join("tallow-stream.bin");
    let mut best = f64::INFINITY;
    for _ in 0..rounds {
        let st = stream::copy(src, &dst).unwrap();
        best = best.min(st.seconds);
    }
    println!(
        "stream        : {:.3}s best ({:.2} GB/s)",
        best,
        size as f64 / best / 1e9
    );

    let cp_dst = dst_dir.join("cp.bin");
    let mut cp_best = f64::INFINITY;
    for _ in 0..rounds {
        let t = Instant::now();
        assert!(std::process::Command::new("cp")
            .arg(src)
            .arg(&cp_dst)
            .status()
            .unwrap()
            .success());
        cp_best = cp_best.min(t.elapsed().as_secs_f64());
    }
    println!(
        "cp            : {:.3}s best ({:.2} GB/s)",
        cp_best,
        size as f64 / cp_best / 1e9
    );

    let data = std::fs::read(src).unwrap();
    let t = Instant::now();
    let sig = chunk::split(&data, chunk::DEFAULT_AVG_CHUNK);
    let sig_s = t.elapsed().as_secs_f64();
    println!(
        "delta planning: {:.3}s ({:.0} MB/s) for {} chunks",
        sig_s,
        size as f64 / 1e6 / sig_s.max(1e-9),
        sig.len()
    );

    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_file(&cp_dst);
}

fn loopback(src: &Path, streams: usize) {
    let size = std::fs::metadata(src).unwrap().len();
    let dst = std::env::temp_dir().join("tc-loopback-out.bin");
    let _ = std::fs::remove_file(&dst);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let dst2 = dst.clone();
    let server = std::thread::spawn(move || transport::serve(&listener, &dst2, streams).unwrap());

    let st = transport::send(&addr, src, streams).unwrap();
    let written = server.join().unwrap();
    println!(
        "loopback {} streams: {:.3}s -> {:.2} GB/s ({} bytes sent, {} written)",
        streams,
        st.seconds,
        st.mb_per_sec() / 1000.0,
        st.bytes,
        written
    );
    let a = std::fs::read(src).unwrap();
    let b = std::fs::read(&dst).unwrap();
    println!("byte-identical: {}", a == b && size == b.len() as u64);
    let _ = std::fs::remove_file(&dst);
}
