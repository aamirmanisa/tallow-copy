use std::fs;

use tallow_copy_engine::{
    CopyActionKind, CopyJob, CopyMode, CopyProgressKind, MetadataPolicy, VerifyPolicy,
    execute_with_progress, plan,
};
use tempfile::tempdir;

#[test]
fn production_copy_engine_mirror_smoke_copies_verifies_and_deletes() {
    let temp = tempdir().expect("tempdir should be created");
    let source = temp.path().join("source");
    let target = temp.path().join("target");

    fs::create_dir_all(source.join("nested")).expect("source tree should be created");
    fs::create_dir_all(&target).expect("target tree should be created");
    fs::write(source.join("alpha.txt"), b"alpha from source")
        .expect("source alpha should be written");
    fs::write(
        source.join("nested").join("beta.bin"),
        vec![7_u8; 16 * 1024],
    )
    .expect("source beta should be written");
    fs::write(target.join("stale.txt"), b"delete me").expect("stale target file should be written");

    let mut job = CopyJob::copy(&source, &target);
    job.mode = CopyMode::Mirror;
    job.verify = VerifyPolicy::Size;
    job.metadata = MetadataPolicy::Timestamps;
    job.threads = 4;
    job.buffer_size_bytes = 64 * 1024;

    let copy_plan = plan(job).expect("mirror plan should be created");
    assert!(
        copy_plan
            .actions
            .iter()
            .any(|action| action.kind == CopyActionKind::Copy),
        "plan should include copy work"
    );
    assert!(
        copy_plan
            .actions
            .iter()
            .any(|action| action.kind == CopyActionKind::Delete),
        "plan should include stale target deletion"
    );

    let mut saw_file_started = false;
    let mut saw_bytes = false;
    let report = execute_with_progress(&copy_plan, |event| match event.kind {
        CopyProgressKind::FileStarted => saw_file_started = true,
        CopyProgressKind::BytesCopied => saw_bytes = true,
        _ => {}
    });

    assert!(
        report.errors.is_empty(),
        "copy should finish without errors: {:?}",
        report.errors
    );
    assert_eq!(
        fs::read(target.join("alpha.txt")).expect("alpha should exist"),
        b"alpha from source"
    );
    assert_eq!(
        fs::read(target.join("nested").join("beta.bin")).expect("beta should exist"),
        vec![7_u8; 16 * 1024]
    );
    assert!(
        !target.join("stale.txt").exists(),
        "mirror mode should remove stale target files"
    );
    assert!(saw_file_started, "progress should emit file-start events");
    assert!(saw_bytes, "progress should emit byte-copy events");
    assert!(
        report.worker_threads_used > 1,
        "mirror smoke should use multiple copy workers"
    );
}
