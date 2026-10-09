//! Ported from the project's own later copy of this crate
//! (artifacts/tallow/website-deepdive-2026-09-19/mirror/language/crates/tallow-copy-engine).
//! Unmodified: `copy_reason` was ported with it, so plans here advertise the verification that
//! execution performs, exactly as the original suite asserts.
//!
//!
//! Contract under test: every selectable policy either does something real or fails
//! with an explicit error, and the plan never advertises work that execution skips.

use std::fs;
use std::path::{Path, PathBuf};

use tallow_copy_engine::{
    execute, plan, CopyAction, CopyActionKind, CopyErrorCategory, CopyJob, CopyMode, CopyPlan,
    LinkPolicy, MetadataPolicy, VerifyPolicy,
};
use tempfile::tempdir;

/// Copies `template`'s modification time onto `path` so the default `SizeMtime` skip
/// policy reports the pair as unchanged.
fn match_mtime(path: &Path, template: &Path) {
    let modified = fs::metadata(template)
        .expect("template should stat")
        .modified()
        .expect("template should have an mtime");
    fs::File::options()
        .write(true)
        .open(path)
        .expect("path should open for set_modified")
        .set_modified(modified)
        .expect("set_modified should succeed");
}

#[test]
fn size_verify_runs_after_copy() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("src.bin");
    let destination = temp.path().join("dst.bin");
    fs::write(&source, vec![3_u8; 8192]).unwrap();

    let mut job = CopyJob::copy(&source, &destination);
    job.verify = VerifyPolicy::Size;

    let copy_plan = plan(job).unwrap();
    assert!(
        copy_plan
            .actions
            .iter()
            .any(|action| action.kind == CopyActionKind::Copy && action.reason.contains("verify")),
        "copy actions must advertise the verification execution performs: {:?}",
        copy_plan.actions
    );

    let report = execute(&copy_plan);
    assert_eq!(report.copied_files, 1);
    assert_eq!(
        report.verified_files, 1,
        "VerifyPolicy::Size must really verify the file that was written"
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(fs::metadata(&destination).unwrap().len(), 8192);
}

#[test]
fn size_verify_action_detects_destination_size_drift() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("src.bin");
    let destination = temp.path().join("dst.bin");
    fs::write(&source, b"0123456789").unwrap();
    fs::write(&destination, b"0123456789").unwrap();
    match_mtime(&destination, &source);

    let mut job = CopyJob::copy(&source, &destination);
    job.mode = CopyMode::Verify;
    job.verify = VerifyPolicy::Size;

    let copy_plan = plan(job).unwrap();
    assert!(
        copy_plan
            .actions
            .iter()
            .any(|action| action.kind == CopyActionKind::Verify),
        "an unchanged destination under CopyMode::Verify must plan a Verify action"
    );

    // The destination changes on disk after planning: the planned verification must
    // report the mismatch instead of silently passing.
    fs::write(&destination, b"0123456").unwrap();

    let report = execute(&copy_plan);
    assert_eq!(report.verified_files, 0);
    assert!(
        report.errors.iter().any(|error| {
            error.category == CopyErrorCategory::VerifyMismatch
                && error.message.contains("size mismatch")
        }),
        "{:?}",
        report.errors
    );
}

#[test]
fn sampled_hash_verify_reports_verified_for_a_matching_file() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("src.bin");
    let destination = temp.path().join("dst.bin");
    let content = vec![5_u8; 300 * 1024];
    fs::write(&source, &content).unwrap();
    fs::write(&destination, &content).unwrap();
    match_mtime(&destination, &source);

    let mut job = CopyJob::copy(&source, &destination);
    job.verify = VerifyPolicy::SampledHash;

    let copy_plan = plan(job).unwrap();
    assert!(
        copy_plan
            .actions
            .iter()
            .any(|action| action.kind == CopyActionKind::Verify),
        "SampledHash verifies unchanged destinations: {:?}",
        copy_plan.actions
    );
    assert!(
        copy_plan
            .actions
            .iter()
            .all(|action| action.kind != CopyActionKind::Copy),
        "an unchanged destination needs no copy work"
    );

    let report = execute(&copy_plan);
    assert_eq!(report.verified_files, 1);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[test]
fn sampled_hash_verify_catches_same_size_corruption() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("src.bin");
    let destination = temp.path().join("dst.bin");
    let content = vec![5_u8; 300 * 1024];
    fs::write(&source, &content).unwrap();
    fs::write(&destination, &content).unwrap();
    match_mtime(&destination, &source);

    let mut job = CopyJob::copy(&source, &destination);
    job.verify = VerifyPolicy::SampledHash;
    let copy_plan = plan(job).unwrap();

    // Same length, same mtime, different bytes inside a sampled window.
    let mut corrupted = content.clone();
    corrupted[0] = 6;
    fs::write(&destination, &corrupted).unwrap();
    match_mtime(&destination, &source);

    let report = execute(&copy_plan);
    assert_eq!(report.verified_files, 0);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.category == CopyErrorCategory::VerifyMismatch),
        "{:?}",
        report.errors
    );
}

#[test]
fn parallel_copy_still_verifies_every_written_file() {
    let temp = tempdir().unwrap();
    let source_root = temp.path().join("src");
    let destination_root = temp.path().join("dst");
    fs::create_dir_all(&source_root).unwrap();
    for index in 0..3 {
        fs::write(
            source_root.join(format!("file-{index}.bin")),
            vec![index; 16 * 1024],
        )
        .unwrap();
    }

    let mut job = CopyJob::copy(&source_root, &destination_root);
    job.threads = 3;
    job.verify = VerifyPolicy::Size;

    let copy_plan = plan(job).unwrap();
    let report = execute(&copy_plan);

    assert_eq!(report.copied_files, 3);
    assert!(
        report.worker_threads_used > 1,
        "test intent is the parallel path, got {} workers",
        report.worker_threads_used
    );
    assert_eq!(
        report.verified_files, 3,
        "worker-side verification must cover every copied file"
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[test]
fn plan_and_execute_agree_about_verification_actions() {
    // Unchanged destination: `execute` must really perform exactly the verification the
    // plan advertised — no more, no less — for every policy this build accepts.
    for policy in [
        VerifyPolicy::None,
        VerifyPolicy::Size,
        VerifyPolicy::SampledHash,
        VerifyPolicy::FullHash,
    ] {
        let temp = tempdir().unwrap();
        let source = temp.path().join("src.bin");
        let destination = temp.path().join("dst.bin");
        fs::write(&source, vec![9_u8; 4096]).unwrap();
        fs::write(&destination, vec![9_u8; 4096]).unwrap();
        match_mtime(&destination, &source);

        let mut job = CopyJob::copy(&source, &destination);
        job.verify = policy.clone();

        let copy_plan = plan(job).unwrap();
        assert!(
            !copy_plan
                .actions
                .iter()
                .any(|action| action.kind == CopyActionKind::Copy),
            "{policy:?}: an unchanged destination needs no copy work"
        );
        let planned_verify_actions = copy_plan
            .actions
            .iter()
            .filter(|action| action.kind == CopyActionKind::Verify)
            .count();

        let report = execute(&copy_plan);
        let verify_outcomes = report.verified_files
            + report
                .errors
                .iter()
                .filter(|error| error.category == CopyErrorCategory::VerifyMismatch)
                .count() as u64;
        assert_eq!(
            verify_outcomes, planned_verify_actions as u64,
            "{policy:?}: planned {planned_verify_actions} Verify actions but executed {verify_outcomes}"
        );
        assert!(report.errors.is_empty(), "{policy:?}: {:?}", report.errors);
    }
}

#[test]
fn manifest_verify_is_an_explicit_unsupported_error() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("src.bin");
    let destination = temp.path().join("dst.bin");
    fs::write(&source, b"data").unwrap();

    let mut job = CopyJob::copy(&source, &destination);
    job.verify = VerifyPolicy::Manifest;

    let error = plan(job).unwrap_err();
    assert_eq!(error.category, CopyErrorCategory::Unsupported);
    assert!(error.message.contains("Manifest"), "{}", error.message);
    assert!(
        !destination.exists(),
        "a rejected plan must not copy anything"
    );
}

#[test]
fn security_and_owner_metadata_are_explicit_unsupported_errors() {
    for policy in [MetadataPolicy::Security, MetadataPolicy::Owner] {
        let temp = tempdir().unwrap();
        let source = temp.path().join("src.bin");
        let destination = temp.path().join("dst.bin");
        fs::write(&source, b"data").unwrap();

        let mut job = CopyJob::copy(&source, &destination);
        job.metadata = policy;

        let error = plan(job).unwrap_err();
        assert_eq!(error.category, CopyErrorCategory::Unsupported);
        assert!(!destination.exists());
    }
}

#[test]
fn execute_rejects_a_hand_built_plan_with_an_unsupported_policy() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("src.bin");
    let destination = temp.path().join("dst.bin");
    fs::write(&source, b"data").unwrap();

    let mut job = CopyJob::copy(&source, &destination);
    job.metadata = MetadataPolicy::Security;

    // Bypass plan(): an embedder holding a CopyPlan directly must not be able to run a
    // policy this build cannot honour.
    let hand_built = CopyPlan {
        job,
        actions: vec![CopyAction {
            kind: CopyActionKind::Copy,
            source: Some(source.clone()),
            destination: destination.clone(),
            relative_path: PathBuf::from("src.bin"),
            bytes: 4,
            reason: "hand-built plan, never produced by plan()".to_string(),
        }],
        total_files: 1,
        total_bytes: 4,
        skipped_files: 0,
        delete_files: 0,
        // Mechanical: `timings` was added to CopyPlan for phase reporting. The assertions in
        // this suite are untouched.
        timings: Default::default(),
    };

    let report = execute(&hand_built);
    assert_eq!(report.copied_files, 0);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.category == CopyErrorCategory::Unsupported),
        "{:?}",
        report.errors
    );
    assert!(!destination.exists());
}

#[cfg(unix)]
mod link_policy {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Tree {
        _temp: tempfile::TempDir,
        source_root: PathBuf,
        destination_root: PathBuf,
    }

    fn tree() -> Tree {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("src");
        let destination_root = temp.path().join("dst");
        fs::create_dir_all(&source_root).unwrap();
        fs::write(source_root.join("payload.txt"), b"payload").unwrap();
        symlink("payload.txt", source_root.join("link.txt")).unwrap();
        Tree {
            _temp: temp,
            source_root,
            destination_root,
        }
    }

    #[test]
    fn preserve_recreates_the_symlink_instead_of_the_content() {
        let tree = tree();
        let mut job = CopyJob::copy(&tree.source_root, &tree.destination_root);
        job.link_policy = LinkPolicy::Preserve;

        let copy_plan = plan(job).unwrap();
        assert!(
            copy_plan.actions.iter().any(|action| {
                action.kind == CopyActionKind::Copy
                    && action.relative_path == Path::new("link.txt")
                    && action.reason.contains("symlink preserved")
            }),
            "{:?}",
            copy_plan.actions
        );

        let report = execute(&copy_plan);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.copied_files, 2);

        let link_path = tree.destination_root.join("link.txt");
        let link_meta = fs::symlink_metadata(&link_path).unwrap();
        assert!(
            link_meta.file_type().is_symlink(),
            "LinkPolicy::Preserve must recreate a symlink, not a copy of the target"
        );
        assert_eq!(fs::read_link(&link_path).unwrap(), Path::new("payload.txt"));
        assert_eq!(
            fs::read(tree.destination_root.join("payload.txt")).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn preserve_keeps_a_dangling_symlink_dangling() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("src");
        let destination_root = temp.path().join("dst");
        fs::create_dir_all(&source_root).unwrap();
        symlink("missing-target", source_root.join("dangling")).unwrap();

        let mut job = CopyJob::copy(&source_root, &destination_root);
        job.link_policy = LinkPolicy::Preserve;

        let copy_plan = plan(job).unwrap();
        let report = execute(&copy_plan);
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let link_path = destination_root.join("dangling");
        assert!(
            fs::symlink_metadata(&link_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "a preserved dangling link is still a link"
        );
        assert_eq!(
            fs::read_link(&link_path).unwrap(),
            Path::new("missing-target")
        );
    }

    #[test]
    fn preserve_with_size_verify_verifies_the_link_itself() {
        let tree = tree();
        let mut job = CopyJob::copy(&tree.source_root, &tree.destination_root);
        job.link_policy = LinkPolicy::Preserve;
        job.verify = VerifyPolicy::Size;

        let copy_plan = plan(job).unwrap();
        let report = execute(&copy_plan);

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.copied_files, 2);
        assert_eq!(
            report.verified_files, 2,
            "the preserved symlink is verified by link target, the file by size"
        );
    }

    #[test]
    fn follow_copies_the_target_content() {
        let tree = tree();
        let mut job = CopyJob::copy(&tree.source_root, &tree.destination_root);
        job.link_policy = LinkPolicy::Follow;

        let copy_plan = plan(job).unwrap();
        let report = execute(&copy_plan);

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let link_path = tree.destination_root.join("link.txt");
        let link_meta = fs::symlink_metadata(&link_path).unwrap();
        assert!(
            link_meta.file_type().is_file(),
            "LinkPolicy::Follow must copy the target content"
        );
        assert_eq!(fs::read(&link_path).unwrap(), b"payload");
    }

    #[test]
    fn skip_plans_a_skip_action_and_writes_no_link() {
        let tree = tree();
        let mut job = CopyJob::copy(&tree.source_root, &tree.destination_root);
        job.link_policy = LinkPolicy::Skip;

        let copy_plan = plan(job).unwrap();
        assert!(
            copy_plan.actions.iter().any(|action| {
                action.kind == CopyActionKind::Skip && action.reason.contains("symlink skipped")
            }),
            "Skip must be visible in the plan: {:?}",
            copy_plan.actions
        );

        let report = execute(&copy_plan);
        assert_eq!(report.skipped_files, 1);
        assert!(!tree.destination_root.join("link.txt").exists());
        assert!(tree.destination_root.join("payload.txt").exists());
    }

    #[test]
    fn follow_refuses_to_loop_through_a_self_referential_symlink() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("src");
        fs::create_dir_all(&source_root).unwrap();
        fs::write(source_root.join("file.txt"), b"x").unwrap();
        symlink(".", source_root.join("self")).unwrap();

        let mut job = CopyJob::copy(&source_root, temp.path().join("dst"));
        job.link_policy = LinkPolicy::Follow;

        let error = plan(job).unwrap_err();
        assert!(
            error.message.contains("symlink loop"),
            "a following loop must fail loudly, got: {}",
            error.message
        );
    }
}
