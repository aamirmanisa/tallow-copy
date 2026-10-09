use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::SystemTime;

use filetime::FileTime;

pub const DEFAULT_BUFFER_SIZE_BYTES: usize = 1024 * 1024;
pub const MIN_BUFFER_SIZE_BYTES: usize = 4096;

/// Copy one file, preserving sparse layout and the source modification time.
///
/// This is the single policy-free file primitive for the Tallow copy surface: the
/// stdlib's `transfer.fast_copy` / `fast_copy_dir` and the CLI call through it, so
/// sparse handling and mtime preservation live in exactly one place. Parent
/// directories are NOT created — callers that need that must do it first.
///
/// The bytes land in a sibling temporary file which is then renamed over the destination.
/// Both halves of that are load-bearing:
///
///   * **A read-only destination can still be replaced.** `fs::copy` opens the destination for
///     write and truncates it, which fails with `EACCES` when the existing file is mode 444.
///     Git writes its loose objects 0444, so re-pushing a repository over a populated share
///     failed on every object — measured: 160 of 200 files, exit 5. A rename only needs write
///     permission on the *directory*, which is exactly what rsync relies on.
///   * **The replace is atomic.** A crash or a full disk can never leave a half-written file
///     at the destination path; the old file stays intact until the rename.
///
/// The temporary is created in the same directory, so the rename stays on one filesystem and
/// cannot fail with `EXDEV`.
/// `FICLONE` from `linux/fs.h`: ask the filesystem to share the source's extents with the
/// destination instead of moving bytes. Same-filesystem, same-filesystem-type only.
#[cfg(target_os = "linux")]
const FICLONE: libc::c_ulong = 0x4004_9409;

/// Kernel paths tried per file, in order. Anything they refuse falls through to the buffered
/// loop unchanged, so these are pure upside - but that also means a machine where they never
/// engage looks identical from the outside, which is why the disable knob exists.
///
/// Set `TALLOW_COPY_DISABLE_FAST_PATH` to any non-empty value to force the generic path. It
/// exists so the two can be measured against each other in one binary (see
/// `bench_fast_paths_against_the_generic_loop`), and so a filesystem that misbehaves can be
/// worked around without a rebuild.
fn fast_paths_disabled() -> bool {
    std::env::var_os("TALLOW_COPY_DISABLE_FAST_PATH").is_some_and(|value| !value.is_empty())
}

/// A copy_file_range failure that means "this filesystem cannot do it" rather than "this copy
/// just failed". The first is a fallback; the second must surface.
#[cfg(target_os = "linux")]
fn is_unsupported_errno(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EOPNOTSUPP) | Some(libc::EXDEV) | Some(libc::EINVAL) | Some(libc::ENOSYS)
            | Some(libc::EPERM) | Some(libc::ENOTTY) | Some(libc::EBADF)
    )
}

/// Reflink `source` onto `destination`. Both must already be open; the destination must be
/// empty (a clone replaces the file's contents wholesale, which is why resume never takes
/// this path). `Ok(false)` means the filesystem does not support reflinks - not an error.
#[cfg(target_os = "linux")]
fn try_reflink(source: &fs::File, destination: &fs::File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd as _;
    // SAFETY: plain ioctl on two fds we own; FICLONE takes the source fd as its argument and
    // returns -1 with errno set when the filesystem refuses.
    let rc = unsafe { libc::ioctl(destination.as_raw_fd(), FICLONE, source.as_raw_fd()) };
    if rc == 0 {
        return Ok(true);
    }
    Ok(false)
}

/// Move up to `len` bytes from `offset` in `source` to `destination` using the kernel's
/// `copy_file_range`. Returns `Ok(Some(bytes))` only when it finished the whole range;
/// `Ok(None)` when the filesystem does not support it; `Err` when it broke part-way (the
/// caller then redoes the file generically, because a half-copied destination must never be
/// published).
///
/// Chunked at `chunk` - the SAME size the buffered loop would use - so that progress
/// reporting and cancellation keep the granularity the caller already expects: a caller that
/// cancels after N bytes must be able to stop the copy at N bytes, and `on_chunk` is where it
/// gets the chance (return false from it to stop).
#[cfg(target_os = "linux")]
fn try_copy_file_range<A: FnMut(u64) -> bool>(
    source: &fs::File,
    destination: &fs::File,
    offset: u64,
    len: u64,
    chunk: u64,
    mut on_chunk: A,
) -> std::io::Result<Option<u64>> {
    use std::os::unix::io::AsRawFd as _;

    let chunk = chunk.max(4096);
    let mut off_in = offset as libc::loff_t;
    let mut off_out = offset as libc::loff_t;
    let mut remaining = len;
    let mut moved = 0_u64;

    while remaining > 0 {
        let want = remaining.min(chunk);
        // SAFETY: plain syscall on fds we own, with offsets we hold mutably for its duration.
        let copied = unsafe {
            libc::copy_file_range(
                source.as_raw_fd(),
                &mut off_in,
                destination.as_raw_fd(),
                &mut off_out,
                want as libc::size_t,
                0,
            )
        };
        if copied < 0 {
            let err = std::io::Error::last_os_error();
            if moved == 0 && is_unsupported_errno(&err) {
                return Ok(None);
            }
            return Err(err);
        }
        if copied == 0 {
            break;
        }
        let copied = copied as u64;
        moved += copied;
        remaining = remaining.saturating_sub(copied);
        // Progress after every chunk, so a cancelling callback stops this copy the same way it
        // stops the buffered loop.
        if !on_chunk(copied) {
            return Ok(None);
        }
    }

    Ok(Some(moved))
}

/// Try reflink, then copy_file_range, then give up so the caller uses the buffered loop.
/// Returns the number of bytes the kernel moved, or `None` when neither path applied.
///
/// `resumable` is the job's REQUEST for resumability (resume mode), not whether this file
/// happens to have a partial yet. A resumable job is sent straight to the buffered loop because
/// a reflink is atomic: it either produces the whole file or nothing, so an interruption during
/// one can leave neither a finished destination nor a resumable partial. Jobs that want
/// partials keep the path that can produce them.
#[cfg(target_os = "linux")]
fn attempt_kernel_copy<A: FnMut(u64) -> bool>(
    source: &fs::File,
    destination: &fs::File,
    offset: u64,
    len: u64,
    resumable: bool,
    chunk: u64,
    mut on_chunk: A,
) -> Option<u64> {
    if fast_paths_disabled() || len == 0 || resumable {
        return None;
    }
    // A clone re-creates the whole file, so it can only serve a fresh destination.
    if offset == 0 && matches!(try_reflink(source, destination), Ok(true)) {
        on_chunk(len);
        return Some(len);
    }
    match try_copy_file_range(source, destination, offset, len, chunk, &mut on_chunk) {
        Ok(Some(moved)) if moved == len => Some(moved),
        // Partial or unsupported: the caller redoes it generically.
        _ => None,
    }
}

pub fn copy_file(src: &Path, dst: &Path) -> std::io::Result<u64> {
    let meta = fs::metadata(src)?;
    // Kernel paths first, in order of how much work they remove:
    //   1. reflink (FICLONE) shares extents: metadata work instead of a byte copy.
    //   2. `fs::copy`, which on Linux uses copy_file_range and preserves holes.
    // `std::fs::copy` never issues FICLONE - an earlier comment here claimed it reflinked on
    // btrfs, and that was simply wrong; the reflink has to be asked for explicitly.
    // Never pre-empt either with a hand-rolled sparse copy: that was measured hundreds of
    // times slower for no gain (the sparse redo below stays as a correctness guard).
    let tmp = temp_sibling(dst);
    #[cfg(target_os = "linux")]
    let cloned = {
        let mut done = false;
        if !fast_paths_disabled() {
            if let (Ok(source_file), Ok(dest_file)) = (fs::File::open(src), fs::File::create(&tmp)) {
                done = matches!(try_reflink(&source_file, &dest_file), Ok(true));
            }
        }
        done
    };
    #[cfg(not(target_os = "linux"))]
    let cloned = false;

    // A reflink already produced the file: the bytes are shared with the source, and the
    // reported count is the file length.
    let written = if cloned {
        meta.len()
    } else {
        match fs::copy(src, &tmp) {
            Ok(n) => n,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }
    };
    preserve_mtime(&meta, &tmp);

    // ...but the kernel path does NOT preserve holes on every filesystem. Measured: on NTFS
    // via ntfs-3g a 64 MiB file holding 4 KiB of data came out fully materialised to 64 MiB.
    // So verify the outcome and only pay for a redo when it actually went wrong: the source
    // must be genuinely sparse AND the destination materially larger on disk.
    #[cfg(unix)]
    {
        if is_sparse(&meta) {
            if let Ok(tmp_meta) = fs::metadata(&tmp) {
                if allocation(&tmp_meta) > allocation(&meta).saturating_mul(2) + 4096 {
                    if let Ok(n) = sparse_copy_extents(src, &tmp) {
                        preserve_mtime(&meta, &tmp);
                        replace_file(&tmp, dst)?;
                        return Ok(n);
                    }
                }
            }
        }
    }
    replace_file(&tmp, dst)?;
    Ok(written)
}

/// A unique temporary path beside `dst`, deliberately in the same directory so the final
/// rename cannot cross a filesystem boundary. Dot-prefixed so a destination walk treats it as
/// hidden. It is removed on every error path and consumed by the rename on success.
fn temp_sibling(dst: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = dst
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tallow-copy".to_string());
    let tmp_name = format!(
        ".{}.tallow-tmp.{}.{}",
        name,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    match dst.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(tmp_name),
        _ => PathBuf::from(tmp_name),
    }
}

/// Removes a partially written temporary file unless the copy published it, so a
/// failed or cancelled copy never leaves a stray `.tallow-tmp` sibling behind.
struct TempCleanup {
    path: Option<PathBuf>,
}

impl TempCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Deterministic partial-file path used by `CopyMode::Resume`. Unlike `temp_sibling` it is
/// stable across runs, so a transfer interrupted by a crash can be continued by the next
/// one. It is renamed onto the destination on success and deliberately kept on failure.
fn partial_sibling(dst: &Path) -> PathBuf {
    let name = dst
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tallow-copy".to_string());
    let partial_name = format!(".{}.tallow-partial", name);
    match dst.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(partial_name),
        _ => PathBuf::from(partial_name),
    }
}

/// Move `tmp` onto `dst`, replacing whatever is there.
fn replace_file(tmp: &Path, dst: &Path) -> std::io::Result<()> {
    if let Err(e) = fs::rename(tmp, dst) {
        // `std::fs::rename` refuses to replace an existing destination on Windows.
        if cfg!(windows) && dst.exists() {
            fs::remove_file(dst)?;
            return fs::rename(tmp, dst);
        }
        let _ = fs::remove_file(tmp);
        return Err(e);
    }
    Ok(())
}

/// Bytes actually allocated on disk.
#[cfg(unix)]
fn allocation(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.blocks().saturating_mul(512)
}

/// Allocation smaller than logical size means the source has holes.
#[cfg(unix)]
fn is_sparse(meta: &fs::Metadata) -> bool {
    meta.is_file() && allocation(meta) < meta.len()
}

/// Copy only the regions of `src` that hold data, leaving the gaps as real holes.
///
/// Returns a byte count, or an error so the caller can keep the plain copy it already made.
///
/// The kernel extent map (SEEK_DATA/SEEK_HOLE) is preferred, but it is NOT trustworthy on
/// every filesystem: ntfs-3g answers "offset 0 to EOF is all data" for a 64 MiB file that
/// holds 4 KiB, which would just redo the whole materialising copy. So the map is checked
/// against how much the filesystem says is actually allocated, and if it disagrees the
/// extents are rebuilt by scanning for zero runs instead.
#[cfg(unix)]
fn sparse_copy_extents(src: &Path, dst: &Path) -> std::io::Result<u64> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::FileExt as _;
    use std::os::unix::io::AsRawFd as _;

    const BUF: usize = 1024 * 1024;
    /// A zero run at least this long is written as a hole rather than as data.
    const MIN_HOLE: u64 = 4096;
    /// How far the kernel's claimed data may exceed the allocated size before we distrust it.
    const SLACK: u64 = 1 << 20;

    let mut input = fs::File::open(src)?;
    let meta = input.metadata()?;
    let total = meta.len();
    let fd = input.as_raw_fd();
    if total == 0 {
        return Ok(0);
    }

    // 1. Ask the kernel where the data is.
    let mut extents: Vec<(u64, u64)> = Vec::new();
    let mut pos: u64 = 0;
    while pos < total {
        // SAFETY: plain lseek on an fd we own; negative means "no further data".
        let data = unsafe { libc::lseek(fd, pos as libc::off_t, libc::SEEK_DATA) };
        if data < 0 {
            break;
        }
        let data = data as u64;
        let hole = unsafe { libc::lseek(fd, data as libc::off_t, libc::SEEK_HOLE) };
        let end = if hole < 0 {
            total
        } else {
            (hole as u64).min(total)
        };
        if end <= data {
            break;
        }
        extents.push((data, end));
        pos = end;
        if extents.len() > 4_000_000 {
            break; // sanity bound
        }
    }

    // 2. Distrust it when it claims far more data than is allocated.
    let claimed: u64 = extents.iter().map(|(s, e)| e - s).sum();
    if claimed > allocation(&meta).saturating_add(SLACK) {
        // 3. Rebuild the map from actual content: emit every maximal non-zero region.
        extents.clear();
        input.seek(SeekFrom::Start(0))?;
        let mut buf = vec![0u8; BUF];
        let mut file_off: u64 = 0;
        let mut run_start: Option<u64> = None;
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let mut i = 0usize;
            while i < n {
                if buf[i] == 0 {
                    let zstart = file_off + i as u64;
                    let mut j = i;
                    while j < n && buf[j] == 0 {
                        j += 1;
                    }
                    if (j - i) as u64 >= MIN_HOLE {
                        if let Some(s) = run_start.take() {
                            extents.push((s, zstart));
                        }
                    }
                    i = j;
                } else {
                    if run_start.is_none() {
                        run_start = Some(file_off + i as u64);
                    }
                    i += 1;
                }
            }
            file_off += n as u64;
        }
        if let Some(s) = run_start {
            extents.push((s, total));
        }
    }

    // 4. Write the data regions; the gaps stay holes because the length is set first.
    let mut output = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dst)?;
    output.set_len(total)?;
    let mut buf = vec![0u8; BUF];
    for (start, end) in extents {
        input.seek(SeekFrom::Start(start))?;
        let mut off = start;
        while off < end {
            let want = BUF.min((end - off) as usize);
            let n = input.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            output.write_all_at(&buf[..n], off)?;
            off += n as u64;
        }
    }
    output.flush()?;
    Ok(total)
}

/// Carry the source's modification time onto the destination.
///
/// Required for the size+mtime skip predicate in `copy` / `delta-sync` to reach a warm
/// no-op: `fs::copy` does not carry mtime across, so a freshly written destination
/// would otherwise always look "changed" and be recopied on every run.
fn preserve_mtime(meta: &fs::Metadata, dst: &Path) {
    if let Ok(mtime) = meta.modified() {
        let _ = filetime::set_file_mtime(dst, FileTime::from_system_time(mtime));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CopyMode {
    Copy,
    Mirror,
    Sync,
    Verify,
    Resume,
    DryRun,
}

impl CopyMode {
    /// What this mode does today, so callers do not have to infer it.
    ///
    /// - `Copy`   — copy the source into the destination, applying `skip`, `verify` and
    ///              `metadata`; destination files the skip policy accepts are left alone.
    /// - `Mirror` — `Copy` plus deletion of destination entries with no source counterpart.
    /// - `Sync`   — one-way sync without deletions: identical to `Copy` today, which is what
    ///              the desktop app's Sync mode (DeletePolicy::Never) expects.
    /// - `Verify` — plan verify actions against files that already exist instead of copying.
    /// - `DryRun` — plan only, no I/O in `execute` (also settable via `CopyJob::dry_run`).
    /// - `Resume` — continue each interrupted file from its deterministic partial sibling
    ///              (".<name>.tallow-partial"); only the missing tail is transferred.
    pub fn description(&self) -> &'static str {
        match self {
            CopyMode::Copy => "copy files, honouring the skip/verify/metadata policies",
            CopyMode::Mirror => "copy files and delete destination entries absent from the source",
            CopyMode::Sync => "one-way sync without deletions (same as Copy)",
            CopyMode::Verify => "verify existing destination files instead of copying them",
            CopyMode::Resume => "resume each interrupted file from its partial sibling",
            CopyMode::DryRun => "plan only, perform no I/O",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SkipPolicy {
    /// Compare size and modification time only: two cheap stats, no file reads.
    SizeMtime,
    /// As `SizeMtime`, but when size and mtime both match, reads BOTH files in full and
    /// compares hashes. That confirmation runs on the already-in-sync majority, so a warm
    /// re-sync of a large tree costs O(bytes) of reads rather than O(files) of stats.
    SizeMtimeHash,
    /// Read and hash both files unconditionally, ignoring size and mtime.
    Hash,
    /// NOT IMPLEMENTED: planning treats it like `Hash`. Unlike the verify-side `Manifest`
    /// this is not refused, because it still does real work - a full hash comparison - which
    /// is stronger than the manifest lookup it claims; it is only the mechanism that differs.
    Manifest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerifyPolicy {
    None,
    /// Verify by comparing file lengths only: cheap, and catches truncation, but not
    /// same-size corruption.
    Size,
    /// Verify by comparing a hash of fixed head/middle/tail windows (see
    /// `VERIFY_SAMPLE_WINDOW_BYTES`). Weaker than `FullHash`, but independent of file size.
    SampledHash,
    FullHash,
    ReadAfterWrite,
    /// NOT IMPLEMENTED: verification is refused with an `Unsupported` error rather than
    /// silently behaving like a full hash, which is what it used to do.
    Manifest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataPolicy {
    DataOnly,
    Timestamps,
    Attributes,
    /// NOT IMPLEMENTED: refused by `plan()` and `execute()` with an `Unsupported` error
    /// instead of silently copying data without the security context.
    Security,
    /// NOT IMPLEMENTED: refused by `plan()` and `execute()` with an `Unsupported` error
    /// instead of silently dropping ownership.
    Owner,
    All,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LinkPolicy {
    Skip,
    /// Recreate symlinks at the destination with the same link target string; no bytes are
    /// copied. Fails with `Unsupported` on platforms that cannot create symlinks.
    Preserve,
    /// Resolve symlinks and copy what they point at: a file's contents, or a directory's
    /// contents (guarded against links that point back up the tree).
    Follow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ErrorPolicy {
    /// Record a file that fails and carry on with the rest of the tree. The failure lands in
    /// `CopyReport::errors`, and the caller decides what that means (the CLI exits 5 when the list
    /// is non-empty) - the destination is as complete as the tree allowed.
    BestEffort,
    /// Stop the job at the first failure. The error is recorded exactly as in `BestEffort`, but the
    /// remaining files are never attempted, so a Strict run can leave the destination partly
    /// populated. Callers still learn the job failed from `CopyReport::errors` being non-empty.
    Strict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyJob {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub mode: CopyMode,
    pub skip: SkipPolicy,
    pub verify: VerifyPolicy,
    pub metadata: MetadataPolicy,
    pub link_policy: LinkPolicy,
    pub error_policy: ErrorPolicy,
    pub threads: usize,
    pub buffer_size_bytes: usize,
    pub dry_run: bool,
    /// The manifest a `VerifyPolicy::Manifest` job is checked against. Required for that policy: without
    /// it the policy would verify nothing, which is the same as not verifying at all.
    pub manifest: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CopyActionKind {
    Copy,
    Skip,
    Verify,
    Delete,
    Mkdir,
    Metadata,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyAction {
    pub kind: CopyActionKind,
    pub source: Option<PathBuf>,
    pub destination: PathBuf,
    pub relative_path: PathBuf,
    pub bytes: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyPlan {
    pub job: CopyJob,
    pub actions: Vec<CopyAction>,
    pub total_files: u64,
    pub total_bytes: u64,
    pub skipped_files: u64,
    pub delete_files: u64,
    /// Planning cost, carried so `execute` can report it beside the execution timings.
    pub timings: CopyTimings,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CopyErrorCategory {
    Io,
    VerifyMismatch,
    InvalidInput,
    Unsupported,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyError {
    pub category: CopyErrorCategory,
    pub path: Option<PathBuf>,
    pub message: String,
    pub retryable: bool,
    pub recommended_action: String,
}

impl std::fmt::Display for CopyError {
    /// The error as a caller-facing line: the message, with the path it concerns when known.
    ///
    /// There was no `Display` here at all, so every caller that wanted to report an engine error
    /// had to reach into `.message` itself - and one that did not made the whole crate fail to
    /// compile. A structured error still needs a text form.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {}", path.display(), self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for CopyError {}

/// Wall-clock phase timings for a job, so a run can satisfy the benchmark protocol without
/// external instrumentation.
///
/// Two honest notes on the shape of this:
/// - Scanning and planning are ONE phase here (`plan_ms`): the plan is built while walking the
///   source and destination, so reporting separate scan and plan figures would be inventing a
///   boundary the code does not have.
/// - `verify_ms` is a subset of `copy_ms`, not an addition to it: files are verified as they
///   finish. It is reported separately because the protocol asks for the verification cost on
///   its own.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CopyTimings {
    pub plan_ms: u64,
    pub copy_ms: u64,
    pub verify_ms: u64,
}

impl CopyTimings {
    /// Plan plus execution. Verification is already counted inside `copy_ms`.
    pub fn total_ms(&self) -> u64 {
        self.plan_ms.saturating_add(self.copy_ms)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct CopyReport {
    pub copied_files: u64,
    pub skipped_files: u64,
    pub verified_files: u64,
    pub deleted_files: u64,
    pub bytes_copied: u64,
    pub worker_threads_used: usize,
    pub errors: Vec<CopyError>,
    /// When each phase ran and for how long. Populated by `plan` and `execute`.
    pub timings: CopyTimings,
}

#[derive(Clone, Debug, Default)]
pub struct CopyControl {
    cancelled: Arc<AtomicBool>,
}

impl CopyControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CopyProgressKind {
    FileStarted,
    BytesCopied,
    FileFinished,
    FileSkipped,
    FileDeleted,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyProgressEvent {
    pub kind: CopyProgressKind,
    pub action_kind: CopyActionKind,
    pub source: Option<PathBuf>,
    pub destination: PathBuf,
    pub relative_path: PathBuf,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub aggregate_bytes_done: u64,
    pub aggregate_bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub error: Option<CopyError>,
}

/// The filesystem relationship between a source and a destination, as far as the measurements
/// distinguish it. Every variant carries the evidence that produced it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathClass {
    /// Same device: the kernel can clone, so extra workers only contend.
    SameVolume,
    /// SMB/CIFS. Measured on this workstation over Wi-Fi: 47.2 / 45.2 / 35.1 / 38.4 MB/s for
    /// 1 / 2 / 3 / 4 concurrent streams - it does not scale, and four streams are SLOWER than
    /// one. One stream is the right answer here, however many cores are free.
    Smb,
    /// Anything else, including other network filesystems. Two lanes is what the measurements
    /// support: a WAN SSH pull measured 13.27 MB/s on one stream, 25.20 MB/s on two (1.90x) and
    /// 25.16 MB/s on three, i.e. the ceiling arrives at two and a third lane only divides it.
    Other,
}

/// `CIFS_MAGIC_NUMBER` (SMB1) and `SMB2_MAGIC_NUMBER` (SMB2/SMB3) from `linux/magic.h`.
///
/// Both are needed: the mount this rule was measured against is `vers=3.0`, which reports
/// `fe534d42` (type `smb2`), so checking only the CIFS value classified a real SMB share as an
/// unknown filesystem. Verified against the live mount with `stat -f`, not assumed from the
/// mount's own `type cifs` label.
// Plain data, so it is declared for every target. The SMB check lives in `classify_paths`, which
// is compiled everywhere, while `filesystem_magic_of` returns `None` off Linux. Gating this
// constant to Linux made the whole crate fail to compile on Windows and macOS with E0425.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SMB_MAGIC_NUMBERS: [i64; 2] = [0xFF53_4D42, 0xFE53_4D42];

/// The nearest ancestor of `path` that exists, so a destination that has not been created yet
/// can still be classified by the mount it will land on.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.exists() {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

/// Collapse `.` and `..` segments lexically, without touching the filesystem.
///
/// `..` only pops a real name: it never climbs past the root, and it is kept as-is when the
/// preceding segment is itself `..` (so `../../x` stays relative rather than silently becoming
/// wrong). This matters for destinations that do not exist yet, where the filesystem cannot answer.
fn lexical_clean(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a caller-supplied path: absolute, no `.`/`..` segments, symlinked PARENTS resolved.
///
/// The FINAL component is deliberately left exactly as given. Whether a symlink is followed,
/// recreated or skipped is `LinkPolicy`'s decision, and canonicalising the last component here
/// would make that decision for it - `Preserve` exists precisely to copy a link as a link.
///
/// Parents are resolved through `canonicalize`, walking up to the nearest existing ancestor for a
/// destination that does not exist yet (the same trick `device_of` uses). This is what makes a
/// destination reached through a symlinked directory plan against the real path, so a copy and a
/// later comparison agree on which file they mean.
pub fn normalise_path(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return PathBuf::new();
    }
    let absolute = if path.is_absolute() {
        lexical_clean(path)
    } else {
        match std::env::current_dir() {
            Ok(cwd) => lexical_clean(&cwd.join(path)),
            Err(_) => lexical_clean(path),
        }
    };

    // No final component to preserve (root, or a bare `..`): nothing to resolve.
    let (Some(name), Some(parent)) = (absolute.file_name(), absolute.parent()) else {
        return absolute;
    };

    let mut candidate = parent.to_path_buf();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match candidate.canonicalize() {
            Ok(mut resolved) => {
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                resolved.push(name);
                return resolved;
            }
            Err(_) => match (candidate.file_name(), candidate.parent()) {
                (Some(part), Some(up)) if up != candidate => {
                    missing.push(part.to_os_string());
                    candidate = up.to_path_buf();
                }
                _ => return absolute,
            },
        }
    }
}

/// Device id of the filesystem holding `path`, looking up the tree when it does not exist yet.
#[cfg(unix)]
fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    nearest_existing(path)
        .and_then(|existing| fs::metadata(existing).ok())
        .map(|meta| meta.dev())
}

#[cfg(not(unix))]
fn device_of(_path: &Path) -> Option<u64> {
    None
}

/// `statfs` magic of the filesystem holding `path`.
#[cfg(target_os = "linux")]
fn filesystem_magic_of(path: &Path) -> Option<i64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let existing = nearest_existing(path)?;
    let c_path = CString::new(existing.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statfs` fills a buffer we own; the path is a valid NUL-terminated C string that
    // outlives the call.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }
    Some(buf.f_type as i64)
}

#[cfg(not(target_os = "linux"))]
fn filesystem_magic_of(_path: &Path) -> Option<i64> {
    None
}

/// Classify a source/destination pair. Never fails: an unreadable path yields `Other`, which is
/// the "one or two lanes, decided by the caller" answer rather than an error, because a copy
/// must not fail over a thread-count hint.
pub fn classify_paths(source: &Path, destination: &Path) -> PathClass {
    if let (Some(source_device), Some(destination_device)) =
        (device_of(source), device_of(destination))
    {
        if source_device == destination_device {
            return PathClass::SameVolume;
        }
    }
    if filesystem_magic_of(destination).is_some_and(|magic| SMB_MAGIC_NUMBERS.contains(&magic)) {
        return PathClass::Smb;
    }
    PathClass::Other
}

/// Worker threads worth using for this source/destination pair, from the measured behaviour
/// above. This is a DEFAULT, not a policy: a caller that asked for a specific count keeps it.
pub fn recommended_threads(source: &Path, destination: &Path) -> usize {
    match classify_paths(source, destination) {
        PathClass::SameVolume | PathClass::Smb => 1,
        PathClass::Other => 2,
    }
}

/// Worker count for a local small-file tree. Measured on 2000 x 32 KiB copies (buffered, btrfs):
/// 440 MiB/s at one thread, 801 at four, 868 at eight. Four is chosen over the measured eight
/// because the gain from 4 -> 8 is ~8% while eight threads were measured *slower* than four for
/// large files (581 vs 3556 MiB/s), and a tree usually holds both. Capped by the machine.
pub const SMALL_FILE_THREADS: usize = 4;

/// Threads worth using for a plan that has already been walked: the path-class baseline, refined by
/// what the tree actually holds.
///
/// Zero as a job's `threads` means "derive it", and `plan` resolves it through here - see the note
/// there. Only one class is refined: a local volume holding many small files, which is the only case
/// measured to benefit from concurrency. Large files do not regress at 2-4 threads, so raising the
/// count for a mixed tree is safe. SMB keeps one worker (four SMB streams were measured slower than
/// one: 47.2 vs 35.1 MB/s) and the WAN keeps two, where the link and not the syscall path is the
/// limit and the two-stream ceiling was already reached (25.20 vs 25.16 MB/s).
pub fn recommended_threads_for_plan(plan: &CopyPlan) -> usize {
    let baseline = recommended_threads(&plan.job.source, &plan.job.destination);
    if classify_paths(&plan.job.source, &plan.job.destination) != PathClass::SameVolume {
        return baseline;
    }
    if plan.bundling_hint().small_files < BUNDLE_MIN_SMALL_FILES {
        return baseline;
    }
    small_file_thread_count()
}

/// The worker count worth using once a tree is KNOWN to hold many small files: capped by the machine
/// and by `SMALL_FILE_THREADS`, because the measured gain from four to eight threads was ~8% while
/// eight threads were measured slower than four for large files (581 vs 3556 MiB/s).
fn small_file_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|cores| cores.get())
        .unwrap_or(1)
        .clamp(1, SMALL_FILE_THREADS)
}

/// How many directory entries the small-file probe examines before giving up. Answering "is this a
/// large-file tree?" should not cost a full walk of a large-file tree, so the probe decides from a
/// bounded sample instead.
pub const SMALL_FILE_PROBE_ENTRIES: u64 = 4096;

/// Count small files under `root`, examining at most `SMALL_FILE_PROBE_ENTRIES` entries and stopping
/// as soon as enough have been seen to decide.
///
/// Links are not followed: this is a scheduling probe, not a security boundary, but it must never
/// traverse out of the tree it was pointed at.
pub fn probe_small_files(root: &Path) -> u64 {
    probe_small_files_within(root, SMALL_FILE_PROBE_ENTRIES).0
}

/// The probe itself, with its budget as an argument so the budget can be tested rather than trusted.
/// Returns the small-file count and how many entries were examined.
pub fn probe_small_files_within(root: &Path, entry_limit: u64) -> (u64, u64) {
    let mut examined = 0_u64;
    let mut small = 0_u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if examined >= entry_limit || small >= BUNDLE_MIN_SMALL_FILES {
                return (small, examined);
            }
            examined += 1;
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if metadata.len() <= SMALL_FILE_BYTES {
                small += 1;
            }
        }
    }
    (small, examined)
}

/// Threads for a source/destination pair, derived from the path class AND from what the source tree
/// actually holds, without needing a plan.
///
/// This is the form a caller wants when it has not walked the tree with the engine's planner - the
/// CLI's `delta-sync` resolves `-j 0` through here. A fixed count cannot be right for both ends of the
/// measured range: eight threads beat one on a local small-file tree (868 vs 440 MiB/s) while eight
/// were far slower than one for large files (581 vs 3556 MiB/s) and ~26% slower over CIFS (35 vs 47
/// MB/s). Deriving from the pair and the tree is better than any single number.
pub fn recommended_threads_for_tree_path(source: &Path, destination: &Path) -> usize {
    let baseline = recommended_threads(source, destination);
    if classify_paths(source, destination) != PathClass::SameVolume {
        return baseline;
    }
    if probe_small_files(source) < BUNDLE_MIN_SMALL_FILES {
        return baseline;
    }
    small_file_thread_count()
}

/// A file at or below this size counts as "small" for bundling: where each file costs a round
/// trip, its bytes are cheap and its metadata cost is not.
pub const SMALL_FILE_BYTES: u64 = 256 * 1024;

/// How many small files make an archive worth building. Below this the archive is the overhead.
pub const BUNDLE_MIN_SMALL_FILES: u64 = 64;

/// What a plan says about bundling, and the numbers behind the judgement.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BundlingHint {
    pub small_files: u64,
    pub small_file_bytes: u64,
    pub bundle_recommended: bool,
}

impl CopyPlan {
    /// Whether this plan's file set would move faster as one archive than as individual files.
    ///
    /// Bundling trades per-file round trips for one sequential stream, so it pays only where each
    /// file has a round-trip cost and there are enough of them for the archive to pay for itself.
    /// Measured on the SMB share this engine was built for: 1000 files of 64 KiB moved **2.1x**
    /// faster as a single tar than file-by-file. On a local or same-volume destination the
    /// recommendation is `false` even for a huge small-file tree, because the kernel already
    /// amortises that cost - which is why this is keyed to the destination's path class and not
    /// to file size alone.
    pub fn bundling_hint(&self) -> BundlingHint {
        let mut hint = BundlingHint::default();
        for action in &self.actions {
            if matches!(action.kind, CopyActionKind::Copy) && action.bytes <= SMALL_FILE_BYTES {
                hint.small_files += 1;
                hint.small_file_bytes += action.bytes;
            }
        }
        hint.bundle_recommended = hint.small_files >= BUNDLE_MIN_SMALL_FILES
            && classify_paths(&self.job.source, &self.job.destination) == PathClass::Smb;
        hint
    }
}

impl CopyJob {
    pub fn copy(source: impl Into<PathBuf>, destination: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            destination: destination.into(),
            mode: CopyMode::Copy,
            skip: SkipPolicy::SizeMtime,
            verify: VerifyPolicy::None,
            metadata: MetadataPolicy::DataOnly,
            link_policy: LinkPolicy::Skip,
            error_policy: ErrorPolicy::Strict,
            threads: 1,
            buffer_size_bytes: DEFAULT_BUFFER_SIZE_BYTES,
            dry_run: false,
            manifest: None,
        }
    }
}

/// The policy that is really executed for a verification request. `CopyMode::Verify` with
/// `VerifyPolicy::None` asks for verification by the mode itself, so it verifies by full
/// content hash; every other combination maps 1:1. Mode is folded in here rather than left to
/// each call site, so a Copy job with `None` can never accidentally verify everything.
fn effective_verify_policy(job: &CopyJob) -> VerifyPolicy {
    match (&job.verify, &job.mode) {
        (VerifyPolicy::None, CopyMode::Verify) => VerifyPolicy::FullHash,
        (policy, _) => policy.clone(),
    }
}

/// A copy action's reason with the verification execution will really perform appended, so a
/// plan never advertises more - or less - than `execute` does.
fn copy_reason(job: &CopyJob, reason: &str) -> String {
    match job.verify {
        VerifyPolicy::None => reason.to_string(),
        ref policy => format!("{reason}; verify ({policy:?}) after copy"),
    }
}

/// Reject policy combinations this build cannot honour. Everything selectable through the
/// API - including whatever the desktop app maps onto it - either does something real or
/// fails loudly here, instead of being accepted and quietly ignored. Called from `plan()` and
/// from `execute()`, so a hand-built `CopyPlan` cannot bypass it.
fn validate_policies(job: &CopyJob) -> Result<(), CopyError> {
    match &job.metadata {
        MetadataPolicy::Security => {
            return Err(unsupported(
                Some(job.source.clone()),
                "metadata policy Security (SELinux contexts / POSIX ACLs) is not implemented in \
                 this build; select DataOnly, Timestamps, Attributes or All",
            ));
        }
        MetadataPolicy::Owner => {
            return Err(unsupported(
                Some(job.source.clone()),
                "metadata policy Owner (uid/gid preservation) is not implemented in this build; \
                 select DataOnly, Timestamps, Attributes or All",
            ));
        }
        _ => {}
    }

    if matches!(job.verify, VerifyPolicy::Manifest) {
        let Some(manifest) = job.manifest.as_ref() else {
            return Err(unsupported(
                Some(job.source.clone()),
                "verify policy Manifest needs a manifest to check the result against; give one, or \
                 select FullHash, SampledHash or Size",
            ));
        };
        // Read and parse it up front: a manifest that cannot be read or understood is refused before
        // the transfer rather than discovered after it, and the failure is then about the manifest
        // instead of about the data that was copied perfectly well.
        let text = fs::read_to_string(manifest).map_err(|err| {
            manifest_error(
                Some(manifest.clone()),
                format!("cannot read manifest: {err}"),
                CopyErrorCategory::Io,
            )
        })?;
        parse_manifest(&text)?;
    }

    Ok(())
}

/// The engine's content hash of a file: BLAKE3, streamed in 1 MiB blocks, hex-encoded.
///
/// Public because callers outside the engine kept re-implementing exactly this, and the copies
/// drifted: an earlier size+mtime "proxy" in `transfer.rs` skipped hashing large files entirely,
/// so a content change that preserved both size and mtime was indistinguishable from no change.
/// One implementation means one place to fix that.
pub fn hash_file_hex(path: &Path) -> Result<String, CopyError> {
    hash_file(path)
}

/// Whether `destination` already holds `source`'s content, judged by `skip`'s rules.
///
/// The engine's skip semantics as a standalone decision, so a caller that SCANS rather than copies
/// can reach the same verdict instead of deriving its own. The variants differ in cost, not just
/// strictness: `SizeMtime` reads no file content at all, while `SizeMtimeHash` and `Hash` read
/// BOTH files in full - which is why the choice belongs to the caller and is never made silently.
pub fn destination_matches(
    source: &Path,
    destination: &Path,
    skip: &SkipPolicy,
) -> Result<bool, CopyError> {
    let source_meta = fs::metadata(source).map_err(|err| {
        io_error(
            Some(source.to_path_buf()),
            format!("cannot stat source: {err}"),
            true,
        )
    })?;
    let destination_meta = fs::metadata(destination).map_err(|err| {
        io_error(
            Some(destination.to_path_buf()),
            format!("cannot stat destination: {err}"),
            true,
        )
    })?;
    destination_matches_inner(source, &source_meta, destination, &destination_meta, skip)
}

pub fn plan(mut job: CopyJob) -> Result<CopyPlan, CopyError> {
    let started = std::time::Instant::now();
    // Pipeline step 1: normalise. Callers hand over relative paths, `.`/`..` segments, trailing
    // slashes or paths under a symlinked parent; the plan, the report and the resume journal are
    // all built from these paths, so they are resolved once here instead of being left for every
    // caller (and every future caller) to get right. See `normalise_path` for what is and is not
    // resolved. Nothing existing is touched: this is lexical work plus a parent lookup.
    job.source = normalise_path(&job.source);
    job.destination = normalise_path(&job.destination);
    validate_policies(&job)?;
    // Symlink-aware existence check: `Preserve` also copies dangling links, which
    // `Path::exists` would reject because it follows the link.
    if fs::symlink_metadata(&job.source).is_err() {
        return Err(invalid_input(
            Some(job.source.clone()),
            "source path does not exist",
        ));
    }

    // `CopyMode::Resume` is honoured by the copy path: each file continues from its
    // deterministic partial sibling (see partial_sibling) when one exists and the
    // destination is still absent. `plan` itself needs no special case.

    let mut actions = Vec::new();

    let source_is_symlink = fs::symlink_metadata(&job.source)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false);
    if source_is_symlink && matches!(job.link_policy, LinkPolicy::Preserve) {
        // An explicitly named symlink is recreated rather than dereferenced under
        // `Preserve`, so nothing about the link target is read.
        actions.push(CopyAction {
            kind: CopyActionKind::Copy,
            source: Some(job.source.clone()),
            destination: job.destination.clone(),
            relative_path: PathBuf::new(),
            bytes: fs::symlink_metadata(&job.source)
                .map(|meta| meta.len())
                .unwrap_or(0),
            reason: "source symlink preserved as symlink".to_string(),
        });
        return finish_plan(job, actions, started);
    }

    let source_meta = fs::metadata(&job.source).map_err(|err| {
        io_error(
            Some(job.source.clone()),
            format!("cannot stat source: {err}"),
            true,
        )
    })?;

    if source_meta.is_file() {
        plan_file(
            &job,
            &job.source,
            &job.destination,
            Path::new(""),
            source_meta,
            &mut actions,
        )?;
    } else if source_meta.is_dir() {
        let mut ancestors = Vec::new();
        plan_dir(
            &job,
            &job.source,
            &job.destination,
            Path::new(""),
            &mut actions,
            &mut ancestors,
        )?;
        if matches!(job.mode, CopyMode::Mirror) {
            plan_mirror_deletes(
                &job.destination,
                &job.source,
                &job.destination,
                &mut actions,
            )?;
        }
    } else {
        return Err(unsupported(
            Some(job.source.clone()),
            "source path is not a regular file or directory",
        ));
    }

    finish_plan(job, actions, started)
}

/// Assemble the plan and its counters once every action is known. Split out so the
/// symlink `Preserve` path can return a plan without duplicating the counting rules.
fn finish_plan(
    job: CopyJob,
    actions: Vec<CopyAction>,
    started: std::time::Instant,
) -> Result<CopyPlan, CopyError> {
    let total_files = actions
        .iter()
        .filter(|action| matches!(action.kind, CopyActionKind::Copy | CopyActionKind::Verify))
        .count() as u64;
    let skipped_files = actions
        .iter()
        .filter(|action| matches!(action.kind, CopyActionKind::Skip))
        .count() as u64;
    let delete_files = actions
        .iter()
        .filter(|action| matches!(action.kind, CopyActionKind::Delete))
        .count() as u64;
    let total_bytes = actions
        .iter()
        .filter(|action| matches!(action.kind, CopyActionKind::Copy | CopyActionKind::Verify))
        .map(|action| action.bytes)
        .sum();

    let mut plan = CopyPlan {
        job,
        actions,
        total_files,
        total_bytes,
        skipped_files,
        delete_files,
        timings: CopyTimings {
            plan_ms: started.elapsed().as_millis() as u64,
            ..CopyTimings::default()
        },
    };
    // An explicit count belongs to the caller; zero means "derive it", and that cannot be answered
    // until the tree has been walked - it depends on how many small files are in it - so it is
    // resolved here, once, instead of by every caller. The plan then carries the count that will
    // actually be used, which is what the CLI, the `.tl` binding and the app report back.
    if plan.job.threads == 0 {
        plan.job.threads = recommended_threads_for_plan(&plan);
    }
    Ok(plan)
}

pub fn execute(plan: &CopyPlan) -> CopyReport {
    execute_with_progress(plan, |_| {})
}

pub fn execute_with_progress<F>(plan: &CopyPlan, progress: F) -> CopyReport
where
    F: FnMut(CopyProgressEvent),
{
    execute_with_control(plan, &CopyControl::new(), progress)
}

/// Run a plan, then apply the checks that belong to the finished result rather than to any one file.
///
/// The wrapper exists so every route out of a transfer - serial, parallel, or an early return -
/// gets the same treatment; honouring the manifest policy on some paths only would be worse than not
/// offering it.
pub fn execute_with_control<F>(
    plan: &CopyPlan,
    control: &CopyControl,
    progress: F,
) -> CopyReport
where
    F: FnMut(CopyProgressEvent),
{
    let mut report = execute_with_control_inner(plan, control, progress);
    apply_manifest_verification(plan, control, &mut report);
    report
}

fn execute_with_control_inner<F>(
    plan: &CopyPlan,
    control: &CopyControl,
    mut progress: F,
) -> CopyReport
where
    F: FnMut(CopyProgressEvent),
{
    // Checked before any work: an embedder holding a CopyPlan directly (or an app version
    // that predates validation) must not be able to run a policy this build cannot honour.
    if let Err(error) = validate_policies(&plan.job) {
        return CopyReport {
            errors: vec![error],
            ..CopyReport::default()
        };
    }

    let dry_run = plan.job.dry_run || matches!(plan.job.mode, CopyMode::DryRun);
    if should_parallel_copy(plan, dry_run) {
        return execute_parallel_copy(plan, control, progress);
    }

    let copy_started = std::time::Instant::now();
    let mut report = CopyReport::default();
    // ErrorPolicy was declared, settable and never read: the engine behaved as BestEffort
    // whatever a caller chose, including its own default of Strict. This path honours it now.
    let strict = plan.job.error_policy == ErrorPolicy::Strict;
    let mut files_done = 0_u64;
    let mut aggregate_bytes_done = 0_u64;
    // One buffer for the whole job, sized per file on first use. Allocating a fresh
    // zero-filled buffer per file cost up to buffer_size/file_size of pure memset on
    // trees of small files (a 4 KiB file behind a 32 MiB buffer: 8192x).
    let mut reuse_buffer: Vec<u8> = Vec::new();
    // The policy that will really run: CopyMode::Verify with the default None policy still
    // verifies, by full hash. Manifest is refused inside verify_after_copy.
    let policy = effective_verify_policy(&plan.job);
    let verify_enabled = !matches!(policy, VerifyPolicy::None);
    // Only the whole-file policies can reuse the hash produced during the copy.
    let hash_source = matches!(
        policy,
        VerifyPolicy::FullHash | VerifyPolicy::ReadAfterWrite
    );
    let resume = matches!(plan.job.mode, CopyMode::Resume);

    for action in &plan.actions {
        // ErrorPolicy::Strict ends the run at the first failure: the push sites inside this
        // loop record the error, and this is where the next action stops being attempted.
        if strict && !report.errors.is_empty() {
            break;
        }
        if control.is_cancelled() {
            let error = cancelled_error(Some(action.destination.clone()));
            progress(with_aggregate(
                progress_event(
                    CopyProgressKind::Error,
                    action,
                    0,
                    files_done,
                    plan.total_files,
                    Some(error.clone()),
                ),
                aggregate_bytes_done,
                plan.total_bytes,
            ));
            report.errors.push(error);
            break;
        }
        match action.kind {
            CopyActionKind::Copy => {
                if dry_run {
                    continue;
                }
                let Some(source) = &action.source else {
                    let error = invalid_input(
                        Some(action.destination.clone()),
                        "copy action missing source",
                    );
                    progress(with_aggregate(
                        progress_event(
                            CopyProgressKind::Error,
                            action,
                            0,
                            files_done,
                            plan.total_files,
                            Some(error.clone()),
                        ),
                        aggregate_bytes_done,
                        plan.total_bytes,
                    ));
                    report.errors.push(error);
                    continue;
                };
                progress(with_aggregate(
                    progress_event(
                        CopyProgressKind::FileStarted,
                        action,
                        0,
                        files_done,
                        plan.total_files,
                        None,
                    ),
                    aggregate_bytes_done,
                    plan.total_bytes,
                ));
                match copy_file_bounded_with_progress(
                    source,
                    &action.destination,
                    action,
                    files_done,
                    plan.total_files,
                    plan.job.buffer_size_bytes,
                    &plan.job.metadata,
                    &plan.job.link_policy,
                    hash_source,
                    resume,
                    &mut reuse_buffer,
                    &mut |event| {
                        let bytes_done = aggregate_bytes_done.saturating_add(event.bytes_done);
                        progress(with_aggregate(event, bytes_done, plan.total_bytes));
                    },
                    control,
                ) {
                    Ok(outcome) => {
                        let bytes = outcome.bytes;
                        report.worker_threads_used = report.worker_threads_used.max(1);
                        report.copied_files += 1;
                        report.bytes_copied += bytes;
                        aggregate_bytes_done = aggregate_bytes_done.saturating_add(bytes);
                        files_done += 1;
                        progress(with_aggregate(
                            progress_event(
                                CopyProgressKind::FileFinished,
                                action,
                                bytes,
                                files_done,
                                plan.total_files,
                                None,
                            ),
                            aggregate_bytes_done,
                            plan.total_bytes,
                        ));
                        if verify_enabled {
                            // The source was hashed during the copy itself, so verification
                            // only has to read the destination back.
                            verify_after_copy(
                                source,
                                &action.destination,
                                &policy,
                                outcome.source_hash.as_deref(),
                                &mut report,
                            );
                        }
                    }
                    Err(err) => {
                        progress(with_aggregate(
                            progress_event(
                                CopyProgressKind::Error,
                                action,
                                0,
                                files_done,
                                plan.total_files,
                                Some(err.clone()),
                            ),
                            aggregate_bytes_done,
                            plan.total_bytes,
                        ));
                        report.errors.push(err);
                    }
                }
            }
            CopyActionKind::Skip => {
                report.skipped_files += 1;
                files_done += 1;
                progress(with_aggregate(
                    progress_event(
                        CopyProgressKind::FileSkipped,
                        action,
                        action.bytes,
                        files_done,
                        plan.total_files,
                        None,
                    ),
                    aggregate_bytes_done,
                    plan.total_bytes,
                ));
            }
            CopyActionKind::Verify => {
                let Some(source) = &action.source else {
                    let error = invalid_input(
                        Some(action.destination.clone()),
                        "verify action missing source",
                    );
                    progress(with_aggregate(
                        progress_event(
                            CopyProgressKind::Error,
                            action,
                            0,
                            files_done,
                            plan.total_files,
                            Some(error.clone()),
                        ),
                        aggregate_bytes_done,
                        plan.total_bytes,
                    ));
                    report.errors.push(error);
                    continue;
                };
                verify_after_copy(source, &action.destination, &policy, None, &mut report);
                files_done += 1;
                aggregate_bytes_done = aggregate_bytes_done.saturating_add(action.bytes);
                progress(with_aggregate(
                    progress_event(
                        CopyProgressKind::FileFinished,
                        action,
                        action.bytes,
                        files_done,
                        plan.total_files,
                        None,
                    ),
                    aggregate_bytes_done,
                    plan.total_bytes,
                ));
            }
            CopyActionKind::Delete => {
                if dry_run {
                    continue;
                }
                match fs::remove_file(&action.destination) {
                    Ok(()) => {
                        report.deleted_files += 1;
                        progress(with_aggregate(
                            progress_event(
                                CopyProgressKind::FileDeleted,
                                action,
                                action.bytes,
                                files_done,
                                plan.total_files,
                                None,
                            ),
                            aggregate_bytes_done,
                            plan.total_bytes,
                        ));
                    }
                    Err(err) => {
                        let error = io_error(
                            Some(action.destination.clone()),
                            format!("delete failed: {err}"),
                            true,
                        );
                        progress(with_aggregate(
                            progress_event(
                                CopyProgressKind::Error,
                                action,
                                0,
                                files_done,
                                plan.total_files,
                                Some(error.clone()),
                            ),
                            aggregate_bytes_done,
                            plan.total_bytes,
                        ));
                        report.errors.push(error);
                    }
                }
            }
            CopyActionKind::Mkdir => {
                if dry_run {
                    continue;
                }
                if let Err(err) = fs::create_dir_all(&action.destination) {
                    report.errors.push(io_error(
                        Some(action.destination.clone()),
                        format!("mkdir failed: {err}"),
                        true,
                    ));
                }
            }
            CopyActionKind::Metadata | CopyActionKind::Error => {}
        }
    }

    report.timings.plan_ms = plan.timings.plan_ms;
    report.timings.copy_ms = copy_started.elapsed().as_millis() as u64;
    report
}

fn should_parallel_copy(plan: &CopyPlan, dry_run: bool) -> bool {
    if dry_run || plan.job.threads <= 1 {
        return false;
    }
    // Verification no longer forces the serial path: the source is hashed during the
    // copy and the read-back happens in the collector, so verify-mode jobs get the
    // same worker pool as unverified ones.
    plan.actions
        .iter()
        .filter(|action| matches!(action.kind, CopyActionKind::Copy))
        .count()
        > 1
}

fn execute_parallel_copy<F>(plan: &CopyPlan, control: &CopyControl, mut progress: F) -> CopyReport
where
    F: FnMut(CopyProgressEvent),
{
    let copy_started = std::time::Instant::now();
    let mut report = CopyReport::default();

    let mut files_done = 0_u64;
    let mut aggregate_bytes_done = 0_u64;

    for action in plan
        .actions
        .iter()
        .filter(|action| matches!(action.kind, CopyActionKind::Mkdir))
    {
        if let Err(err) = fs::create_dir_all(&action.destination) {
            report.errors.push(io_error(
                Some(action.destination.clone()),
                format!("mkdir failed: {err}"),
                true,
            ));
        }
    }

    for action in plan.actions.iter().filter(|action| {
        matches!(
            action.kind,
            CopyActionKind::Skip
                | CopyActionKind::Verify
                | CopyActionKind::Metadata
                | CopyActionKind::Error
        )
    }) {
        process_non_copy_action(
            plan,
            action,
            &mut report,
            &mut files_done,
            &mut aggregate_bytes_done,
            &mut progress,
        );
    }

    execute_copy_actions_in_parallel(
        plan,
        control,
        &mut report,
        &mut files_done,
        &mut aggregate_bytes_done,
        &mut progress,
    );

    for action in plan
        .actions
        .iter()
        .filter(|action| matches!(action.kind, CopyActionKind::Delete))
    {
        process_delete_action(
            plan,
            action,
            &mut report,
            files_done,
            aggregate_bytes_done,
            &mut progress,
        );
    }

    report.timings.plan_ms = plan.timings.plan_ms;
    report.timings.copy_ms = copy_started.elapsed().as_millis() as u64;
    report
}

fn process_non_copy_action<F>(
    plan: &CopyPlan,
    action: &CopyAction,
    report: &mut CopyReport,
    files_done: &mut u64,
    aggregate_bytes_done: &mut u64,
    progress: &mut F,
) where
    F: FnMut(CopyProgressEvent),
{
    match action.kind {
        CopyActionKind::Skip => {
            report.skipped_files += 1;
            *files_done += 1;
            progress(with_aggregate(
                progress_event(
                    CopyProgressKind::FileSkipped,
                    action,
                    action.bytes,
                    *files_done,
                    plan.total_files,
                    None,
                ),
                *aggregate_bytes_done,
                plan.total_bytes,
            ));
        }
        CopyActionKind::Verify => {
            let Some(source) = &action.source else {
                let error = invalid_input(
                    Some(action.destination.clone()),
                    "verify action missing source",
                );
                progress(with_aggregate(
                    progress_event(
                        CopyProgressKind::Error,
                        action,
                        0,
                        *files_done,
                        plan.total_files,
                        Some(error.clone()),
                    ),
                    *aggregate_bytes_done,
                    plan.total_bytes,
                ));
                report.errors.push(error);
                return;
            };
            verify_after_copy(
                source,
                &action.destination,
                &effective_verify_policy(&plan.job),
                None,
                report,
            );
            *files_done += 1;
            *aggregate_bytes_done = (*aggregate_bytes_done).saturating_add(action.bytes);
            progress(with_aggregate(
                progress_event(
                    CopyProgressKind::FileFinished,
                    action,
                    action.bytes,
                    *files_done,
                    plan.total_files,
                    None,
                ),
                *aggregate_bytes_done,
                plan.total_bytes,
            ));
        }
        CopyActionKind::Metadata | CopyActionKind::Error => {}
        CopyActionKind::Copy | CopyActionKind::Delete | CopyActionKind::Mkdir => {}
    }
}

fn process_delete_action<F>(
    plan: &CopyPlan,
    action: &CopyAction,
    report: &mut CopyReport,
    files_done: u64,
    aggregate_bytes_done: u64,
    progress: &mut F,
) where
    F: FnMut(CopyProgressEvent),
{
    match fs::remove_file(&action.destination) {
        Ok(()) => {
            report.deleted_files += 1;
            progress(with_aggregate(
                progress_event(
                    CopyProgressKind::FileDeleted,
                    action,
                    action.bytes,
                    files_done,
                    plan.total_files,
                    None,
                ),
                aggregate_bytes_done,
                plan.total_bytes,
            ));
        }
        Err(err) => {
            let error = io_error(
                Some(action.destination.clone()),
                format!("delete failed: {err}"),
                true,
            );
            progress(with_aggregate(
                progress_event(
                    CopyProgressKind::Error,
                    action,
                    0,
                    files_done,
                    plan.total_files,
                    Some(error.clone()),
                ),
                aggregate_bytes_done,
                plan.total_bytes,
            ));
            report.errors.push(error);
        }
    }
}

enum WorkerMessage {
    Progress(CopyProgressEvent),
    Finished {
        action: CopyAction,
        result: Result<CopyOutcome, CopyError>,
    },
}

fn execute_copy_actions_in_parallel<F>(
    plan: &CopyPlan,
    control: &CopyControl,
    report: &mut CopyReport,
    files_done: &mut u64,
    aggregate_bytes_done: &mut u64,
    progress: &mut F,
) where
    F: FnMut(CopyProgressEvent),
{
    let copy_actions = plan
        .actions
        .iter()
        .filter(|action| matches!(action.kind, CopyActionKind::Copy))
        .cloned()
        .collect::<Vec<_>>();
    if copy_actions.is_empty() {
        return;
    }
    if control.is_cancelled() {
        report.errors.push(cancelled_error(None));
        return;
    }

    let worker_count = plan.job.threads.min(copy_actions.len()).max(1);
    report.worker_threads_used = worker_count;
    let copy_actions = Arc::new(copy_actions);
    let next_action = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::channel::<WorkerMessage>();
    let mut handles = Vec::with_capacity(worker_count);

    // Computed once, outside the worker loop: the workers need `hash_source`/`resume`, and
    // the collector needs `verify_enabled` to decide whether to verify a finished file.
    let policy = effective_verify_policy(&plan.job);
    let verify_enabled = !matches!(policy, VerifyPolicy::None);
    let hash_source = matches!(
        policy,
        VerifyPolicy::FullHash | VerifyPolicy::ReadAfterWrite
    );
    let resume = matches!(plan.job.mode, CopyMode::Resume);

    for _ in 0..worker_count {
        let actions = Arc::clone(&copy_actions);
        let next = Arc::clone(&next_action);
        let sender = sender.clone();
        let total_files = plan.total_files;
        let buffer_size_bytes = plan.job.buffer_size_bytes;
        let metadata_policy = plan.job.metadata.clone();
        let link_policy = plan.job.link_policy.clone();
        let control = control.clone();
        handles.push(thread::spawn(move || {
            // One buffer per worker thread, reused across every file it copies.
            let mut reuse_buffer: Vec<u8> = Vec::new();
            loop {
                if control.is_cancelled() {
                    break;
                }
                let index = next.fetch_add(1, Ordering::SeqCst);
                let Some(action) = actions.get(index).cloned() else {
                    break;
                };
                let Some(source) = action.source.clone() else {
                    let error = invalid_input(
                        Some(action.destination.clone()),
                        "copy action missing source",
                    );
                    let _ = sender.send(WorkerMessage::Progress(progress_event(
                        CopyProgressKind::Error,
                        &action,
                        0,
                        0,
                        total_files,
                        Some(error.clone()),
                    )));
                    let _ = sender.send(WorkerMessage::Finished {
                        action,
                        result: Err(error),
                    });
                    continue;
                };
                let _ = sender.send(WorkerMessage::Progress(progress_event(
                    CopyProgressKind::FileStarted,
                    &action,
                    0,
                    0,
                    total_files,
                    None,
                )));
                let result = copy_file_bounded_with_progress(
                    &source,
                    &action.destination,
                    &action,
                    0,
                    total_files,
                    buffer_size_bytes,
                    &metadata_policy,
                    &link_policy,
                    hash_source,
                    resume,
                    &mut reuse_buffer,
                    &mut |event| {
                        let _ = sender.send(WorkerMessage::Progress(event));
                    },
                    &control,
                );
                let _ = sender.send(WorkerMessage::Finished { action, result });
            }
        }));
    }
    drop(sender);

    let mut active_copy_bytes = HashMap::<PathBuf, u64>::new();
    // ErrorPolicy::Strict cancels the worker pool when a file fails instead of draining
    // the queue; the check is inside the collector loop below.
    let strict = plan.job.error_policy == ErrorPolicy::Strict;
    for message in receiver {
        // Strict: a worker reported a failure, so cancel the pool and stop collecting.
        if strict && !report.errors.is_empty() {
            control.cancel();
            break;
        }
        match message {
            WorkerMessage::Progress(mut event) => {
                event.files_done = *files_done;
                match event.kind {
                    CopyProgressKind::FileStarted => {
                        active_copy_bytes.insert(event.destination.clone(), 0);
                    }
                    CopyProgressKind::BytesCopied => {
                        active_copy_bytes.insert(event.destination.clone(), event.bytes_done);
                    }
                    CopyProgressKind::Error => {
                        active_copy_bytes.remove(&event.destination);
                    }
                    CopyProgressKind::FileFinished
                    | CopyProgressKind::FileSkipped
                    | CopyProgressKind::FileDeleted => {}
                }
                let active_bytes = active_copy_bytes.values().copied().sum::<u64>();
                event = with_aggregate(
                    event,
                    (*aggregate_bytes_done).saturating_add(active_bytes),
                    plan.total_bytes,
                );
                progress(event);
            }
            WorkerMessage::Finished { action, result } => match result {
                Ok(outcome) => {
                    let bytes = outcome.bytes;
                    active_copy_bytes.remove(&action.destination);
                    report.copied_files += 1;
                    report.bytes_copied += bytes;
                    *aggregate_bytes_done = (*aggregate_bytes_done).saturating_add(bytes);
                    if verify_enabled {
                        if let Some(source) = action.source.as_deref() {
                            verify_after_copy(
                                source,
                                &action.destination,
                                &policy,
                                outcome.source_hash.as_deref(),
                                &mut *report,
                            );
                        }
                    }
                    *files_done += 1;
                    let active_bytes = active_copy_bytes.values().copied().sum::<u64>();
                    progress(with_aggregate(
                        progress_event(
                            CopyProgressKind::FileFinished,
                            &action,
                            bytes,
                            *files_done,
                            plan.total_files,
                            None,
                        ),
                        (*aggregate_bytes_done).saturating_add(active_bytes),
                        plan.total_bytes,
                    ));
                }
                Err(error) => {
                    active_copy_bytes.remove(&action.destination);
                    let active_bytes = active_copy_bytes.values().copied().sum::<u64>();
                    progress(with_aggregate(
                        progress_event(
                            CopyProgressKind::Error,
                            &action,
                            0,
                            *files_done,
                            plan.total_files,
                            Some(error.clone()),
                        ),
                        (*aggregate_bytes_done).saturating_add(active_bytes),
                        plan.total_bytes,
                    ));
                    report.errors.push(error);
                }
            },
        }
    }

    for handle in handles {
        if handle.join().is_err() {
            report.errors.push(CopyError {
                category: CopyErrorCategory::Io,
                path: None,
                message: "copy worker panicked".to_string(),
                retryable: true,
                recommended_action: "retry".to_string(),
            });
        }
    }
}

fn progress_event(
    kind: CopyProgressKind,
    action: &CopyAction,
    bytes_done: u64,
    files_done: u64,
    files_total: u64,
    error: Option<CopyError>,
) -> CopyProgressEvent {
    CopyProgressEvent {
        kind,
        action_kind: action.kind.clone(),
        source: action.source.clone(),
        destination: action.destination.clone(),
        relative_path: action.relative_path.clone(),
        bytes_done,
        bytes_total: action.bytes,
        aggregate_bytes_done: 0,
        aggregate_bytes_total: 0,
        files_done,
        files_total,
        error,
    }
}

fn with_aggregate(
    mut event: CopyProgressEvent,
    aggregate_bytes_done: u64,
    aggregate_bytes_total: u64,
) -> CopyProgressEvent {
    event.aggregate_bytes_done = aggregate_bytes_done.min(aggregate_bytes_total);
    event.aggregate_bytes_total = aggregate_bytes_total;
    event
}

fn plan_dir(
    job: &CopyJob,
    source_dir: &Path,
    destination_dir: &Path,
    relative_dir: &Path,
    actions: &mut Vec<CopyAction>,
    ancestors: &mut Vec<PathBuf>,
) -> Result<(), CopyError> {
    // Loop guard for `LinkPolicy::Follow`: a symlink pointing back at a directory already on
    // this traversal chain must not be followed forever.
    let mut pushed_ancestor = false;
    if let Ok(canonical) = fs::canonicalize(source_dir) {
        if ancestors.contains(&canonical) {
            return Err(unsupported(
                Some(source_dir.to_path_buf()),
                "symlink loop detected while following links; select LinkPolicy::Preserve or Skip",
            ));
        }
        ancestors.push(canonical);
        pushed_ancestor = true;
    }
    if !destination_dir.exists() {
        actions.push(CopyAction {
            kind: CopyActionKind::Mkdir,
            source: Some(source_dir.to_path_buf()),
            destination: destination_dir.to_path_buf(),
            relative_path: relative_dir.to_path_buf(),
            bytes: 0,
            reason: "destination directory missing".to_string(),
        });
    }

    for entry in fs::read_dir(source_dir).map_err(|err| {
        io_error(
            Some(source_dir.to_path_buf()),
            format!("cannot read directory: {err}"),
            true,
        )
    })? {
        let entry = entry.map_err(|err| {
            io_error(
                Some(source_dir.to_path_buf()),
                format!("cannot read directory entry: {err}"),
                true,
            )
        })?;
        let source_path = entry.path();
        let relative_path = relative_dir.join(entry.file_name());
        let destination_path = destination_dir.join(entry.file_name());
        // `DirEntry::file_type` does not traverse symlinks, so a link is visible here as a
        // link and `LinkPolicy` decides what happens to it.
        let file_type = entry.file_type().map_err(|err| {
            io_error(
                Some(source_path.clone()),
                format!("cannot stat entry: {err}"),
                true,
            )
        })?;

        if file_type.is_symlink() {
            let link_bytes = fs::symlink_metadata(&source_path)
                .map(|meta| meta.len())
                .unwrap_or(0);
            match job.link_policy {
                LinkPolicy::Skip => actions.push(CopyAction {
                    kind: CopyActionKind::Skip,
                    source: Some(source_path.clone()),
                    destination: destination_path,
                    relative_path,
                    bytes: link_bytes,
                    reason: "symlink skipped by link policy".to_string(),
                }),
                LinkPolicy::Preserve => actions.push(CopyAction {
                    kind: CopyActionKind::Copy,
                    source: Some(source_path.clone()),
                    destination: destination_path,
                    relative_path,
                    bytes: link_bytes,
                    reason: "symlink preserved as symlink".to_string(),
                }),
                LinkPolicy::Follow => {
                    let target_meta = fs::metadata(&source_path).map_err(|err| {
                        io_error(
                            Some(source_path.clone()),
                            format!("cannot follow symlink: {err}"),
                            true,
                        )
                    })?;
                    if target_meta.is_dir() {
                        plan_dir(
                            job,
                            &source_path,
                            &destination_path,
                            &relative_path,
                            actions,
                            ancestors,
                        )?;
                    } else if target_meta.is_file() {
                        plan_file(
                            job,
                            &source_path,
                            &destination_path,
                            &relative_path,
                            target_meta,
                            actions,
                        )?;
                    } else {
                        return Err(unsupported(
                            Some(source_path.clone()),
                            "symlink target is not a regular file or directory",
                        ));
                    }
                }
            }
            continue;
        }

        let metadata = entry.metadata().map_err(|err| {
            io_error(
                Some(source_path.clone()),
                format!("cannot stat entry: {err}"),
                true,
            )
        })?;

        if metadata.is_dir() {
            plan_dir(
                job,
                &source_path,
                &destination_path,
                &relative_path,
                actions,
                ancestors,
            )?;
        } else if metadata.is_file() {
            plan_file(
                job,
                &source_path,
                &destination_path,
                &relative_path,
                metadata,
                actions,
            )?;
        }
    }

    if pushed_ancestor {
        ancestors.pop();
    }

    Ok(())
}

fn plan_file(
    job: &CopyJob,
    source: &Path,
    destination: &Path,
    relative_path: &Path,
    source_meta: fs::Metadata,
    actions: &mut Vec<CopyAction>,
) -> Result<(), CopyError> {
    let bytes = source_meta.len();
    let action = if !destination.exists() {
        CopyAction {
            kind: CopyActionKind::Copy,
            source: Some(source.to_path_buf()),
            destination: destination.to_path_buf(),
            relative_path: relative_path.to_path_buf(),
            bytes,
            reason: copy_reason(job, "destination missing"),
        }
    } else {
        let destination_meta = fs::metadata(destination).map_err(|err| {
            io_error(
                Some(destination.to_path_buf()),
                format!("cannot stat destination: {err}"),
                true,
            )
        })?;
        if destination_matches_inner(
            source,
            &source_meta,
            destination,
            &destination_meta,
            &job.skip,
        )? {
            // Any verify policy other than None asks for verification of a destination that
            // looks unchanged, and so does CopyMode::Verify. Listing the policies inline here
            // is how Size and SampledHash came to plan a Skip instead of a Verify.
            if matches!(job.mode, CopyMode::Verify) || !matches!(job.verify, VerifyPolicy::None) {
                CopyAction {
                    kind: CopyActionKind::Verify,
                    source: Some(source.to_path_buf()),
                    destination: destination.to_path_buf(),
                    relative_path: relative_path.to_path_buf(),
                    bytes,
                    reason: format!(
                        "destination appears unchanged; verify requested ({:?})",
                        effective_verify_policy(job)
                    ),
                }
            } else {
                CopyAction {
                    kind: CopyActionKind::Skip,
                    source: Some(source.to_path_buf()),
                    destination: destination.to_path_buf(),
                    relative_path: relative_path.to_path_buf(),
                    bytes,
                    reason: "unchanged by skip policy".to_string(),
                }
            }
        } else {
            CopyAction {
                kind: CopyActionKind::Copy,
                source: Some(source.to_path_buf()),
                destination: destination.to_path_buf(),
                relative_path: relative_path.to_path_buf(),
                bytes,
                reason: copy_reason(job, "changed by skip policy"),
            }
        }
    };

    actions.push(action);
    Ok(())
}

fn plan_mirror_deletes(
    destination_root: &Path,
    source_root: &Path,
    current_destination: &Path,
    actions: &mut Vec<CopyAction>,
) -> Result<(), CopyError> {
    if !current_destination.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(current_destination).map_err(|err| {
        io_error(
            Some(current_destination.to_path_buf()),
            format!("cannot read destination directory: {err}"),
            true,
        )
    })? {
        let entry = entry.map_err(|err| {
            io_error(
                Some(current_destination.to_path_buf()),
                format!("cannot read destination entry: {err}"),
                true,
            )
        })?;
        let destination_path = entry.path();
        let relative_path = destination_path
            .strip_prefix(destination_root)
            .unwrap_or(&destination_path)
            .to_path_buf();
        let source_path = source_root.join(&relative_path);
        let metadata = entry.metadata().map_err(|err| {
            io_error(
                Some(destination_path.clone()),
                format!("cannot stat destination entry: {err}"),
                true,
            )
        })?;

        // Both sides are compared WITHOUT following links, and that is what keeps deletions inside
        // the destination: a symlinked directory here is a link, not a tree to walk into, so mirror
        // can never delete through it. The same lstat view is why an orphaned LINK used to be skipped
        // entirely - a link is neither a file nor a directory - which contradicted mirror's contract
        // of deleting destination entries the source does not have. `remove_file` removes a link
        // itself and never what it points at, so a link is deleted as a link.
        let source_has_entry = source_path.symlink_metadata().is_ok();
        if !source_has_entry {
            let is_link = metadata.file_type().is_symlink();
            if metadata.is_file() || is_link {
                actions.push(CopyAction {
                    kind: CopyActionKind::Delete,
                    source: None,
                    destination: destination_path,
                    relative_path,
                    // A link's `len` is the length of its target path, not data, so it is not bytes.
                    bytes: if is_link { 0 } else { metadata.len() },
                    reason: "mirror target has no matching source".to_string(),
                });
            }
        } else if metadata.is_dir() {
            plan_mirror_deletes(destination_root, source_root, &destination_path, actions)?;
        }
    }

    Ok(())
}

fn destination_matches_inner(
    source: &Path,
    source_meta: &fs::Metadata,
    destination: &Path,
    destination_meta: &fs::Metadata,
    skip: &SkipPolicy,
) -> Result<bool, CopyError> {
    match skip {
        SkipPolicy::SizeMtime => Ok(source_meta.len() == destination_meta.len()
            && modified_seconds(source_meta) == modified_seconds(destination_meta)),
        SkipPolicy::SizeMtimeHash => {
            if source_meta.len() != destination_meta.len()
                || modified_seconds(source_meta) != modified_seconds(destination_meta)
            {
                return Ok(false);
            }
            Ok(hash_file(source)? == hash_file(destination)?)
        }
        SkipPolicy::Hash | SkipPolicy::Manifest => {
            Ok(hash_file(source)? == hash_file(destination)?)
        }
    }
}

fn modified_seconds(metadata: &fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

/// Buffer size for copying a file of `source_len` bytes: never below the minimum,
/// never above the configured size, and never larger than the file itself, so a
/// small file cannot be charged a large zero-fill.
fn copy_buffer_size(requested: usize, source_len: u64) -> usize {
    let floor = MIN_BUFFER_SIZE_BYTES as u64;
    let ceiling = requested.max(MIN_BUFFER_SIZE_BYTES) as u64;
    ceiling.min(source_len.max(floor)) as usize
}

/// Result of copying one file: the bytes moved, plus the source's BLAKE3 hash when
/// the caller asked for it. The hash is computed in the same pass as the copy, so
/// verification never has to read the source a second time.
struct CopyOutcome {
    bytes: u64,
    source_hash: Option<String>,
}

fn copy_file_bounded_with_progress<F>(
    source: &Path,
    destination: &Path,
    action: &CopyAction,
    files_done: u64,
    files_total: u64,
    buffer_size_bytes: usize,
    metadata_policy: &MetadataPolicy,
    link_policy: &LinkPolicy,
    hash_source: bool,
    resume: bool,
    buffer: &mut Vec<u8>,
    progress: &mut F,
    control: &CopyControl,
) -> Result<CopyOutcome, CopyError>
where
    F: FnMut(CopyProgressEvent),
{
    if control.is_cancelled() {
        return Err(cancelled_error(Some(destination.to_path_buf())));
    }

    // `LinkPolicy::Preserve` re-creates the symlink instead of copying the bytes of whatever
    // it points at. Skip and Follow never plan a symlink as a plain Copy, so this branch is
    // only reachable for a link that is meant to stay a link.
    if matches!(link_policy, LinkPolicy::Preserve) && is_symlink(source) {
        let bytes = copy_symlink_preserving(source, destination, control)?;
        return Ok(CopyOutcome {
            bytes,
            source_hash: None,
        });
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            io_error(
                Some(parent.to_path_buf()),
                format!("cannot create parent directory: {err}"),
                true,
            )
        })?;
    }

    let mut input = fs::File::open(source).map_err(|err| {
        io_error(
            Some(source.to_path_buf()),
            format!("cannot open source: {err}"),
            true,
        )
    })?;
    let source_len = input.metadata().map(|meta| meta.len()).unwrap_or(u64::MAX);

    // Resume: continue where an interrupted run stopped, but only when the destination is
    // absent. A finished destination means any partial beside it is stale, and resuming
    // onto a stale prefix would silently splice two different byte sequences together.
    let partial_path = partial_sibling(destination);
    let mut resume_offset = 0_u64;
    if resume && !destination.exists() {
        if let Ok(meta) = fs::metadata(&partial_path) {
            let staged = meta.len();
            if staged > 0 && staged < source_len {
                resume_offset = staged;
            }
        }
    }
    let resuming = resume_offset > 0;

    // Write to a sibling and rename into place: a crash or a full disk must never leave a
    // half-written destination behind, and the rename can replace a read-only destination,
    // which `File::create` refuses to do.
    //
    // In resume mode the target is the deterministic partial path and it is deliberately
    // NOT cleaned up on failure - that partial file is the point of a resume, so the next
    // run continues from it. Other modes keep the transient, uniquely-named temp file and
    // the guard that unlinks it on every error path.
    let write_path = if resume {
        partial_path.clone()
    } else {
        temp_sibling(destination)
    };
    let mut temp_cleanup = (!resume).then(|| TempCleanup::new(write_path.clone()));
    let mut output = if resuming {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&write_path)
            .map_err(|err| {
                io_error(
                    Some(destination.to_path_buf()),
                    format!("cannot open partial file for resume: {err}"),
                    true,
                )
            })?;
        file.seek(SeekFrom::Start(resume_offset)).map_err(|err| {
            io_error(
                Some(destination.to_path_buf()),
                format!("cannot seek partial file for resume: {err}"),
                true,
            )
        })?;
        file
    } else {
        fs::File::create(&write_path).map_err(|err| {
            io_error(
                Some(destination.to_path_buf()),
                format!("cannot create destination: {err}"),
                true,
            )
        })?
    };

    if resuming {
        input.seek(SeekFrom::Start(resume_offset)).map_err(|err| {
            io_error(
                Some(source.to_path_buf()),
                format!("cannot seek source for resume: {err}"),
                true,
            )
        })?;
    }

    // Reuse the caller's buffer across files and size it to the smaller of the
    // configured buffer and this file: a 4 KiB file must not pay a 32 MiB zero-fill,
    // and a large file must not re-allocate one for every file in the tree. It is sized
    // BEFORE the fast path because the fast path chunks at the same size, so progress and
    // cancellation keep the granularity the buffered loop would have given them.
    let wanted = copy_buffer_size(buffer_size_bytes, source_len);
    if buffer.len() < wanted {
        buffer.resize(wanted, 0_u8);
    }
    let buffer = buffer.as_mut_slice();
    // Kernel fast paths, before any buffered work: reflink shares extents, copy_file_range
    // moves the bytes inside the kernel. Both are opportunistic - a refusal falls through to
    // the loop below with no change in behaviour - and both are skipped when the source length
    // is unknown, since they need to know how much to move.
    let mut bytes = 0_u64;
    #[cfg(target_os = "linux")]
    let kernel_copied = {
        let mut copied = false;
        let remaining = if source_len == u64::MAX {
            0
        } else {
            source_len.saturating_sub(resume_offset)
        };
        if remaining > 0 {
            let mut moved_total = 0_u64;
            let mut report_chunk = |chunk_bytes: u64| -> bool {
                moved_total += chunk_bytes;
                progress(progress_event(
                    CopyProgressKind::BytesCopied,
                    action,
                    moved_total,
                    files_done,
                    files_total,
                    None,
                ));
                !control.is_cancelled()
            };
            if let Some(moved) = attempt_kernel_copy(
                &input,
                &output,
                resume_offset,
                remaining,
                resume,
                wanted as u64,
                &mut report_chunk,
            ) {
                bytes = moved;
                copied = true;
            }
        }
        copied
    };
    #[cfg(not(target_os = "linux"))]
    let kernel_copied = false;

    // A resumed file is missing its prefix, so hashing only the tail would describe a
    // different byte sequence than the source. Report no streamed hash and let
    // verification fall back to a full pair comparison. A kernel copy reads nothing, so there
    // is no streamed hash to collect either - it is computed in its own pass below.
    let mut hasher = (hash_source && !resuming && !kernel_copied).then(blake3::Hasher::new);

    while !kernel_copied {
        if control.is_cancelled() {
            return Err(cancelled_error(Some(destination.to_path_buf())));
        }
        let read = input.read(buffer).map_err(|err| {
            io_error(
                Some(source.to_path_buf()),
                format!("read failed: {err}"),
                true,
            )
        })?;
        if read == 0 {
            break;
        }
        if let Some(active_hasher) = hasher.as_mut() {
            active_hasher.update(&buffer[..read]);
        }
        output.write_all(&buffer[..read]).map_err(|err| {
            io_error(
                Some(destination.to_path_buf()),
                format!("write failed: {err}"),
                true,
            )
        })?;
        bytes += read as u64;
        progress(progress_event(
            CopyProgressKind::BytesCopied,
            action,
            bytes,
            files_done,
            files_total,
            None,
        ));
    }

    drop(output);
    drop(input);
    apply_metadata_policy(source, &write_path, metadata_policy)?;
    replace_file(&write_path, destination).map_err(|err| {
        io_error(
            Some(destination.to_path_buf()),
            format!("cannot publish destination: {err}"),
            true,
        )
    })?;
    if let Some(cleanup) = temp_cleanup.as_mut() {
        cleanup.disarm();
    }

    // A kernel copy never read the bytes, so the streamed hash the caller asked for has to be
    // computed in its own pass. That pass costs one read of the source; both fast paths are
    // still far ahead of read+write, and the alternative would be to report no hash and make
    // verification re-read both files.
    let kernel_hash = if kernel_copied && hash_source && !resuming {
        hash_file(source).ok()
    } else {
        None
    };

    Ok(CopyOutcome {
        bytes,
        source_hash: hasher
            .map(|active_hasher| active_hasher.finalize().to_hex().to_string())
            .or(kernel_hash),
    })
}

/// Verify a destination against the source hash produced while copying it.
/// Falls back to a full pair comparison when no streamed hash is available.
/// Recreate `source` (a symlink) at `destination` with the same link target string. This is
/// the `LinkPolicy::Preserve` payload: no file bytes are copied, so it reports 0 bytes.
fn copy_symlink_preserving(
    source: &Path,
    destination: &Path,
    control: &CopyControl,
) -> Result<u64, CopyError> {
    if control.is_cancelled() {
        return Err(cancelled_error(Some(destination.to_path_buf())));
    }

    let target = fs::read_link(source).map_err(|err| {
        io_error(
            Some(source.to_path_buf()),
            format!("cannot read symlink target: {err}"),
            false,
        )
    })?;

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            io_error(
                Some(parent.to_path_buf()),
                format!("cannot create parent directory: {err}"),
                true,
            )
        })?;
    }

    if let Ok(existing) = fs::symlink_metadata(destination) {
        if existing.is_dir() {
            return Err(io_error(
                Some(destination.to_path_buf()),
                "cannot replace an existing directory with a symlink",
                false,
            ));
        }
        fs::remove_file(destination).map_err(|err| {
            io_error(
                Some(destination.to_path_buf()),
                format!("cannot replace existing destination: {err}"),
                false,
            )
        })?;
    }

    create_symlink(&target, destination)?;
    Ok(0)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<(), CopyError> {
    std::os::unix::fs::symlink(target, link).map_err(|err| {
        io_error(
            Some(link.to_path_buf()),
            format!("cannot create symlink: {err}"),
            false,
        )
    })
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, link: &Path) -> Result<(), CopyError> {
    Err(unsupported(
        Some(link.to_path_buf()),
        "LinkPolicy::Preserve is not supported on this platform; select LinkPolicy::Follow or Skip",
    ))
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

/// Compare a source symlink with a destination symlink by link target string.
fn verify_symlink_pair(source: &Path, destination: &Path) -> Result<(), CopyError> {
    let source_target = fs::read_link(source).map_err(|err| {
        io_error(
            Some(source.to_path_buf()),
            format!("cannot read source symlink: {err}"),
            false,
        )
    })?;
    let destination_target = fs::read_link(destination).map_err(|err| {
        io_error(
            Some(destination.to_path_buf()),
            format!("cannot read destination symlink: {err}"),
            false,
        )
    })?;
    if source_target == destination_target {
        Ok(())
    } else {
        Err(verify_mismatch(
            destination,
            format!(
                "symlink target mismatch: expected {}, got {}",
                source_target.display(),
                destination_target.display()
            ),
        ))
    }
}

/// Sample window size for [`VerifyPolicy::SampledHash`]: head, middle and tail.
const VERIFY_SAMPLE_WINDOW_BYTES: u64 = 64 * 1024;

/// Number of sampled windows used by [`VerifyPolicy::SampledHash`].
const VERIFY_SAMPLE_WINDOW_COUNT: u64 = 3;

/// Applies a verification outcome to the report: a match counts as a verified file, and
/// anything else is an error. A mismatch never "un-copies" the file.
/// Check a finished transfer against the manifest it was told to verify against.
///
/// After the transfer rather than per file, because a manifest names everything that was sent -
/// including files the copy skipped as already in sync, which a per-file check never sees. `extra`
/// entries are reported by `verify_manifest` and are deliberately not errors: a manifest records what
/// was sent and is not an authority to delete anything.
fn apply_manifest_verification(plan: &CopyPlan, control: &CopyControl, report: &mut CopyReport) {
    if !matches!(plan.job.verify, VerifyPolicy::Manifest) {
        return;
    }
    if plan.job.dry_run {
        // A dry run writes nothing, so every manifest entry would be reported missing - a report about
        // the run that did not happen rather than about the data.
        return;
    }
    let Some(manifest) = plan.job.manifest.as_ref() else {
        // `plan` refuses this combination, so reaching here means a plan built by hand. Say so rather
        // than reporting a verification that never happened.
        report.errors.push(manifest_error(
            Some(plan.job.destination.clone()),
            "manifest verification was requested but no manifest was given; nothing was checked"
                .to_string(),
            CopyErrorCategory::InvalidInput,
        ));
        return;
    };
    if control.is_cancelled() {
        // A cancelled transfer has nothing worth checking, and calling every un-copied file a mismatch
        // would blame the data for stopping early.
        return;
    }
    match verify_manifest(&plan.job.destination, manifest) {
        Ok(check) => {
            report.verified_files += check.checked as u64;
            for path in check.missing.iter().chain(check.changed.iter()) {
                report.errors.push(CopyError {
                    category: CopyErrorCategory::VerifyMismatch,
                    path: Some(PathBuf::from(path)),
                    message: format!(
                        "manifest verification failed: {path} does not match the manifest"
                    ),
                    retryable: true,
                    recommended_action: "re-transfer the file".to_string(),
                });
            }
        }
        Err(error) => report.errors.push(error),
    }
}

fn apply_verify_result(result: Result<(), CopyError>, report: &mut CopyReport) {
    match result {
        Ok(()) => report.verified_files += 1,
        Err(err) => report.errors.push(err),
    }
}

/// Perform the verification a copy actually asked for. `FullHash`/`ReadAfterWrite` compare
/// whole-file hashes and reuse the hash produced while copying when one is available; `Size`
/// compares lengths; `SampledHash` compares hashes of head/middle/tail windows; `Manifest` is
/// not implemented and is refused, rather than silently behaving like a full hash.
///
/// Timed wrapper: records verification cost in `report.timings.verify_ms`, which is a subset of
/// `copy_ms` because files are verified as they finish rather than in a separate pass.
fn verify_after_copy(
    source: &Path,
    destination: &Path,
    policy: &VerifyPolicy,
    streamed_source_hash: Option<&str>,
    report: &mut CopyReport,
) {
    let started = std::time::Instant::now();
    verify_after_copy_inner(source, destination, policy, streamed_source_hash, report);
    report.timings.verify_ms = report
        .timings
        .verify_ms
        .saturating_add(started.elapsed().as_millis() as u64);
}

fn verify_after_copy_inner(
    source: &Path,
    destination: &Path,
    policy: &VerifyPolicy,
    streamed_source_hash: Option<&str>,
    report: &mut CopyReport,
) {
    // A preserved symlink is verified as a symlink: the artifact that was copied is the link
    // itself, so comparing what the link points at would verify the wrong thing.
    if is_symlink(destination) {
        apply_verify_result(verify_symlink_pair(source, destination), report);
        return;
    }

    match policy {
        VerifyPolicy::None => {}
        VerifyPolicy::FullHash | VerifyPolicy::ReadAfterWrite => {
            verify_from_source_hash(source, destination, streamed_source_hash, report);
        }
        VerifyPolicy::Size => apply_verify_result(verify_size(source, destination), report),
        VerifyPolicy::SampledHash => {
            apply_verify_result(verify_sampled_hash(source, destination), report);
        }
        // Nothing per file: a manifest names what was sent - including files this copy skipped as
        // already in sync, which a per-file check would never look at - so the check is one pass over
        // the result, in `apply_manifest_verification`. Doing nothing here is not a silent downgrade:
        // that pass always runs for this policy.
        VerifyPolicy::Manifest => {}
    }
}

fn verify_size(source: &Path, destination: &Path) -> Result<(), CopyError> {
    let source_len = file_len(source)?;
    let destination_len = file_len(destination)?;
    if source_len == destination_len {
        Ok(())
    } else {
        Err(verify_mismatch(
            destination,
            format!("size mismatch: expected {source_len} bytes, got {destination_len} bytes"),
        ))
    }
}

fn verify_sampled_hash(source: &Path, destination: &Path) -> Result<(), CopyError> {
    let source_hash = sampled_hash_file(source)?;
    let destination_hash = sampled_hash_file(destination)?;
    if source_hash == destination_hash {
        Ok(())
    } else {
        Err(verify_mismatch(
            destination,
            format!("sampled hash mismatch: expected {source_hash}, got {destination_hash}"),
        ))
    }
}

fn verify_mismatch(destination: &Path, message: String) -> CopyError {
    CopyError {
        category: CopyErrorCategory::VerifyMismatch,
        path: Some(destination.to_path_buf()),
        message,
        retryable: true,
        recommended_action: "retry_copy".to_string(),
    }
}

fn file_len(path: &Path) -> Result<u64, CopyError> {
    fs::metadata(path).map(|meta| meta.len()).map_err(|err| {
        io_error(
            Some(path.to_path_buf()),
            format!("cannot stat for size verification: {err}"),
            true,
        )
    })
}

/// BLAKE3 over the file length plus up to three fixed windows (head, middle, tail) of
/// [`VERIFY_SAMPLE_WINDOW_BYTES`]. Files that fit inside the sampled region are hashed
/// completely. This is deliberately weaker than [`hash_file`]: a difference outside the
/// sampled windows is not detected, which is the trade the caller asked for.
fn sampled_hash_file(path: &Path) -> Result<String, CopyError> {
    let mut file = fs::File::open(path).map_err(|err| {
        io_error(
            Some(path.to_path_buf()),
            format!("cannot open for sampled hashing: {err}"),
            true,
        )
    })?;
    let len = file
        .metadata()
        .map_err(|err| {
            io_error(
                Some(path.to_path_buf()),
                format!("cannot stat for sampled hashing: {err}"),
                true,
            )
        })?
        .len();

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tallow-copy-engine sampled-hash v1");
    hasher.update(&len.to_le_bytes());

    let window = VERIFY_SAMPLE_WINDOW_BYTES;
    if len <= window.saturating_mul(VERIFY_SAMPLE_WINDOW_COUNT) {
        hash_file_region(&mut file, &mut hasher, len, path)?;
    } else {
        for offset in [0_u64, (len - window) / 2, len - window] {
            hasher.update(&offset.to_le_bytes());
            file.seek(SeekFrom::Start(offset)).map_err(|err| {
                io_error(
                    Some(path.to_path_buf()),
                    format!("sampled hash seek failed: {err}"),
                    true,
                )
            })?;
            hash_file_region(&mut file, &mut hasher, window, path)?;
        }
    }

    Ok(hasher.finalize().to_hex().to_string())
}

/// Reads exactly `bytes` (or until EOF) from the current position into `hasher`.
fn hash_file_region(
    file: &mut fs::File,
    hasher: &mut blake3::Hasher,
    bytes: u64,
    path: &Path,
) -> Result<(), CopyError> {
    let mut remaining = bytes;
    let mut buffer = vec![0_u8; VERIFY_SAMPLE_WINDOW_BYTES as usize];
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        let read = file.read(&mut buffer[..want]).map_err(|err| {
            io_error(
                Some(path.to_path_buf()),
                format!("sampled hash read failed: {err}"),
                true,
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(())
}

fn verify_from_source_hash(
    source: &Path,
    destination: &Path,
    source_hash: Option<&str>,
    report: &mut CopyReport,
) {
    let Some(expected) = source_hash else {
        verify_pair(source, destination, report);
        return;
    };

    match hash_file(destination) {
        Ok(actual) if actual == expected => report.verified_files += 1,
        Ok(actual) => report.errors.push(CopyError {
            category: CopyErrorCategory::VerifyMismatch,
            path: Some(destination.to_path_buf()),
            message: format!("destination hash mismatch: expected {expected}, got {actual}"),
            retryable: true,
            recommended_action: "retry_copy".to_string(),
        }),
        Err(err) => report.errors.push(err),
    }
}

fn apply_metadata_policy(
    source: &Path,
    destination: &Path,
    policy: &MetadataPolicy,
) -> Result<(), CopyError> {
    if matches!(policy, MetadataPolicy::DataOnly) {
        return Ok(());
    }

    let metadata = fs::metadata(source).map_err(|err| {
        io_error(
            Some(source.to_path_buf()),
            format!("cannot stat source metadata: {err}"),
            true,
        )
    })?;

    if matches!(policy, MetadataPolicy::Timestamps | MetadataPolicy::All) {
        let modified = FileTime::from_last_modification_time(&metadata);
        filetime::set_file_mtime(destination, modified).map_err(|err| {
            io_error(
                Some(destination.to_path_buf()),
                format!("cannot set destination modified time: {err}"),
                true,
            )
        })?;
        if let Ok(accessed) = metadata.accessed() {
            let accessed = FileTime::from_system_time(accessed);
            filetime::set_file_atime(destination, accessed).map_err(|err| {
                io_error(
                    Some(destination.to_path_buf()),
                    format!("cannot set destination access time: {err}"),
                    true,
                )
            })?;
        }
    }

    if matches!(policy, MetadataPolicy::Attributes | MetadataPolicy::All) {
        fs::set_permissions(destination, metadata.permissions()).map_err(|err| {
            io_error(
                Some(destination.to_path_buf()),
                format!("cannot set destination permissions: {err}"),
                true,
            )
        })?;
    }

    Ok(())
}

fn verify_pair(source: &Path, destination: &Path, report: &mut CopyReport) {
    match (hash_file(source), hash_file(destination)) {
        (Ok(source_hash), Ok(destination_hash)) if source_hash == destination_hash => {
            report.verified_files += 1;
        }
        (Ok(source_hash), Ok(destination_hash)) => {
            report.errors.push(CopyError {
                category: CopyErrorCategory::VerifyMismatch,
                path: Some(destination.to_path_buf()),
                message: format!(
                    "destination hash mismatch: expected {source_hash}, got {destination_hash}"
                ),
                retryable: true,
                recommended_action: "retry_copy".to_string(),
            });
        }
        (Err(err), _) | (_, Err(err)) => report.errors.push(err),
    }
}

fn hash_file(path: &Path) -> Result<String, CopyError> {
    let mut file = fs::File::open(path).map_err(|err| {
        io_error(
            Some(path.to_path_buf()),
            format!("cannot open for hashing: {err}"),
            true,
        )
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 1024 * 1024];

    loop {
        let read = file.read(&mut buffer).map_err(|err| {
            io_error(
                Some(path.to_path_buf()),
                format!("hash read failed: {err}"),
                true,
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

fn invalid_input(path: Option<PathBuf>, message: impl Into<String>) -> CopyError {
    CopyError {
        category: CopyErrorCategory::InvalidInput,
        path,
        message: message.into(),
        retryable: false,
        recommended_action: "fix_request".to_string(),
    }
}

fn unsupported(path: Option<PathBuf>, message: impl Into<String>) -> CopyError {
    CopyError {
        category: CopyErrorCategory::Unsupported,
        path,
        message: message.into(),
        retryable: false,
        recommended_action: "change_policy".to_string(),
    }
}

fn io_error(path: Option<PathBuf>, message: impl Into<String>, retryable: bool) -> CopyError {
    CopyError {
        category: CopyErrorCategory::Io,
        path,
        message: message.into(),
        retryable,
        recommended_action: if retryable { "retry" } else { "inspect_path" }.to_string(),
    }
}

fn cancelled_error(path: Option<PathBuf>) -> CopyError {
    CopyError {
        category: CopyErrorCategory::Cancelled,
        path,
        message: "copy cancelled".to_string(),
        retryable: false,
        recommended_action: "cancelled".to_string(),
    }
}


// ── Transfer manifests ────────────────────────────────────────────────────────────────────────────
//
// `verify_transfer` answers "do these two trees agree?", which needs both trees present. The question
// that outlives a transfer is "is this copy still what was sent?", asked later, possibly on a machine
// that never had the originals. A manifest is what makes that answerable.
//
// Format - one line per file, behind a version header:
//
//     # nixe manifest v1
//     relative/path<TAB>bytes<TAB>blake3-hex
//
// TSV rather than JSON because a manifest is read by a human during an incident and by a script
// afterwards: JSON needs escaping rules to stay parseable, TSV needs none as long as paths carry no
// tabs or newlines - and a path that does is refused rather than written into an unreadable manifest.
//
// Paths are relative, slash-separated and validated on read: absolute paths and `..` components are
// rejected, so a manifest can never point a verification at something outside the tree it describes.
// Symlinked entries are skipped when writing (the same rule the mirror walk and the small-file probe
// already follow) because following a link would hash - and verify - content the tree does not own.

/// Names that belong to the transfer machinery rather than to the tree's content: the sync journal and
/// its write-ahead files, and the deterministic partial sibling a resume leaves behind (which is by
/// definition incomplete, so it must never be recorded as content). This mirrors `is_sync_bookkeeping`
/// in the standard library, duplicated deliberately: the engine is standalone and must not depend on the
/// crate that binds it.
pub fn is_manifest_bookkeeping(file_name: &str) -> bool {
    matches!(
        file_name,
        ".tallow-sync.db" | ".tallow-sync.db-shm" | ".tallow-sync.db-wal"
    ) || file_name.ends_with(".tallow-partial")
}

/// Version header a manifest begins with.
pub const MANIFEST_HEADER: &str = "# nixe manifest v1";

/// One recorded file: its path relative to the root, its size, and its content hash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestEntry {
    pub path: String,
    pub size: u64,
    pub hash: String,
}

/// What a verification found. `missing` and `changed` decide `is_clean`; `extra` is reported and never
/// acted on, because a manifest records what was sent and is not an authority to delete anything.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ManifestReport {
    pub checked: usize,
    pub missing: Vec<String>,
    pub changed: Vec<String>,
    pub extra: Vec<String>,
    /// Entries skipped because the manifest could not be trusted to name a path inside the root.
    pub skipped_unsafe: Vec<String>,
}

impl ManifestReport {
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.changed.is_empty()
    }
}

fn manifest_error(path: Option<PathBuf>, message: String, category: CopyErrorCategory) -> CopyError {
    CopyError {
        category,
        path,
        message,
        retryable: false,
        recommended_action: "re-run the verification; if it persists, inspect the manifest and tree"
            .to_string(),
    }
}

/// A path is only usable in a manifest if it is relative, has no parent-directory hops, and can survive
/// the line format. Returns the normalized slash-separated form.
fn manifest_path(text: &str) -> Option<String> {
    if text.is_empty() || text.contains('\t') || text.contains('\n') {
        return None;
    }
    let path = Path::new(text);
    if path.is_absolute() {
        return None;
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn relative_manifest_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    manifest_path(&relative.to_string_lossy())
}

/// Walk the tree, recording every regular file. Symlinks and sync bookkeeping are skipped (the journal
/// changes on every run, so recording it would make every verification report drift), as is the manifest
/// file itself when it is written inside the tree it describes.
fn manifest_entries(root: &Path, skip_path: Option<&Path>) -> Result<Vec<ManifestEntry>, CopyError> {
    let mut entries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let listing = fs::read_dir(&directory).map_err(|err| {
            manifest_error(
                Some(directory.clone()),
                format!("cannot read directory: {err}"),
                CopyErrorCategory::Io,
            )
        })?;
        for entry in listing {
            let entry = entry.map_err(|err| {
                manifest_error(
                    Some(directory.clone()),
                    format!("cannot read directory entry: {err}"),
                    CopyErrorCategory::Io,
                )
            })?;
            let path = entry.path();
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if let Some(skip) = skip_path {
                if path == skip {
                    continue;
                }
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if is_manifest_bookkeeping(name) {
                continue;
            }
            let Some(relative) = relative_manifest_path(root, &path) else {
                continue;
            };
            let hash = hash_file_hex(&path)?;
            entries.push(ManifestEntry {
                path: relative,
                size: metadata.len(),
                hash,
            });
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

/// Write a manifest for `root` to `out`. Returns how many files were recorded.
pub fn create_manifest(root: &Path, out: &Path) -> Result<usize, CopyError> {
    if !root.is_dir() {
        return Err(manifest_error(
            Some(root.to_path_buf()),
            "manifest root is not a directory".to_string(),
            CopyErrorCategory::InvalidInput,
        ));
    }
    let entries = manifest_entries(root, Some(out))?;
    let mut text = String::from(MANIFEST_HEADER);
    text.push('\n');
    for entry in &entries {
        text.push_str(&entry.path);
        text.push('\t');
        text.push_str(&entry.size.to_string());
        text.push('\t');
        text.push_str(&entry.hash);
        text.push('\n');
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| {
                manifest_error(
                    Some(parent.to_path_buf()),
                    format!("cannot create manifest directory: {err}"),
                    CopyErrorCategory::Io,
                )
            })?;
        }
    }
    fs::write(out, text).map_err(|err| {
        manifest_error(
            Some(out.to_path_buf()),
            format!("cannot write manifest: {err}"),
            CopyErrorCategory::Io,
        )
    })?;
    Ok(entries.len())
}

/// Parse a manifest. Comments and blank lines are ignored; a malformed line is an error rather than a
/// silently skipped entry, because a manifest that reads incorrectly is worse than one that fails.
pub fn parse_manifest(text: &str) -> Result<Vec<ManifestEntry>, CopyError> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        // A whitespace-only line is blank. The line is still split untrimmed below, so trailing spaces
        // in a path stay significant - only lines with no content at all are skipped.
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(path), Some(size), Some(hash)) = (fields.next(), fields.next(), fields.next()) else {
            return Err(manifest_error(
                None,
                format!("manifest line {} is not path<TAB>size<TAB>hash", index + 1),
                CopyErrorCategory::InvalidInput,
            ));
        };
        let Some(path) = manifest_path(path) else {
            return Err(manifest_error(
                None,
                format!(
                    "manifest line {} names a path outside the tree or unreadable in this format: {path}",
                    index + 1
                ),
                CopyErrorCategory::InvalidInput,
            ));
        };
        let size = size.parse::<u64>().map_err(|_| {
            manifest_error(
                None,
                format!("manifest line {} has a non-numeric size", index + 1),
                CopyErrorCategory::InvalidInput,
            )
        })?;
        if hash.is_empty() {
            return Err(manifest_error(
                None,
                format!("manifest line {} has no hash", index + 1),
                CopyErrorCategory::InvalidInput,
            ));
        }
        entries.push(ManifestEntry {
            path,
            size,
            hash: hash.to_string(),
        });
    }
    Ok(entries)
}

/// Verify `root` against a manifest. Reads only, and reports what it finds.
pub fn verify_manifest(root: &Path, manifest: &Path) -> Result<ManifestReport, CopyError> {
    if !root.is_dir() {
        return Err(manifest_error(
            Some(root.to_path_buf()),
            "manifest root is not a directory".to_string(),
            CopyErrorCategory::InvalidInput,
        ));
    }
    let text = fs::read_to_string(manifest).map_err(|err| {
        manifest_error(
            Some(manifest.to_path_buf()),
            format!("cannot read manifest: {err}"),
            CopyErrorCategory::Io,
        )
    })?;
    let entries = parse_manifest(&text)?;

    let mut report = ManifestReport::default();
    let mut recorded: Vec<&str> = Vec::with_capacity(entries.len());
    for entry in &entries {
        report.checked += 1;
        recorded.push(entry.path.as_str());
        let path = root.join(&entry.path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                report.missing.push(entry.path.clone());
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            report.changed.push(entry.path.clone());
            continue;
        }
        if metadata.len() != entry.size {
            report.changed.push(entry.path.clone());
            continue;
        }
        if hash_file_hex(&path)? != entry.hash {
            report.changed.push(entry.path.clone());
        }
    }

    // Anything present but unrecorded is reported, never removed.
    for entry in manifest_entries(root, Some(manifest))? {
        if !recorded.contains(&entry.path.as_str()) {
            report.extra.push(entry.path);
        }
    }
    Ok(report)
}

/// What auditing a target against a source found, per relative path. This is the one implementation:
/// the standard library's `TransferAudit` is mapped from it and the app reads it, so a report from any
/// of the three surfaces describes the same walk.
#[derive(Debug, Default, Clone)]
pub struct TreeAudit {
    pub source: String,
    pub target: String,
    pub verify: String,
    pub matching_files: u64,
    pub matching_bytes: u64,
    pub differing_files: u64,
    pub differing_bytes: u64,
    pub extra_files: u64,
    pub extra_bytes: u64,
    pub error_files: u64,
    /// Bounded by the caller's limit, so a 100k-file mismatch cannot flood a console.
    pub differing: Vec<String>,
    pub extra: Vec<String>,
    pub problems: Vec<String>,
}

impl TreeAudit {
    /// Clean means nothing to copy and nothing to delete - NOT "the trees are identical", which the
    /// cheap policies cannot establish.
    pub fn is_clean(&self) -> bool {
        self.differing_files == 0 && self.extra_files == 0 && self.error_files == 0
    }

    pub fn differences(&self) -> u64 {
        self.differing_files + self.extra_files + self.error_files
    }
}

/// Compare a target against a source and report what does not agree, writing nothing anywhere.
///
/// Read-only by construction, not by promise: it plans a mirror job with `dry_run` set and never calls
/// `execute`, so the copy, the delete and the sync journal all belong to a code path this cannot reach.
/// The mirror mode is what makes the target's extras visible - a copy plan never looks at them - and
/// `skip` decides how hard the comparison looks (two stats, or reading both files).
pub fn audit_trees(
    source: &Path,
    target: &Path,
    skip: SkipPolicy,
    verify_label: &str,
    limit: usize,
) -> Result<TreeAudit, CopyError> {
    let mut job = CopyJob::copy(source, target);
    job.mode = CopyMode::Mirror;
    job.skip = skip;
    job.dry_run = true;
    job.threads = 1;

    let planned = plan(job)?;
    let mut audit = TreeAudit {
        source: planned.job.source.to_string_lossy().to_string(),
        target: planned.job.destination.to_string_lossy().to_string(),
        verify: verify_label.to_string(),
        ..TreeAudit::default()
    };

    for action in &planned.actions {
        let relative = action.relative_path.to_string_lossy().to_string();
        let name = Path::new(&relative)
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        // A stray sync database or partial is ours, not the user's data: reporting it as a difference
        // would be noise, and delta-sync refuses to copy it for the same reason.
        if relative.is_empty() || is_manifest_bookkeeping(&name) {
            continue;
        }
        match action.kind {
            CopyActionKind::Skip => {
                audit.matching_files += 1;
                audit.matching_bytes += action.bytes;
            }
            CopyActionKind::Copy => {
                audit.differing_files += 1;
                audit.differing_bytes += action.bytes;
                if audit.differing.len() < limit {
                    audit.differing.push(relative);
                }
            }
            CopyActionKind::Delete => {
                audit.extra_files += 1;
                audit.extra_bytes += action.bytes;
                if audit.extra.len() < limit {
                    audit.extra.push(relative);
                }
            }
            CopyActionKind::Error => {
                audit.error_files += 1;
                if audit.problems.len() < limit {
                    audit.problems.push(format!("{relative}: {}", action.reason));
                }
            }
            _ => {}
        }
    }
    Ok(audit)
}

#[cfg(test)]
mod manifest_policy_tests {
    use super::*;
    use tempfile::tempdir;
    use std::fs;

    #[test]
    fn manifest_verification_checks_a_finished_transfer_against_the_manifest() {
        let source = tempdir().expect("source");
        let target = tempdir().expect("target");
        fs::write(source.path().join("a.txt"), "alpha").unwrap();
        fs::write(source.path().join("b.txt"), "beta").unwrap();
        let manifest = source.path().join("manifest.tsv");
        let entries = create_manifest(source.path(), &manifest).expect("create manifest");
        assert_eq!(entries, 2);

        let mut job = CopyJob::copy(source.path(), target.path());
        job.verify = VerifyPolicy::Manifest;
        job.manifest = Some(manifest.clone());
        let planned = plan(job).expect("a manifest job plans");
        // Pin the two preconditions, so a failure below is about the check and not about the job.
        assert_eq!(planned.job.verify, VerifyPolicy::Manifest);
        assert!(planned.job.manifest.is_some(), "the manifest must survive planning");
        // Isolate the wiring from the verification: does the check itself find the two files?
        let direct = verify_manifest(target.path(), planned.job.manifest.as_ref().unwrap())
            .expect("direct verify");
        assert_eq!(direct.checked, 2, "the manifest names both copied files");
        let report = execute(&planned);
        assert!(
            report.errors.is_empty(),
            "a faithful copy must verify: {:?}",
            report.errors
        );
        assert_eq!(report.verified_files, 2, "the manifest pass checked both files");
    }

    #[test]
    fn manifest_verification_names_a_file_that_does_not_match() {
        let source = tempdir().expect("source");
        let target = tempdir().expect("target");
        fs::write(source.path().join("a.txt"), "alpha").unwrap();
        // A manifest that disagrees with the file about to be sent: the check must catch it rather than
        // trusting the copy to be honest.
        let manifest = source.path().join("manifest.tsv");
        fs::write(
            &manifest,
            "# nixe manifest v1\na.txt\t5\taaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        )
        .unwrap();

        let mut job = CopyJob::copy(source.path(), target.path());
        job.verify = VerifyPolicy::Manifest;
        job.manifest = Some(manifest);
        let report = execute(&plan(job).expect("plan"));
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert_eq!(report.errors[0].category, CopyErrorCategory::VerifyMismatch);
    }

    #[test]
    fn a_manifest_job_without_a_manifest_is_refused_before_it_starts() {
        let source = tempdir().expect("source");
        let target = tempdir().expect("target");
        fs::write(source.path().join("a.txt"), "alpha").unwrap();
        let mut job = CopyJob::copy(source.path(), target.path());
        job.verify = VerifyPolicy::Manifest;
        let error = plan(job).expect_err("a manifest policy without a manifest verifies nothing");
        assert_eq!(error.category, CopyErrorCategory::Unsupported);
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, relative: &str, body: &str) {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&path, body).expect("write");
    }

    #[test]
    fn two_matching_trees_audit_clean() {
        let source = TempDir::new().expect("source");
        let target = TempDir::new().expect("target");
        write(source.path(), "a.txt", "same");
        write(source.path(), "nested/b.bin", "same too");
        write(target.path(), "a.txt", "same");
        write(target.path(), "nested/b.bin", "same too");

        let audit = audit_trees(source.path(), target.path(), SkipPolicy::SizeMtime, "size", 10)
            .expect("audit");
        assert!(audit.is_clean(), "unexpected differences: {:?}", audit.differing);
        assert_eq!(audit.matching_files, 2);
        assert_eq!(audit.differences(), 0);
    }

    #[test]
    fn an_audit_reports_both_kinds_of_difference_and_touches_nothing() {
        let source = TempDir::new().expect("source");
        let target = TempDir::new().expect("target");
        write(source.path(), "changed.txt", "source version");
        write(source.path(), "only-in-source.txt", "fresh");
        write(target.path(), "changed.txt", "target version, longer");
        write(target.path(), "only-in-target.txt", "stray");
        let before = fs::read(target.path().join("changed.txt")).expect("read");

        let audit = audit_trees(source.path(), target.path(), SkipPolicy::SizeMtime, "size", 10)
            .expect("audit");
        assert!(!audit.is_clean());
        assert_eq!(audit.differing_files, 2, "changed + only-in-source: {:?}", audit.differing);
        assert_eq!(audit.extra_files, 1);
        assert!(audit.extra.contains(&"only-in-target.txt".to_string()));

        // The point of a read-only audit: the target is exactly as it was.
        assert_eq!(fs::read(target.path().join("changed.txt")).expect("read"), before);
        assert!(target.path().join("only-in-target.txt").exists());
        assert_eq!(audit.differences(), 3);
    }

    #[test]
    fn our_own_bookkeeping_files_are_not_differences() {
        let source = TempDir::new().expect("source");
        let target = TempDir::new().expect("target");
        write(source.path(), "data.txt", "same");
        write(target.path(), "data.txt", "same");
        // Ours, written by delta-sync; a user should never see it reported as their data.
        write(target.path(), ".tallow-sync.db", "journal");
        write(target.path(), "half.tallow-partial", "partial");

        let audit = audit_trees(source.path(), target.path(), SkipPolicy::SizeMtime, "size", 10)
            .expect("audit");
        assert!(audit.is_clean(), "bookkeeping must not count: {:?}", audit.extra);
        assert_eq!(audit.extra_files, 0);
    }

    #[test]
    fn the_limit_bounds_what_is_listed_while_the_counts_stay_complete() {
        let source = TempDir::new().expect("source");
        let target = TempDir::new().expect("target");
        for index in 0..5 {
            write(source.path(), &format!("f{index}.txt"), "body");
        }
        let audit = audit_trees(source.path(), target.path(), SkipPolicy::SizeMtime, "size", 2)
            .expect("audit");
        assert_eq!(audit.differing_files, 5, "the count is not capped");
        assert_eq!(audit.differing.len(), 2, "only the listing is capped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_execute_does_not_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.txt");
        let destination = tmp.path().join("dst.txt");
        fs::write(&source, b"hello").unwrap();

        let mut job = CopyJob::copy(&source, &destination);
        job.dry_run = true;
        let plan = plan(job).unwrap();
        let report = execute(&plan);

        assert_eq!(report.copied_files, 0);
        assert!(!destination.exists());
    }

    #[test]
    fn copy_job_defaults_to_one_mib_buffer() {
        let job = CopyJob::copy("source", "destination");

        assert_eq!(job.buffer_size_bytes, DEFAULT_BUFFER_SIZE_BYTES);
    }

    #[test]
    fn cancelled_control_stops_before_copying() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.txt");
        let destination = tmp.path().join("dst.txt");
        fs::write(&source, b"hello").unwrap();

        let plan = plan(CopyJob::copy(&source, &destination)).unwrap();
        let control = CopyControl::new();
        control.cancel();
        let report = execute_with_control(&plan, &control, |_| {});

        assert_eq!(report.copied_files, 0);
        assert!(matches!(
            report.errors.first().map(|error| &error.category),
            Some(CopyErrorCategory::Cancelled)
        ));
        assert!(!destination.exists());
    }

    #[test]
    fn requested_threads_schedule_multiple_copy_workers() {
        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("src");
        let destination_root = tmp.path().join("dst");
        fs::create_dir_all(&source_root).unwrap();
        for index in 0..4 {
            fs::write(
                source_root.join(format!("file-{index}.bin")),
                vec![index; 4096],
            )
            .unwrap();
        }

        let mut job = CopyJob::copy(&source_root, &destination_root);
        job.threads = 2;
        let plan = plan(job).unwrap();
        let report = execute(&plan);

        assert_eq!(report.copied_files, 4);
        assert_eq!(
            fs::read(destination_root.join("file-3.bin")).unwrap(),
            vec![3; 4096]
        );
        assert!(
            report.worker_threads_used >= 2,
            "expected multiple copy workers, got {}",
            report.worker_threads_used
        );
    }

    #[test]
    fn copy_buffer_is_capped_by_file_size() {
        // A 4 KiB file behind a 32 MiB job buffer must not allocate 32 MiB.
        assert_eq!(copy_buffer_size(32 * 1024 * 1024, 4096), 4096);
        assert_eq!(copy_buffer_size(32 * 1024 * 1024, 0), MIN_BUFFER_SIZE_BYTES);
        assert_eq!(copy_buffer_size(8, 1024), MIN_BUFFER_SIZE_BYTES);
        assert_eq!(copy_buffer_size(1024 * 1024, u64::MAX), 1024 * 1024);
    }

    #[test]
    fn verification_does_not_need_the_source_once_it_is_hashed() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        fs::write(&source, vec![7_u8; 8192]).unwrap();
        let expected = hash_file(&source).unwrap();

        let mut job = CopyJob::copy(&source, &destination);
        job.verify = VerifyPolicy::FullHash;
        let plan = plan(job).unwrap();
        let report = execute(&plan);
        assert_eq!(report.verified_files, 1, "expected the copy to be verified");

        // The hash produced during the copy is enough. If verification re-read the
        // source, this would fail: the source no longer exists.
        fs::remove_file(&source).unwrap();
        let mut report = CopyReport::default();
        verify_from_source_hash(&source, &destination, Some(&expected), &mut report);
        assert_eq!(report.verified_files, 1);
        assert!(report.errors.is_empty(), "got {:?}", report.errors);

        // A wrong hash must still be caught.
        let mut mismatch = CopyReport::default();
        verify_from_source_hash(&source, &destination, Some("00"), &mut mismatch);
        assert!(
            matches!(
                mismatch.errors.first().map(|error| &error.category),
                Some(CopyErrorCategory::VerifyMismatch)
            ),
            "expected a VerifyMismatch, got {:?}",
            mismatch.errors
        );
    }

    #[cfg(unix)]
    #[test]
    fn follow_symlinks_copies_what_the_link_points_at() {
        let tmp = tempfile::tempdir().unwrap();
        let source_dir = tmp.path().join("src");
        let destination_dir = tmp.path().join("dst");
        fs::create_dir_all(source_dir.join("sub")).unwrap();
        fs::write(source_dir.join("sub/real.bin"), b"real bytes").unwrap();
        std::os::unix::fs::symlink("sub/real.bin", source_dir.join("link.bin")).unwrap();

        let mut job = CopyJob::copy(&source_dir, &destination_dir);
        job.link_policy = LinkPolicy::Follow;
        let plan = plan(job).unwrap();
        let report = execute(&plan);

        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        let followed = destination_dir.join("link.bin");
        let meta = fs::symlink_metadata(&followed).unwrap();
        assert!(
            meta.file_type().is_file(),
            "Follow must copy the target's content, not recreate the link"
        );
        assert_eq!(fs::read(&followed).unwrap(), b"real bytes");
        assert_eq!(
            fs::read(destination_dir.join("sub/real.bin")).unwrap(),
            b"real bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_and_counted_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let source_dir = tmp.path().join("src");
        let destination_dir = tmp.path().join("dst");
        fs::create_dir_all(source_dir.join("sub")).unwrap();
        fs::write(source_dir.join("sub/real.bin"), b"real bytes").unwrap();
        std::os::unix::fs::symlink("sub/real.bin", source_dir.join("link.bin")).unwrap();

        let plan = plan(CopyJob::copy(&source_dir, &destination_dir)).unwrap();
        let report = execute(&plan);

        assert!(!fs::symlink_metadata(destination_dir.join("link.bin")).is_ok());
        assert_eq!(plan.skipped_files, 1, "the link must be an explicit Skip");
        assert!(
            plan.actions.iter().any(|action| {
                matches!(action.kind, CopyActionKind::Skip) && action.reason.contains("symlink")
            }),
            "a skipped symlink must say why: {:?}",
            plan.actions
                .iter()
                .map(|action| (&action.kind, &action.reason))
                .collect::<Vec<_>>()
        );
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    }

    #[cfg(unix)]
    #[test]
    fn preserve_recreates_the_symlink_without_copying_target_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let source_dir = tmp.path().join("src");
        let destination_dir = tmp.path().join("dst");
        fs::create_dir_all(source_dir.join("sub")).unwrap();
        fs::write(source_dir.join("sub/real.bin"), b"real bytes").unwrap();
        std::os::unix::fs::symlink("sub/real.bin", source_dir.join("link.bin")).unwrap();

        let mut job = CopyJob::copy(&source_dir, &destination_dir);
        job.link_policy = LinkPolicy::Preserve;
        let plan = plan(job).unwrap();
        let report = execute(&plan);

        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        let copied = destination_dir.join("link.bin");
        let meta = fs::symlink_metadata(&copied).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "expected a symlink, got {meta:?}"
        );
        assert_eq!(fs::read_link(&copied).unwrap(), Path::new("sub/real.bin"));
        // Only the real file's bytes moved: the link's own entry contributed nothing.
        assert_eq!(report.bytes_copied, b"real bytes".len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_reported_rather_than_followed_forever() {
        let tmp = tempfile::tempdir().unwrap();
        let source_dir = tmp.path().join("src");
        fs::create_dir_all(source_dir.join("sub")).unwrap();
        // sub/up -> .. resolves back to source_dir, so following it would recurse for ever.
        std::os::unix::fs::symlink("..", source_dir.join("sub/up")).unwrap();

        let mut job = CopyJob::copy(&source_dir, tmp.path().join("dst"));
        job.link_policy = LinkPolicy::Follow;
        let error = plan(job).expect_err("a loop must be reported, not followed forever");
        assert!(error.message.contains("loop"), "{}", error.message);
    }

    #[test]
    fn size_verification_accepts_equal_lengths_and_rejects_a_truncated_file() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        fs::write(&source, b"0123456789").unwrap();
        fs::write(&destination, b"0123456789").unwrap();
        assert!(verify_size(&source, &destination).is_ok());

        // Same prefix, one byte short: exactly what a size check exists to catch.
        fs::write(&destination, b"012345678").unwrap();
        let err = verify_size(&source, &destination).unwrap_err();
        assert!(matches!(err.category, CopyErrorCategory::VerifyMismatch));
        assert!(err.message.contains("size mismatch"), "{}", err.message);
    }

    #[test]
    fn sampled_hash_detects_same_size_corruption_in_a_sampled_window() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        // Longer than the sampled windows, so only head/middle/tail are compared.
        let mut content: Vec<u8> = (0..1_048_576u32).map(|i| (i % 251) as u8).collect();
        fs::write(&source, &content).unwrap();
        fs::write(&destination, &content).unwrap();
        assert!(verify_sampled_hash(&source, &destination).is_ok());

        // Flip one byte inside the middle window: same length, must still be caught.
        let middle = content.len() / 2;
        content[middle] ^= 0xff;
        fs::write(&destination, &content).unwrap();
        let err = verify_sampled_hash(&source, &destination).unwrap_err();
        assert!(matches!(err.category, CopyErrorCategory::VerifyMismatch));
        assert!(
            err.message.contains("sampled hash mismatch"),
            "{}",
            err.message
        );
    }

    #[test]
    fn verify_mode_without_a_policy_still_verifies_by_full_hash() {
        // CopyMode::Verify asks for verification by itself, so the default VerifyPolicy::None
        // must not mean "verify nothing". Before effective_verify_policy this combination
        // planned a Verify action and performed none - a plan advertising skipped work.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        fs::write(&source, b"identical").unwrap();
        fs::write(&destination, b"identical").unwrap();
        let mtime = fs::metadata(&source).unwrap().modified().unwrap();
        fs::File::options()
            .write(true)
            .open(&destination)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        let mut job = CopyJob::copy(&source, &destination);
        job.mode = CopyMode::Verify;
        assert!(
            matches!(job.verify, VerifyPolicy::None),
            "the default policy is the point of this test"
        );
        let matching_plan = plan(job).unwrap();
        assert!(
            matching_plan
                .actions
                .iter()
                .any(|action| matches!(action.kind, CopyActionKind::Verify)),
            "Verify mode must plan a Verify action: {:?}",
            matching_plan
                .actions
                .iter()
                .map(|a| &a.kind)
                .collect::<Vec<_>>()
        );
        let report = execute(&matching_plan);
        assert_eq!(
            report.verified_files, 1,
            "Verify mode with no explicit policy must still verify"
        );
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);

        // And it is a real full-hash check: same size, same mtime, different bytes must fail.
        let mut job = CopyJob::copy(&source, &destination);
        job.mode = CopyMode::Verify;
        let impostor_plan = plan(job).unwrap();
        fs::write(&destination, b"differnt!").unwrap(); // same length as "identical"
        fs::File::options()
            .write(true)
            .open(&destination)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let report = execute(&impostor_plan);
        assert_eq!(
            report.verified_files, 0,
            "a same-size impostor must not verify"
        );
        assert!(
            report
                .errors
                .iter()
                .any(|error| matches!(error.category, CopyErrorCategory::VerifyMismatch)),
            "expected a VerifyMismatch, got {:?}",
            report.errors
        );
    }

    #[test]
    fn sizemtimehash_is_the_variant_that_does_not_trust_mtime() {
        // The pending semantics decision, recorded as a test: SizeMtimeHash keeps hashing both
        // files on the matching path. It has to - if it trusted size+mtime alone it would be
        // exactly SizeMtime and the variant would have no reason to exist. The cost is real
        // and documented: it reads both files in full on files that are already in sync.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        fs::write(&source, b"AAAA").unwrap();
        fs::write(&destination, b"BBBB").unwrap(); // same length, different bytes
        let mtime = fs::metadata(&source).unwrap().modified().unwrap();
        fs::File::options()
            .write(true)
            .open(&destination)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        // Same size, same mtime, different content: SizeMtime trusts the metadata and skips.
        let mut trusting = CopyJob::copy(&source, &destination);
        trusting.skip = SkipPolicy::SizeMtime;
        let trusting_plan = plan(trusting).unwrap();
        assert!(
            trusting_plan
                .actions
                .iter()
                .all(|action| matches!(action.kind, CopyActionKind::Skip)),
            "SizeMtime is the cheap variant: {:?}",
            trusting_plan
                .actions
                .iter()
                .map(|action| (&action.kind, &action.reason))
                .collect::<Vec<_>>()
        );

        // SizeMtimeHash reads the bytes and catches the impostor.
        let mut thorough = CopyJob::copy(&source, &destination);
        thorough.skip = SkipPolicy::SizeMtimeHash;
        let thorough_plan = plan(thorough).unwrap();
        assert!(
            thorough_plan
                .actions
                .iter()
                .any(|action| matches!(action.kind, CopyActionKind::Copy)),
            "SizeMtimeHash must catch a same-size, same-mtime impostor: {:?}",
            thorough_plan
                .actions
                .iter()
                .map(|action| (&action.kind, &action.reason))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_unchanged_destination_is_verified_not_skipped_when_a_policy_asks_for_it() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        fs::write(&source, b"identical").unwrap();
        fs::write(&destination, b"identical").unwrap();

        // Size is the desktop app's default verify mode: an unchanged destination must be
        // verified, not skipped, or the app's default promise is quietly empty.
        for policy in [VerifyPolicy::Size, VerifyPolicy::SampledHash] {
            let mut job = CopyJob::copy(&source, &destination);
            job.verify = policy.clone();
            let plan = plan(job).unwrap();
            assert!(
                plan.actions
                    .iter()
                    .any(|action| matches!(action.kind, CopyActionKind::Verify)),
                "policy {policy:?} must plan a Verify action, got {:?}",
                plan.actions.iter().map(|a| &a.kind).collect::<Vec<_>>()
            );
            let report = execute(&plan);
            assert_eq!(
                report.verified_files, 1,
                "policy {policy:?} must verify the file"
            );
            assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        }

        // With no policy and Copy mode, the unchanged file is still just a Skip.
        let plan = plan(CopyJob::copy(&source, &destination)).unwrap();
        assert!(
            plan.actions
                .iter()
                .all(|action| !matches!(action.kind, CopyActionKind::Verify)),
            "no policy asked for verification: {:?}",
            plan.actions.iter().map(|a| &a.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_copy_asking_for_size_or_sampled_verification_really_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        for (index, policy) in [VerifyPolicy::Size, VerifyPolicy::SampledHash]
            .into_iter()
            .enumerate()
        {
            // A fresh tree per policy: reusing one destination would let the skip policy
            // legitimately skip the second copy and prove nothing.
            let root = tmp.path().join(format!("run-{index}"));
            fs::create_dir_all(&root).unwrap();
            let source = root.join("src.bin");
            let destination = root.join("dst.bin");
            fs::write(&source, vec![3_u8; 4096]).unwrap();

            let mut job = CopyJob::copy(&source, &destination);
            job.verify = policy.clone();
            let plan = plan(job).unwrap();
            let report = execute(&plan);

            assert_eq!(report.copied_files, 1, "errors: {:?}", report.errors);
            assert_eq!(
                report.verified_files, 1,
                "policy {policy:?} is offered by the app and must actually verify something"
            );
            assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        }
    }

    #[test]
    fn manifest_verification_is_refused_before_anything_is_copied() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        fs::write(&source, b"content").unwrap();

        let mut job = CopyJob::copy(&source, &destination);
        job.verify = VerifyPolicy::Manifest;

        let error = plan(job).expect_err("Manifest must be refused, not downgraded");
        assert!(matches!(error.category, CopyErrorCategory::Unsupported));
        assert!(error.message.contains("Manifest"), "{}", error.message);
        assert!(!destination.exists(), "a refused plan must copy nothing");
    }

    #[test]
    fn resume_continues_from_a_partial_instead_of_restarting() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        let content: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        fs::write(&source, &content).unwrap();

        // A previous run staged the first 3000 bytes and was interrupted.
        let partial = tmp.path().join(".dst.bin.tallow-partial");
        fs::write(&partial, &content[..3000]).unwrap();

        let mut job = CopyJob::copy(&source, &destination);
        job.mode = CopyMode::Resume;
        let plan = plan(job).unwrap();
        let report = execute(&plan);

        assert_eq!(report.copied_files, 1, "errors: {:?}", report.errors);
        assert_eq!(
            fs::read(&destination).unwrap(),
            content,
            "resumed file must be byte-exact"
        );
        assert_eq!(
            report.bytes_copied,
            content.len() as u64 - 3000,
            "only the missing tail should have been transferred"
        );
        assert!(!partial.exists(), "the partial is consumed by the rename");
    }

    #[test]
    fn an_interrupted_resume_keeps_a_partial_that_the_next_run_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        let content = vec![7_u8; 4 * 1024 * 1024];
        fs::write(&source, &content).unwrap();

        // Cancel after the first chunk, i.e. mid-file, like a crash or a ^C would.
        let mut job = CopyJob::copy(&source, &destination);
        job.mode = CopyMode::Resume;
        job.buffer_size_bytes = 64 * 1024;
        let interrupted_plan = plan(job).unwrap();
        let control = CopyControl::new();
        let written = std::cell::Cell::new(0_u64);
        let report = execute_with_control(&interrupted_plan, &control, |event| {
            if matches!(event.kind, CopyProgressKind::BytesCopied) {
                written.set(written.get() + event.bytes_done);
                if written.get() >= 64 * 1024 {
                    control.cancel();
                }
            }
        });
        assert_eq!(report.copied_files, 0, "the first run must be interrupted");
        assert!(
            !destination.exists(),
            "an interrupted run leaves no destination"
        );

        let partial = tmp.path().join(".dst.bin.tallow-partial");
        let staged = fs::metadata(&partial).map(|meta| meta.len()).unwrap_or(0);
        assert!(
            staged > 0 && staged < content.len() as u64,
            "a resumable partial must survive the interruption, got {staged} bytes"
        );

        // The next run picks it up and finishes the file.
        let mut job = CopyJob::copy(&source, &destination);
        job.mode = CopyMode::Resume;
        let resume_plan = plan(job).unwrap();
        let report = execute(&resume_plan);

        assert_eq!(report.copied_files, 1, "errors: {:?}", report.errors);
        assert_eq!(
            fs::read(&destination).unwrap(),
            content,
            "resumed content must match"
        );
        assert_eq!(
            report.bytes_copied,
            content.len() as u64 - staged,
            "the second run should transfer only what was missing"
        );
    }

    #[test]
    fn a_stale_partial_is_ignored_when_the_destination_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        let content = vec![9_u8; 8192];
        fs::write(&source, &content).unwrap();
        fs::write(&destination, b"old content").unwrap();
        let partial = tmp.path().join(".dst.bin.tallow-partial");
        fs::write(&partial, &content[..1000]).unwrap();

        let mut job = CopyJob::copy(&source, &destination);
        job.mode = CopyMode::Resume;
        let plan = plan(job).unwrap();
        let report = execute(&plan);

        assert_eq!(report.copied_files, 1, "errors: {:?}", report.errors);
        assert_eq!(fs::read(&destination).unwrap(), content);
        assert_eq!(
            report.bytes_copied,
            content.len() as u64,
            "a stale partial must not be spliced onto the destination"
        );
    }

    #[test]
    fn the_reflink_helper_answers_without_ever_failing_a_copy() {
        // On a filesystem that supports reflinks this returns true; on one that does not it
        // returns false. What it must never do is error, because a filesystem without reflink
        // is a fallback, not a copy failure.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        fs::write(&source, vec![3u8; 64 * 1024]).unwrap();
        let source_file = fs::File::open(&source).unwrap();
        let destination_file = fs::File::create(&destination).unwrap();

        let cloned = try_reflink(&source_file, &destination_file).expect("must not error");
        drop(destination_file);
        if cloned {
            assert_eq!(fs::read(&destination).unwrap(), fs::read(&source).unwrap());
        }
        println!("reflink supported on this filesystem: {cloned}");
    }

    #[test]
    fn the_range_copy_helper_moves_exactly_the_bytes_it_claims() {
        // Covers both the whole-file case and the resume case (a nonzero offset), and the
        // partial-request case: whatever it reports as moved must be on disk verbatim.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        let content: Vec<u8> = (0..=(256 * 1024)).map(|n| (n % 251) as u8).collect();
        fs::write(&source, &content).unwrap();
        let source_file = fs::File::open(&source).unwrap();

        // A tail copy: the shape a resume uses. The destination already holds the prefix, and
        // both offsets advance together - writing at an offset into an EMPTY file would punch a
        // hole and make the file longer than the range requested.
        let offset = 1024_u64;
        let len = content.len() as u64 - offset;
        fs::write(&destination, &content[..offset as usize]).unwrap();
        let destination_file = fs::OpenOptions::new().write(true).open(&destination).unwrap();
        let moved = try_copy_file_range(
            &source_file,
            &destination_file,
            offset,
            len,
            64 * 1024,
            |_chunk| true,
        )
        .expect("must not error");
        drop(destination_file);
        match moved {
            Some(moved) => {
                assert_eq!(moved, len, "must move the whole requested range");
                assert_eq!(
                    fs::read(&destination).unwrap(),
                    content,
                    "prefix plus transferred tail must be the whole file"
                );
            }
            None => println!("copy_file_range unsupported on this filesystem; generic path used"),
        }
    }

    /// Real A/B on this machine's filesystem, through the ENGINE's own copier rather than
    /// through `copy_file`: `copy_file`'s generic branch is `std::fs::copy`, which already does a
    /// kernel-side copy_file_range on Linux, so comparing those two measures kernel-vs-kernel and
    /// shows nothing. This compares what the planner/executor actually does with and without the
    /// fast paths: a user-space buffered loop versus the kernel.
    #[test]
    #[ignore = "writes 512 MiB twice; run with --ignored --nocapture"]
    fn bench_fast_paths_against_the_generic_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        let destination_root = tmp.path().join("destination");
        fs::create_dir_all(&source_root).unwrap();
        let source = source_root.join("bench.bin");
        let block = vec![0x5au8; 8 * 1024 * 1024];
        {
            let mut file = fs::File::create(&source).unwrap();
            for _ in 0..64 {
                std::io::Write::write_all(&mut file, &block).unwrap();
            }
        }
        let size = fs::metadata(&source).unwrap().len();

        let mut measure = |fast: bool| -> f64 {
            // SAFETY: single-threaded benchmark; the variable is set before the copy and the
            // process exits shortly after, so nothing else can observe a torn environment.
            unsafe {
                if fast {
                    std::env::remove_var("TALLOW_COPY_DISABLE_FAST_PATH");
                } else {
                    std::env::set_var("TALLOW_COPY_DISABLE_FAST_PATH", "1");
                }
            }
            let destination = destination_root.join(if fast { "fast.bin" } else { "generic.bin" });
            let mut job = CopyJob::copy(&source, &destination);
            job.threads = 1;
            job.verify = VerifyPolicy::None;
            let plan = plan(job).expect("plan");
            let started = std::time::Instant::now();
            let report = execute(&plan);
            let elapsed = started.elapsed().as_secs_f64();
            assert_eq!(report.copied_files, 1, "errors: {:?}", report.errors);
            assert_eq!(report.bytes_copied, size);
            assert_eq!(
                fs::metadata(&destination).unwrap().len(),
                size,
                "the destination must be the full file either way"
            );
            let _ = fs::remove_file(&destination);
            (size as f64 / (1024.0 * 1024.0)) / elapsed
        };

        let generic = measure(false);
        let fast = measure(true);
        unsafe { std::env::remove_var("TALLOW_COPY_DISABLE_FAST_PATH") };
        println!("512 MiB through the engine copier on {}", tmp.path().display());
        println!("  buffered loop (fast paths disabled) : {generic:.1} MB/s");
        println!("  kernel fast paths                   : {fast:.1} MB/s");
        println!("  ratio                               : {:.1}x", fast / generic);
    }

    #[test]
    fn the_recommended_thread_count_follows_the_measured_path_classes() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.bin"), b"x").unwrap();

        // Same directory, same device: one stream, because the kernel does the work and extra
        // workers only contend.
        assert_eq!(
            classify_paths(&source, &tmp.path().join("dst")),
            PathClass::SameVolume
        );
        assert_eq!(recommended_threads(&source, &tmp.path().join("dst")), 1);

        // A different filesystem entirely: two lanes.
        if Path::new("/dev/shm").is_dir() {
            let other = Path::new("/dev/shm/does-not-need-to-exist");
            let class = classify_paths(&source, other);
            if class == PathClass::Other {
                assert_eq!(recommended_threads(&source, other), 2);
            } else {
                println!("skipped the cross-filesystem case: classified as {class:?}");
            }
        }

        // A live SMB mount, if this machine has one: measured no scaling at all, so one stream.
        let nas = Path::new("/mnt/NAS/<account>");
        if nas.is_dir() {
            assert_eq!(
                classify_paths(&source, nas),
                PathClass::Smb,
                "a CIFS mount must be recognised by its filesystem magic"
            );
            assert_eq!(recommended_threads(&source, nas), 1);
            println!("classified the live CIFS mount as Smb -> 1 stream");
        } else {
            println!("no CIFS mount present; the Smb branch was not exercised here");
        }

        // A hint must never turn a copy into a failure, even for paths that do not exist.
        let missing = Path::new("/nonexistent-root/nope/deeper");
        let _ = recommended_threads(missing, missing);
    }

    #[test]
    fn paths_are_normalised_before_planning() {
        use std::path::Component;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/file.txt"), b"x").unwrap();

        // A source with `.` and `..` inside it, a destination with a trailing slash.
        let messy_source = root.join("sub/./../sub/file.txt");
        let messy_destination = root.join("out/");
        let planned = plan(CopyJob::copy(&messy_source, &messy_destination)).unwrap();

        assert!(planned.job.source.is_absolute(), "{:?}", planned.job.source);
        assert!(
            !planned
                .job
                .source
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir)),
            "no `.`/`..` left in the planned source: {:?}",
            planned.job.source
        );
        assert_eq!(planned.job.source, normalise_path(&root.join("sub/file.txt")));
        assert_eq!(planned.job.destination, normalise_path(&root.join("out")));
        assert!(
            !planned.job.destination.to_string_lossy().ends_with('/'),
            "trailing slash should be gone: {:?}",
            planned.job.destination
        );

        // A relative path is made absolute against the working directory, and `..` pops the
        // segment before it (`relative/..` cancels `relative`, not `some`).
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            normalise_path(Path::new("some/relative/../otherwise/path")),
            normalise_path(&cwd.join("some/otherwise/path"))
        );
        assert_eq!(
            normalise_path(Path::new("../x")),
            normalise_path(&cwd.join("../x"))
        );
        // `..` never climbs past the root.
        assert_eq!(normalise_path(Path::new("/../x")), PathBuf::from("/x"));
        // The normaliser is idempotent, which is what lets callers run it first without harm.
        let once = normalise_path(&messy_source);
        assert_eq!(normalise_path(&once), once);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_parents_are_resolved_but_the_final_component_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let real = root.join("real");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("payload.txt"), b"payload").unwrap();

        // A symlinked DIRECTORY: a destination reached through it resolves to the real path, so a
        // copy and any later comparison agree on which file they mean.
        let link_dir = root.join("linkdir");
        std::os::unix::fs::symlink(&real, &link_dir).unwrap();
        let through_link = plan(CopyJob::copy(
            real.join("payload.txt"),
            link_dir.join("nested/copy.txt"),
        ))
        .unwrap();
        assert_eq!(
            through_link.job.destination,
            normalise_path(&real.canonicalize().unwrap().join("nested/copy.txt")),
            "a symlinked parent must be resolved: {:?}",
            through_link.job.destination
        );
        assert!(
            !through_link.job.destination.starts_with(&link_dir),
            "the link name should not survive in the planned path: {:?}",
            through_link.job.destination
        );

        // A symlink as the FINAL component is NOT resolved: LinkPolicy owns that decision, and
        // `Preserve` exists to copy a link as a link rather than as its target.
        let final_link = root.join("dlink");
        std::os::unix::fs::symlink(&real, &final_link).unwrap();
        let keep_link =
            plan(CopyJob::copy(real.join("payload.txt"), final_link.clone())).unwrap();
        assert_eq!(
            keep_link.job.destination.file_name().unwrap(),
            "dlink",
            "the final component must be kept as given: {:?}",
            keep_link.job.destination
        );
    }

    #[test]
    fn the_public_helpers_are_the_ones_the_engine_uses() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.bin");
        let destination = tmp.path().join("destination.bin");
        fs::write(&source, b"hello tallow").unwrap();
        fs::copy(&source, &destination).unwrap();

        // The PUBLISHED BLAKE3 digest of the empty input: an external check that this really is
        // BLAKE3 with the standard 32-byte output, not a truncated or variant hash.
        let empty = tmp.path().join("empty.txt");
        fs::write(&empty, b"").unwrap();
        assert_eq!(
            hash_file_hex(&empty).unwrap(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );

        for policy in [SkipPolicy::SizeMtime, SkipPolicy::SizeMtimeHash, SkipPolicy::Hash] {
            assert!(
                destination_matches(&source, &destination, &policy).unwrap(),
                "{policy:?} should accept an exact copy"
            );
        }

        // Change the content WITHOUT changing the length, then match mtime back up: the cheap
        // policy is fooled by design, and the hashing policies must not be.
        fs::write(&destination, b"hello TALLOW").unwrap();
        let source_mtime = fs::metadata(&source).unwrap().modified().unwrap();
        filetime::set_file_mtime(
            &destination,
            filetime::FileTime::from_system_time(source_mtime),
        )
        .unwrap();
        assert!(
            destination_matches(&source, &destination, &SkipPolicy::SizeMtime).unwrap(),
            "SizeMtime trusts size+mtime by design - the documented risk, not a bug"
        );
        assert!(
            !destination_matches(&source, &destination, &SkipPolicy::SizeMtimeHash).unwrap(),
            "SizeMtimeHash must catch a same-length impostor"
        );

        // A missing destination is an error, not a silent false.
        assert!(
            destination_matches(&source, &tmp.path().join("absent.bin"), &SkipPolicy::Hash)
                .is_err()
        );
        println!("hash_file_hex = {}", hash_file_hex(&source).unwrap());
    }

    #[test]
    fn bundling_is_recommended_for_a_many_small_file_tree_only_across_a_slow_path() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        for index in 0..80 {
            fs::write(source.join(format!("f{index}.bin")), vec![1u8; 4096]).unwrap();
        }

        // Same volume: the hint still counts the small files, but must NOT recommend bundling -
        // the kernel amortises the per-file cost locally, so an archive would just add a copy.
        let local_destination = tmp.path().join("local-destination");
        let local_plan = plan(CopyJob::copy(&source, &local_destination)).unwrap();
        let local = local_plan.bundling_hint();
        assert_eq!(local.small_files, 80);
        assert_eq!(local.small_file_bytes, 80 * 4096);
        assert!(
            !local.bundle_recommended,
            "a same-volume copy has no per-file round trip to remove: {local:?}"
        );

        // Few files: below the threshold the archive is the overhead.
        let tiny_source = tmp.path().join("tiny");
        fs::create_dir_all(&tiny_source).unwrap();
        for index in 0..3 {
            fs::write(tiny_source.join(format!("t{index}.bin")), b"x").unwrap();
        }
        let tiny_plan = plan(CopyJob::copy(&tiny_source, tmp.path().join("tiny-dst"))).unwrap();
        assert!(!tiny_plan.bundling_hint().bundle_recommended);

        // Across a live SMB mount, if there is one: this is the case bundling exists for, and it
        // must be RECOMMENDED. Planning only stats; nothing is written to the share.
        let nas = Path::new("/mnt/NAS/<account>");
        if nas.is_dir() {
            let remote_destination = nas.join("_tallow-bundling-hint-probe");
            let remote_plan = plan(CopyJob::copy(&source, &remote_destination)).unwrap();
            let remote = remote_plan.bundling_hint();
            assert_eq!(remote.small_files, 80);
            assert!(
                remote.bundle_recommended,
                "80 small files across SMB is exactly what bundling is for: {remote:?}"
            );
            println!("bundling recommended across the live SMB mount: {remote:?}");
        } else {
            println!("no SMB mount present; the recommended case was not exercised here");
        }
    }

    #[test]
    fn a_copy_reports_its_phase_timings() {
        // 24 MiB: the millisecond-resolution copy phase cannot round down to zero on a fast
        // disk, so this fails if timing stops being recorded. What the figures *mean* is the
        // benchmark protocol's business; this only pins that they exist and add up.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        let mut data = Vec::with_capacity(24 * 1024 * 1024);
        while data.len() < 24 * 1024 * 1024 {
            data.extend_from_slice(&[0x5a; 1 << 20]);
        }
        fs::write(&source, &data).unwrap();

        let mut job = CopyJob::copy(&source, &destination);
        job.verify = VerifyPolicy::None;
        job.skip = SkipPolicy::SizeMtime;
        let built = plan(job).unwrap();
        let report = execute(&built);

        assert_eq!(report.copied_files, 1, "errors: {:?}", report.errors);
        assert!(
            report.timings.copy_ms > 0,
            "the copy phase must be timed: {:?}",
            report.timings
        );
        assert_eq!(
            report.timings.total_ms(),
            report.timings.plan_ms + report.timings.copy_ms
        );
        // A job with no verification must not claim any.
        assert_eq!(report.timings.verify_ms, 0, "{:?}", report.timings);
    }

    #[test]
    fn copy_replaces_a_read_only_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        fs::write(&source, b"new content").unwrap();
        fs::write(&destination, b"old").unwrap();
        let mut permissions = fs::metadata(&destination).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&destination, permissions).unwrap();

        let plan = plan(CopyJob::copy(&source, &destination)).unwrap();
        let report = execute(&plan);

        // Before the temp+rename change this failed: File::create on a read-only
        // destination returns EACCES.
        assert_eq!(report.copied_files, 1, "errors: {:?}", report.errors);
        assert_eq!(fs::read(&destination).unwrap(), b"new content");
    }

    #[test]
    fn copy_leaves_no_temporary_files_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.bin");
        let destination = tmp.path().join("dst.bin");
        fs::write(&source, vec![3_u8; 4096]).unwrap();

        let plan = plan(CopyJob::copy(&source, &destination)).unwrap();
        let report = execute(&plan);
        assert_eq!(report.copied_files, 1, "errors: {:?}", report.errors);

        let leftovers: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("tallow-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");
    }

    #[test]
    fn verification_and_threads_are_no_longer_mutually_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("src");
        let destination_root = tmp.path().join("dst");
        fs::create_dir_all(&source_root).unwrap();
        for index in 0..4 {
            fs::write(
                source_root.join(format!("file-{index}.bin")),
                vec![index; 8192],
            )
            .unwrap();
        }

        let mut job = CopyJob::copy(&source_root, &destination_root);
        job.threads = 2;
        job.verify = VerifyPolicy::FullHash;
        let plan = plan(job).unwrap();
        let report = execute(&plan);

        assert_eq!(report.copied_files, 4);
        assert_eq!(report.verified_files, 4);
        assert!(
            report.worker_threads_used >= 2,
            "verify mode should still use the worker pool, got {}",
            report.worker_threads_used
        );
        assert!(report.errors.is_empty(), "got {:?}", report.errors);
    }
    #[test]
    fn auto_threads_are_derived_once_the_tree_has_been_walked() {
        // Many small files on a local volume are the one case measured to benefit from concurrency
        // (2000 x 32 KiB went 440 -> 801 -> 868 MiB/s at 1 / 4 / 8 threads). `threads = 0` means
        // "derive it", and the answer needs the walk, so `plan` resolves it.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        for index in 0..80u8 {
            std::fs::write(source.join(format!("f{index}.bin")), vec![index; 4096]).unwrap();
        }

        let mut job = CopyJob::copy(&source, tmp.path().join("destination"));
        job.threads = 0;
        let planned = plan(job).unwrap();
        let cores = std::thread::available_parallelism().map(|c| c.get()).unwrap_or(1);
        assert_eq!(
            planned.job.threads,
            cores.clamp(1, SMALL_FILE_THREADS),
            "a local tree of small files must get the capped worker count, not the path-class 1"
        );
        assert!(planned.bundling_hint().small_files >= BUNDLE_MIN_SMALL_FILES);
    }

    #[test]
    fn auto_threads_leave_large_files_and_explicit_counts_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("big.bin"), vec![7u8; 8 * 1024 * 1024]).unwrap();

        // One large file on the same volume keeps the path-class baseline of a single worker:
        // measured as fast as four (3556 / 3531 / 3556 MiB/s at 1 / 2 / 4 threads).
        let mut job = CopyJob::copy(&source, tmp.path().join("dst-big"));
        job.threads = 0;
        assert_eq!(plan(job).unwrap().job.threads, 1);

        // An explicit count is the caller's and is never overridden.
        let mut job = CopyJob::copy(&source, tmp.path().join("dst-explicit"));
        job.threads = 3;
        assert_eq!(plan(job).unwrap().job.threads, 3);
    }
    #[test]
    fn skip_policy_manifest_behaves_exactly_as_hash() {
        // `SkipPolicy::Manifest` is documented as NOT IMPLEMENTED, substituted by a full hash
        // comparison - which is STRONGER than the manifest lookup the name promises, but different
        // work (it reads both files rather than consulting a manifest), and it is selectable from a
        // Tallow script. This pins the substitution so the doc and the behaviour cannot drift apart.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.bin");
        let destination = tmp.path().join("destination.bin");
        std::fs::write(&source, vec![5u8; 4096]).unwrap();

        std::fs::write(&destination, vec![5u8; 4096]).unwrap();
        for policy in [SkipPolicy::Hash, SkipPolicy::Manifest] {
            assert!(
                destination_matches(&source, &destination, &policy).unwrap(),
                "{policy:?} must see identical files as matching"
            );
        }
        std::fs::write(&destination, vec![6u8; 4096]).unwrap();
        for policy in [SkipPolicy::Hash, SkipPolicy::Manifest] {
            assert!(
                !destination_matches(&source, &destination, &policy).unwrap(),
                "{policy:?} must see differing files as differing"
            );
        }
    }
    #[test]
    fn strict_stops_at_the_first_failure_while_best_effort_finishes_the_tree() {
        // `ErrorPolicy` was declared, settable and never read: the engine behaved as BestEffort no
        // matter what a caller asked for, including its own default of Strict. These pin both halves
        // of the contract the enum now documents.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        // Named to sort first, so "the first failure" is a known file rather than a race.
        let unreadable = source.join("000-unreadable.bin");
        fs::write(&unreadable, vec![1u8; 2048]).unwrap();
        for index in 0..12 {
            fs::write(source.join(format!("file-{index:02}.bin")), vec![index as u8; 4096]).unwrap();
        }
        let mode_000 = std::os::unix::fs::PermissionsExt::from_mode(0o000);
        fs::set_permissions(&unreadable, mode_000).unwrap();

        // One worker: the sequential path, so which file stops the run is deterministic.
        let mut best_effort = CopyJob::copy(&source, &tmp.path().join("best-effort"));
        best_effort.threads = 1;
        best_effort.error_policy = ErrorPolicy::BestEffort;
        let report = execute(&plan(best_effort).unwrap());
        assert!(
            !report.errors.is_empty(),
            "the unreadable file must be reported"
        );
        for index in 0..12 {
            assert!(
                tmp.path().join(format!("best-effort/file-{index:02}.bin")).exists(),
                "BestEffort must carry on: file-{index:02} is missing"
            );
        }

        let mut strict = CopyJob::copy(&source, &tmp.path().join("strict"));
        strict.threads = 1;
        strict.error_policy = ErrorPolicy::Strict;
        let report = execute(&plan(strict).unwrap());
        assert!(
            !report.errors.is_empty(),
            "Strict still reports the failure - the caller learns from this list"
        );
        let attempted = (0..12)
            .filter(|index| tmp.path().join(format!("strict/file-{index:02}.bin")).exists())
            .count();
        assert!(
            attempted < 12,
            "Strict must stop at the first failure, but all {attempted} files were copied"
        );

        // And the parallel path (the collector) must stop the pool the same way.
        let mut parallel = CopyJob::copy(&source, &tmp.path().join("parallel"));
        parallel.threads = 4;
        parallel.error_policy = ErrorPolicy::Strict;
        let report = execute(&plan(parallel).unwrap());
        assert!(!report.errors.is_empty(), "the parallel path must report the failure");
        assert!(
            report.copied_files < 12,
            "the worker pool must be cancelled, not run to completion: copied {}",
            report.copied_files
        );
    }
    #[test]
    fn thread_derivation_follows_the_tree_not_a_fixed_number() {
        // The measurements this encodes: 8 threads beat 1 on a local small-file tree (868 vs 440
        // MiB/s), while 8 were ~6x slower than 1 for large local files (581 vs 3556 MiB/s) and ~26%
        // slower over CIFS (35 vs 47 MB/s). No single number is right for all three, so the count is
        // derived from the pair and the tree.
        let tmp = tempfile::tempdir().unwrap();
        let small_tree = tmp.path().join("small-tree");
        std::fs::create_dir_all(&small_tree).unwrap();
        for index in 0..100 {
            std::fs::write(small_tree.join(format!("f{index:03}.bin")), vec![1u8; 4096]).unwrap();
        }
        let large_tree = tmp.path().join("large-tree");
        std::fs::create_dir_all(&large_tree).unwrap();
        for index in 0..2 {
            std::fs::write(large_tree.join(format!("big{index}.bin")), vec![2u8; 4 * 1024 * 1024]).unwrap();
        }
        let destination = tmp.path().join("destination");

        let cores = std::thread::available_parallelism().map(|c| c.get()).unwrap_or(1);
        // The probe stops as soon as it has enough to decide, so a 100-small-file tree reports the
        // threshold rather than the total: that early exit IS the behaviour being pinned.
        assert!(
            probe_small_files(&small_tree) >= BUNDLE_MIN_SMALL_FILES,
            "a tree of small files must be recognised"
        );
        assert_eq!(probe_small_files(&large_tree), 0);
        assert_eq!(
            recommended_threads_for_tree_path(&small_tree, &destination),
            cores.clamp(1, SMALL_FILE_THREADS),
            "a local tree of small files should use the small-file count"
        );
        assert_eq!(
            recommended_threads_for_tree_path(&large_tree, &destination),
            recommended_threads(&large_tree, &destination),
            "a tree without small files keeps the path-class baseline"
        );
    }

    #[test]
    fn the_small_file_probe_respects_its_budget() {
        // Answering "is this a large-file tree?" must not cost a full walk of a large-file tree, so
        // the probe stops at its entry budget. Tested rather than trusted: the budget is an argument.
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("wide");
        std::fs::create_dir_all(&tree).unwrap();
        for index in 0..128 {
            std::fs::write(tree.join(format!("f{index:03}.bin")), vec![3u8; 8 * 1024 * 1024]).unwrap();
        }
        let (small, examined) = probe_small_files_within(&tree, 16);
        assert_eq!(small, 0);
        assert_eq!(examined, 16, "the probe must stop at the budget it was given");
    }
}

#[cfg(test)]
mod manifest_tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("alpha.txt"), b"alpha contents").unwrap();
        fs::write(dir.path().join("nested/beta.txt"), b"beta contents").unwrap();
        dir
    }

    #[test]
    fn a_manifest_round_trips_clean() {
        let dir = tree();
        let manifest = dir.path().join("m.tsv");
        assert_eq!(create_manifest(dir.path(), &manifest).unwrap(), 2);
        let report = verify_manifest(dir.path(), &manifest).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.checked, 2);
        assert!(report.extra.is_empty(), "the manifest itself is not an extra: {report:?}");
    }

    #[test]
    fn an_edit_that_keeps_the_size_is_caught_by_the_hash() {
        let dir = tree();
        let manifest = dir.path().join("m.tsv");
        create_manifest(dir.path(), &manifest).unwrap();
        // Same length, different bytes: a size-only manifest would call this a match.
        fs::write(dir.path().join("nested/beta.txt"), b"BETA contents").unwrap();
        let report = verify_manifest(dir.path(), &manifest).unwrap();
        assert_eq!(report.changed, vec!["nested/beta.txt".to_string()]);
        assert!(!report.is_clean());
    }

    #[test]
    fn a_removed_file_is_missing() {
        let dir = tree();
        let manifest = dir.path().join("m.tsv");
        create_manifest(dir.path(), &manifest).unwrap();
        fs::remove_file(dir.path().join("alpha.txt")).unwrap();
        let report = verify_manifest(dir.path(), &manifest).unwrap();
        assert_eq!(report.missing, vec!["alpha.txt".to_string()]);
        assert!(!report.is_clean());
    }

    #[test]
    fn an_extra_file_is_reported_and_left_alone() {
        let dir = tree();
        let manifest = dir.path().join("m.tsv");
        create_manifest(dir.path(), &manifest).unwrap();
        fs::write(dir.path().join("stray.txt"), b"not recorded").unwrap();
        let report = verify_manifest(dir.path(), &manifest).unwrap();
        assert_eq!(report.extra, vec!["stray.txt".to_string()]);
        // Reported, never deleted: a manifest records what was sent; it is not an authority to remove.
        assert!(dir.path().join("stray.txt").exists());
        assert!(report.is_clean(), "extra files do not make a tree fail verification");
    }

    #[test]
    fn a_manifest_cannot_name_a_path_outside_the_tree() {
        assert!(parse_manifest("../escape\t1\tabc").is_err());
        assert!(parse_manifest("/etc/passwd\t1\tabc").is_err());
        assert!(parse_manifest("fine\t1\tabc").is_ok());
    }

    #[test]
    fn a_malformed_line_is_an_error_not_a_skipped_entry() {
        assert!(parse_manifest("only-a-path\n").is_err());
        assert!(parse_manifest("path\tnot-a-number\tabc\n").is_err());
        assert!(parse_manifest("path\t3\t\n").is_err());
        assert!(parse_manifest("# comment\n\n  \n").is_ok(), "comments and blanks are fine");
    }

    #[test]
    fn transfer_machinery_is_not_recorded_as_content() {
        let dir = tree();
        fs::write(dir.path().join(".tallow-sync.db"), b"journal").unwrap();
        fs::write(dir.path().join("alpha.txt.tallow-partial"), b"half a file").unwrap();
        let manifest = dir.path().join("m.tsv");
        assert_eq!(create_manifest(dir.path(), &manifest).unwrap(), 2, "only the two real files");
        let report = verify_manifest(dir.path(), &manifest).unwrap();
        assert!(report.is_clean() && report.extra.is_empty(), "{report:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_not_followed_into_the_manifest() {
        let dir = tree();
        std::os::unix::fs::symlink("/etc/hostname", dir.path().join("link.txt")).unwrap();
        let manifest = dir.path().join("m.tsv");
        assert_eq!(create_manifest(dir.path(), &manifest).unwrap(), 2, "the link is not content");
        assert!(verify_manifest(dir.path(), &manifest).unwrap().is_clean());
    }
}
