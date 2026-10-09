//! Benchmark smoke for the copy engine — the entry point named by
//! `docs/benchmarks/copy-benchmark-protocol.md`.
//!
//! Ignored by default because it writes ~32 MiB; run it explicitly:
//!
//! ```sh
//! cargo test -p tallow-copy-engine --test perf_copy_engine -- --ignored --nocapture
//! ```
//!
//! Besides printing the phase metrics the protocol asks for, it asserts that `CopyReport`
//! actually carries them — before `CopyTimings` existed, no run of the engine could satisfy the
//! protocol's metric list at all.

use std::fs;
use std::path::Path;
use std::time::Instant;

use tallow_copy_engine::{
    execute, plan, CopyJob, CopyMode, ErrorPolicy, LinkPolicy, MetadataPolicy, SkipPolicy,
    VerifyPolicy,
};

const BIG_BYTES: usize = 32 * 1024 * 1024;
const SMALL_FILES: usize = 8;
const SMALL_BYTES: usize = 4 * 1024;

fn write_dataset(root: &Path) -> u64 {
    let big = root.join("big.bin");
    // Deterministic patterns, so a mismatch is a real mismatch rather than an artifact of
    // uninitialised memory.
    let block = (0..=255u8).cycle().take(1 << 20).collect::<Vec<u8>>();
    let mut data = Vec::with_capacity(BIG_BYTES);
    while data.len() < BIG_BYTES {
        data.extend_from_slice(&block);
    }
    data.truncate(BIG_BYTES);
    fs::write(&big, &data).unwrap();

    for index in 0..SMALL_FILES {
        let payload: Vec<u8> = (0..SMALL_BYTES)
            .map(|byte| (byte as u8).wrapping_add(index as u8))
            .collect();
        fs::write(root.join(format!("small-{index}.bin")), &payload).unwrap();
    }

    (BIG_BYTES + SMALL_FILES * SMALL_BYTES) as u64
}

fn job(source: &Path, destination: &Path, verify: VerifyPolicy, skip: SkipPolicy) -> CopyJob {
    let mut job = CopyJob::copy(source, destination);
    job.mode = CopyMode::Copy;
    job.verify = verify;
    job.skip = skip;
    // Timestamps are preserved deliberately: the warm pass below relies on the skip policy
    // being able to compare mtimes, and with `DataOnly` the destination carries the copy time
    // instead of the source's, so no size+mtime skip could ever fire. That is a real
    // precondition of the protocol's no-op gates, not a quirk of this test.
    job.metadata = MetadataPolicy::Timestamps;
    job.link_policy = LinkPolicy::Skip;
    job.error_policy = ErrorPolicy::Strict;
    job.threads = 4;
    job
}

#[test]
#[ignore = "writes ~32 MiB; run explicitly with --ignored --nocapture"]
fn copy_engine_perf_smoke_tiny_dataset() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let destination = tmp.path().join("destination");
    fs::create_dir_all(&source).unwrap();
    let expected_bytes = write_dataset(&source);

    // Cold pass: every file moves, verification re-reads the destination.
    let built = plan(job(
        &source,
        &destination,
        VerifyPolicy::ReadAfterWrite,
        SkipPolicy::SizeMtime,
    ))
    .expect("plan the cold pass");
    assert_eq!(built.total_files, SMALL_FILES as u64 + 1);
    assert_eq!(built.total_bytes, expected_bytes);

    let started = Instant::now();
    let cold = execute(&built);
    let wall_ms = started.elapsed().as_millis() as u64;

    assert!(
        cold.errors.is_empty(),
        "cold pass reported errors: {:?}",
        cold.errors
    );
    assert_eq!(cold.copied_files, SMALL_FILES as u64 + 1);
    assert_eq!(cold.bytes_copied, expected_bytes);

    // The protocol's metric list is only satisfiable if these are populated.
    assert!(
        cold.timings.copy_ms > 0,
        "copy phase timing must be recorded: {:?}",
        cold.timings
    );
    assert!(
        cold.timings.verify_ms > 0,
        "read-after-write verification of {} MiB must be measured, got {:?}",
        BIG_BYTES / (1024 * 1024),
        cold.timings
    );
    assert!(
        cold.timings.verify_ms <= cold.timings.copy_ms,
        "verification runs while copying, so it must be a subset of copy time: {:?}",
        cold.timings
    );
    assert_eq!(
        cold.timings.total_ms(),
        cold.timings.plan_ms + cold.timings.copy_ms
    );

    // Warm pass, no verification requested: the plan is a pure no-op, which is only possible
    // because the cold pass preserved timestamps (see `job`). This is the mechanism the
    // protocol's no-op gates depend on.
    let quiet_plan = plan(job(
        &source,
        &destination,
        VerifyPolicy::None,
        SkipPolicy::SizeMtime,
    ))
    .expect("plan the quiet warm pass");
    assert_eq!(
        quiet_plan.skipped_files,
        SMALL_FILES as u64 + 1,
        "a warm destination with no verify request must plan only skips"
    );
    let quiet = execute(&quiet_plan);
    assert_eq!(quiet.copied_files, 0, "warm pass copied files: {:?}", quiet);
    assert_eq!(quiet.skipped_files, SMALL_FILES as u64 + 1);
    assert_eq!(quiet.bytes_copied, 0);

    // Warm pass WITH verification requested: an unchanged destination is upgraded from Skip to
    // Verify, so nothing is copied and nothing is counted as skipped - the work shown in the
    // plan is the work that runs. Encoding both halves here keeps a future "optimisation" from
    // collapsing the verify request into a silent skip.
    let verify_plan = plan(job(
        &source,
        &destination,
        VerifyPolicy::ReadAfterWrite,
        SkipPolicy::SizeMtime,
    ))
    .expect("plan the verifying warm pass");
    assert_eq!(
        verify_plan.skipped_files, 0,
        "an unchanged destination with a verify request must be verified, not skipped"
    );
    let verified = execute(&verify_plan);
    assert_eq!(verified.copied_files, 0, "verifying pass copied files: {:?}", verified);
    assert_eq!(verified.skipped_files, 0);
    assert_eq!(verified.verified_files, SMALL_FILES as u64 + 1);

    let mb_per_s = if cold.timings.copy_ms == 0 {
        0.0
    } else {
        (cold.bytes_copied as f64 / (1024.0 * 1024.0)) / (cold.timings.copy_ms as f64 / 1000.0)
    };
    let files_per_s = if cold.timings.copy_ms == 0 {
        0.0
    } else {
        (cold.copied_files as f64) / (cold.timings.copy_ms as f64 / 1000.0)
    };

    println!("copy engine perf smoke (tiny dataset)");
    println!("  files            : {}", cold.copied_files);
    println!("  logical bytes    : {}", cold.bytes_copied);
    println!(
        "  plan_ms          : {} (scan+plan: one phase in this engine)",
        cold.timings.plan_ms
    );
    println!("  copy_ms          : {}", cold.timings.copy_ms);
    println!(
        "  verify_ms        : {} (subset of copy_ms)",
        cold.timings.verify_ms
    );
    println!("  total_ms         : {}", cold.timings.total_ms());
    println!("  wall_ms          : {wall_ms}");
    println!("  throughput MB/s  : {mb_per_s:.1}");
    println!("  files/s          : {files_per_s:.0}");
    println!("  warm pass skipped: {}", quiet.skipped_files);
    println!("  warm pass verified: {}", verified.verified_files);
}
