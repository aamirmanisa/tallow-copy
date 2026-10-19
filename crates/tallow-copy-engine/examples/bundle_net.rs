//! Phase 3 gate: bundle over N streams across a real network path.
//!
//!   # receiver
//!   cargo run --release --example bundle_net -- --serve <dest_dir> --port 9777 --streams 8
//!   # sender
//!   cargo run --release --example bundle_net -- --send <src_dir> --peer <host:port> --streams 8
//!
//! Timing note: a transfer is not finished when the sender's socket writes
//! return — that only means the bytes reached the local kernel buffer. The
//! receiver opens a control connection before the data streams and writes DONE
//! only after every file has been verified, renamed and unpacked. The sender
//! waits for that line, so "end-to-end" here means pack + network + disk, which
//! is the only figure comparable to rsync's wall clock.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tallow_copy_engine::{bundle, transport};

const CONTROL_MAGIC: &[u8; 4] = b"TCCT";

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

        // Accept the control connection first: it is how the sender learns that
        // the data is on disk rather than merely buffered somewhere.
        let (mut ctl, _) = listener.accept().unwrap();
        let mut magic = [0u8; 4];
        ctl.read_exact(&mut magic).unwrap();
        if &magic != CONTROL_MAGIC {
            panic!("expected a control connection, got {magic:?}");
        }

        let tmp = std::env::temp_dir().join(format!("tc-recv-{}.tcb", std::process::id()));
        let t = std::time::Instant::now();
        let bytes = transport::serve(&listener, &tmp, streams).unwrap();
        let net_s = t.elapsed().as_secs_f64();
        let t2 = std::time::Instant::now();
        let us = bundle::unpack(fs::File::open(&tmp).unwrap(), Path::new(&dest)).unwrap();
        let unpack_s = t2.elapsed().as_secs_f64();
        let _ = fs::remove_file(&tmp);

        ctl.write_all(format!("DONE {}\n", us.entries).as_bytes())
            .unwrap();
        ctl.flush().unwrap();

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

        // Control first, so the receiver is expecting data streams next.
        let mut ctl = TcpStream::connect(&peer).unwrap();
        ctl.write_all(CONTROL_MAGIC).unwrap();
        ctl.flush().unwrap();
        ctl.set_read_timeout(Some(Duration::from_secs(3600))).unwrap();

        let t0 = std::time::Instant::now();

        let tmp = std::env::temp_dir().join(format!("tc-send-{}.tcb", std::process::id()));
        let t = std::time::Instant::now();
        let ps = bundle::pack(&src, &files, fs::File::create(&tmp).unwrap()).unwrap();
        let pack_s = t.elapsed().as_secs_f64();

        let t2 = std::time::Instant::now();
        let st = transport::send(&peer, &tmp, streams).unwrap();
        let net_s = t2.elapsed().as_secs_f64();
        let _ = fs::remove_file(&tmp);

        // Block until the receiver confirms every file is unpacked. Without this
        // the reported figure measures the local socket buffer, not the transfer.
        let mut line = String::new();
        BufReader::new(ctl).read_line(&mut line).unwrap();
        let e2e = t0.elapsed().as_secs_f64();
        let done = line.trim();

        println!(
            "packed {} files ({:.1} MB) in {pack_s:.3}s | sent {} streams in {net_s:.3}s ({:.2} MB/s) | receiver: {done} | end-to-end {e2e:.3}s ({:.2} MB/s)",
            ps.entries,
            total as f64 / 1e6,
            st.streams,
            st.mb_per_sec(),
            total as f64 / 1e6 / e2e.max(1e-9)
        );
        return;
    }

    eprintln!("usage: --serve <dir> --port <p> --streams <n> | --send <dir> --peer <host:port> --streams <n>");
    std::process::exit(2);
}
