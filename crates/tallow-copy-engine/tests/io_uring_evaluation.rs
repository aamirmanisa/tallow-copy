//! Could io_uring help Tallow Copy? This is the measurement that answers that, rather than a guess.
//!
//! The buffered loop is what an async backend would replace, so it is measured with the kernel fast
//! paths disabled (`TALLOW_COPY_DISABLE_FAST_PATH=1`) - with them on, the kernel does the copy and
//! the userspace loop never runs at all. The *shape* of the result decides the answer:
//!
//!   * throughput rising steeply with the buffer size and the thread count means per-operation
//!     syscall overhead dominates - which is exactly what io_uring amortises;
//!   * throughput flat across both means the memory bandwidth or the destination device is the
//!     limit, and an asynchronous interface cannot help, because there are already few enough
//!     syscalls that making them cheaper changes nothing.
//!
//! Ignored by default: it writes ~1.2 GiB. Point `TALLOW_BENCH_ROOT` at a directory on the
//! filesystem you care about, outside the repository, as `docs/benchmarks/copy-benchmark-protocol.md`
//! requires:
//!
//! ```sh
//! export TALLOW_BENCH_ROOT=/some/dir
//! TALLOW_COPY_DISABLE_FAST_PATH=1 \
//!   cargo test -p tallow-copy-engine --test io_uring_evaluation -- --ignored --nocapture
//! TALLOW_BENCH_MODE=ceiling \
//!   cargo test -p tallow-copy-engine --test io_uring_evaluation -- --ignored --nocapture
//! ```
//!
//! The second run is the kernel's own ceiling on the same dataset: a fast path engages, and the
//! gap between the two is what a userspace backend could theoretically chase - not the raw device
//! bandwidth.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use tallow_copy_engine::{execute, plan, CopyJob};

const BIG_BYTES: usize = 512 * 1024 * 1024;
const SMALL_FILES: usize = 2_000;
const SMALL_BYTES: usize = 32 * 1024;
const MIB: f64 = 1024.0 * 1024.0;

fn bench_root() -> PathBuf {
    let root = std::env::var_os("TALLOW_BENCH_ROOT")
        .map(PathBuf::from)
        .expect("set TALLOW_BENCH_ROOT to a directory outside the repository");
    fs::create_dir_all(&root).expect("create bench root");
    root
}

/// 512 MiB of deterministic bytes, written once and reused by every run.
fn write_big(path: &Path) -> u64 {
    if path.exists() {
        return BIG_BYTES as u64;
    }
    let block: Vec<u8> = (0..(1 << 20)).map(|index| index as u8).collect();
    let mut file = fs::File::create(path).expect("create big.bin");
    let mut written = 0usize;
    while written < BIG_BYTES {
        file.write_all(&block).expect("write big.bin");
        written += block.len();
    }
    file.sync_all().expect("sync big.bin");
    written as u64
}

/// The many-small-file class, which is where batching wins if it wins anywhere.
fn write_small(dir: &Path) -> u64 {
    fs::create_dir_all(dir).expect("create small dir");
    let payload: Vec<u8> = (0..SMALL_BYTES).map(|index| (index % 251) as u8).collect();
    for index in 0..SMALL_FILES {
        let path = dir.join(format!("f{index:05}.bin"));
        if !path.exists() {
            fs::write(&path, &payload).expect("write small file");
        }
    }
    (SMALL_FILES * SMALL_BYTES) as u64
}

/// One measured copy into a fresh destination. Returns MiB/s over the copy phase.
fn measure(label: &str, source: &Path, destination: &Path, threads: usize, buffer: usize) -> f64 {
    // A file source writes the destination as a FILE (the engine copies to that exact path),
    // while a directory source makes it a directory - so clearing has to handle both.
    if destination.exists() {
        if destination.is_dir() {
            fs::remove_dir_all(destination).expect("clear destination dir");
        } else {
            fs::remove_file(destination).expect("clear destination file");
        }
    }
    let mut job = CopyJob::copy(source, destination);
    job.threads = threads;
    job.buffer_size_bytes = buffer;

    let prepared = plan(job).expect("plan");
    let report = execute(&prepared);
    assert!(
        report.errors.is_empty(),
        "{label} copied with errors: {:?}",
        report.errors
    );
    let milliseconds = report.timings.copy_ms.max(1) as f64;
    let mib_per_second = report.bytes_copied as f64 / MIB / (milliseconds / 1000.0);
    println!(
        "ROW {label:<10} threads={threads:<2} buffer_kib={:<6} ms={milliseconds:<8.0} MiB/s={mib_per_second:.1}",
        buffer / 1024
    );
    mib_per_second
}

#[test]
#[ignore = "writes ~1.2 GiB; run explicitly with --ignored --nocapture and TALLOW_BENCH_ROOT set"]
fn io_uring_evaluation_buffer_and_thread_sweep() {
    let root = bench_root();
    let big = root.join("big.bin");
    let small = root.join("small");
    write_big(&big);
    write_small(&small);

    // `TALLOW_BENCH_MODE=ceiling` is the same dataset and the same engine with the kernel fast
    // paths left enabled, which is what a userspace rewrite would be competing with.
    if std::env::var("TALLOW_BENCH_MODE").as_deref() == Ok("ceiling") {
        println!("MODE ceiling (kernel fast paths enabled)");
        let defaults = CopyJob::copy(&big, root.join("dst-ceiling"));
        measure(
            "big",
            &big,
            &root.join("dst-ceiling"),
            defaults.threads,
            defaults.buffer_size_bytes,
        );
        // The small-file class, where batching would matter most.
        let defaults = CopyJob::copy(&small, root.join("dst-ceiling-small"));
        measure(
            "small",
            &small,
            &root.join("dst-ceiling-small"),
            defaults.threads,
            defaults.buffer_size_bytes,
        );
        return;
    }

    let disabled = std::env::var_os("TALLOW_COPY_DISABLE_FAST_PATH").is_some_and(|v| !v.is_empty());
    println!(
        "MODE buffered loop (TALLOW_COPY_DISABLE_FAST_PATH {})",
        if disabled {
            "set"
        } else {
            "NOT set - ceiling numbers, not loop numbers"
        }
    );

    // Buffer sweep at one thread: the purest signal for per-operation syscall overhead, because
    // nothing else is competing and the same bytes cross the same device.
    for kib in [64usize, 256, 1024, 4096, 16384] {
        measure("big", &big, &root.join("dst-buf"), 1, kib * 1024);
    }
    // Thread sweep at the middle buffer: if the loop is syscall-bound, more threads help; if it is
    // device-bound, they do not (and previously measured SMB behaviour already showed they can hurt).
    for threads in [1usize, 2, 4, 8] {
        measure("big", &big, &root.join("dst-threads"), threads, 1024 * 1024);
    }
    // Small files, the other place an async interface could pay off.
    for threads in [1usize, 4, 8] {
        measure(
            "small",
            &small,
            &root.join("dst-small"),
            threads,
            1024 * 1024,
        );
    }
}
