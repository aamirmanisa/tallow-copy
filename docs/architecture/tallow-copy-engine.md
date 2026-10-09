# Tallow Native Copy Engine Architecture

The native copy engine is the shared Rust implementation behind Tallow copy,
mirror, sync, verify, resume, and future desktop app jobs. Tallow scripts own
policy and orchestration. Rust owns scanning, planning, byte movement,
verification, metadata, journals, and platform I/O.

## Goals

- One implementation shared by `copy.file`, `copy.dir`,
  `transfer.fast_copy`, `transfer.fast_copy_dir`, CLI `copy`, CLI
  `delta-sync`, and the Tauri Tallow Copy app.
  **True for copying, deliberately not for reconciliation (verified 2026-10-08).**
  Shared by every one of them now: byte movement (`tallow_copy_engine::copy_file`, which
  `transfer.rs::copy_file_with_fallback` calls - so every per-file copy `delta-sync` performs goes
  through the engine), content hashing (`hash_file_hex`; one implementation, after the two copies
  had already drifted once and the defect had to be fixed twice), and the skip/verify verdicts
  (`destination_matches`).
  Planning is shared by the app, CLI `copy` and the Tallow surface: `CopyEngine.plan` /
  `CopyEngine.execute` / `CopyEngine.copy` (declared in `stdlib/transfer.tl`, bound in
  `src/stdlib/copy_engine.rs`), with `tallow copy` pinning `SkipPolicy::SizeMtime` +
  `MetadataPolicy::Timestamps` to keep exactly the behaviour it had. See
  `tests/copy_engine_binding.rs` and the CLI evidence in
  `docs/architecture/tallow-copy-spec-audit-2026-10-08.md` section 6.
  **NOT shared, on purpose:** `delta-sync`'s two-way reconciliation (`twoway_compute_plan`,
  `delta_scan`, `delta_diff`) and its SQLite state. Deciding what changed on BOTH sides against
  remembered state is a different capability from copying - this engine is a stateless copier with
  three dependencies (blake3, filetime, libc), and giving it a state database and merge rules would
  serve this sentence rather than any user. What was genuinely harmful was the duplicated
  skip/verify SEMANTICS (the >1 GiB lazy-hash defect existed in both copies at once), and that is
  now single-owner.
- Bounded-memory data movement for large files.
- Honest policies for skip, verify, metadata, links, and error handling.
- Structured plans and reports that can power CLI, Tallow scripts, Tauri UI,
  and benchmark harnesses.
- Clear path to faster backends such as Windows overlapped I/O, Linux
  `io_uring`, sparse copy, block delta, and clone/copy-range.

## Modes

| Mode | Description |
| --- | --- |
| `copy` | Copy missing or changed source entries to destination. |
| `mirror` | Make destination source-equivalent, including deletes. |
| `sync` | **Alias of `copy` in this engine** (`lib.rs`: "one-way sync without deletions (same as Copy)"). The incrementality the name suggests comes from the skip policy, not the mode. Give it distinct semantics or keep the synonym - do not document it as a third behaviour. |
| `verify` | Verify destination against source or manifest without copying. |
| `resume` | Continue an interrupted journaled job. |
| `dry_run` | Plan work without writing or deleting. |

## Policies

Nothing here is silently ignored: a policy this build cannot honour fails at plan time and again
at execute time (`validate_policies`), so a hand-built `CopyPlan` cannot bypass the check either.

| Policy | Values | State (verified 2026-10-08) |
| --- | --- | --- |
| Skip | `size_mtime`, `size_mtime_hash`, `hash`, `manifest` | All planned. `manifest` is an **alias of `hash`**: it does a real full-hash comparison, it simply has no manifest backing. |
| Verify | `none`, `size`, `sampled_hash`, `full_hash`, `read_after_write`, `manifest` | All implemented (verified 2026-10-09). `manifest` needs a manifest: the plan requires one, names what is missing, and parses it before the transfer; the finished result is then checked once, and a mismatch is reported as `VerifyMismatch` naming the file. `extra` entries are reported, not errors. It used to be refused - it was made real rather than renamed once the engine shipped a manifest. A copy action's `reason` states the verification that will actually run. |
| Metadata | `data_only`, `timestamps`, `attributes`, `security`, `owner`, `all` | `security` and `owner` are **refused** with `Unsupported`; the rest are implemented. |
| Links | `skip`, `preserve`, `follow` | Implemented. `preserve` recreates the link and copies zero bytes; `follow` descends under an ancestor loop guard. |
| Errors | `best_effort`, `strict` | Implemented. |

Skip-policy cost model - the cheapest-looking choice is not always the cheap one:

- `size_mtime` - two stats per file, O(files). Cannot see a same-size, same-mtime change.
- `size_mtime_hash` - as above plus a full read and hash of **both** files wherever they already
  match, i.e. O(bytes) over the in-sync majority. This is the assurance variant and the only skip
  policy that catches a same-size/same-mtime impostor; dropping the hash would make it identical
  to `size_mtime`.
- `hash` - reads and hashes both files unconditionally, O(bytes) always.
- `manifest` - as `hash`.

## Data Contract

`CopyJob` describes a job request: source, destination, mode, skip policy,
verify policy, metadata policy, link policy, error policy, thread count, and
dry-run flag.

`CopyPlan` is a deterministic list of actions. Actions are typed as copy, skip,
verify, delete, mkdir, metadata, or error. Mirror deletes must be visible in the
plan before execution.

`CopyReport` records copied, skipped, verified, deleted, bytes copied, structured errors, and
phase timings (`CopyTimings`: `plan_ms`, `copy_ms`, `verify_ms` and a `total_ms()` accessor).
Scanning and planning are ONE phase here (`plan_ms`) because the plan is built during the walk;
`verify_ms` is a subset of `copy_ms` because files are verified as they finish. No other engine
state is needed to satisfy the benchmark protocol's metric list.

`CopyError` records category, message, optional path, retryability, and a
recommended action. UI and CLI code should not parse raw OS error strings.

## Execution Pipeline

1. **Normalise** and validate paths (existence is checked symlink-aware). Normalisation is
   performed by `plan()` through `normalise_path`, so every caller gets it: paths become absolute,
   `.` and `..` are collapsed lexically, trailing slashes go, and symlinked PARENT directories are
   resolved through `canonicalize` (walking up to the nearest existing ancestor for a destination
   that does not exist yet). The FINAL component is deliberately NOT resolved - whether a symlink
   is followed, recreated or skipped is `LinkPolicy`'s decision, and `Preserve` exists to copy a
   link as a link. Nothing on disk is touched: this is lexical work plus a parent lookup. Pinned by
   `paths_are_normalised_before_planning` and
   `symlinked_parents_are_resolved_but_the_final_component_is_left_alone`.
2. Scan source and destination metadata.
3. Build a typed plan using the requested skip and mirror policies.
4. Execute actions through a bounded-memory file copy path.
5. Verify according to policy by reading destination bytes when full
   verification is requested.
6. Apply metadata according to policy.
7. Commit journal state and return a structured report.

## First Skeleton

The first skeleton intentionally favors correctness and consolidation over
maximum speed. It supports size/mtime skip, dry-run, mirror delete planning,
basic recursive copy, and destination-byte full-hash verification.

The later optimization phases replace the simple copy executor with streaming
copy, restart journals, metadata fidelity, sparse handling, and adaptive I/O
backends without changing the top-level contract.

### Worker count comes from the tree and the path class, not from the core count (verified 2026-10-08)

`recommended_threads(source, destination)` is the path-class baseline - what the measurements
support on that pair of devices alone:

| Path class | Streams | Evidence |
| --- | --- | --- |
| Same volume | 1 | The kernel clones (`FICLONE`) or copies the range; extra workers only contend. |
| SMB/CIFS | 1 | Measured over Wi-Fi: 47.2 / 45.2 / 35.1 / 38.4 MB/s at 1 / 2 / 3 / 4 streams - no scaling, and four streams are slower than one. |
| Anything else | 2 | A WAN SSH pull measured 13.27 MB/s on one stream, 25.20 MB/s on two (1.90x), 25.16 MB/s on three: the ceiling arrives at two lanes. |

Detection is by filesystem, not by name: same device id means the same volume, and the
destination's `statfs` magic identifies SMB. BOTH SMB magics are required - the share measured
here is `vers=3.0`, which reports `fe534d42` (type `smb2`), while the older CIFS value is
`ff534d42`; checking only the latter classified a live SMB mount as an unknown filesystem. An
explicit thread count always overrides this.

A path class cannot see the tree, and for one workload it is the tree that matters. Many small files
are the only case measured to gain from concurrency:

| Local small-file copy | Throughput |
| --- | --- |
| 2000 x 32 KiB, 1 thread | 440 MiB/s |
| 2000 x 32 KiB, 4 threads | 801 MiB/s |
| 2000 x 32 KiB, 8 threads | 868 MiB/s |
| one 512 MiB file, 1 / 2 / 4 threads | 3556 / 3531 / 3556 MiB/s |

So `threads = 0` is resolved by `plan`, once, through `recommended_threads_for_plan`: the baseline,
raised to `SMALL_FILE_THREADS` (4, capped by the machine's parallelism) when the volume is local and
the plan found at least `BUNDLE_MIN_SMALL_FILES` small files. Four is chosen over the measured eight
because 4 -> 8 buys about 8% on small files and eight threads measured *slower* than four for large
ones (581 vs 3556 MiB/s), and a real tree holds both. Large files do not regress at 2-4 threads, so
raising the count on a mixed tree is safe. SMB and WAN keep their baselines: on SMB more streams were
slower, and over the WAN the link, not the syscall path, is the limit.

Resolving it in `plan` rather than in each caller means the plan carries the count that will actually
be used, which is what the CLI prints, the `.tl` binding returns as `plan.threads`, and the app
reports. Verified end to end: `tallow copy` over 80 x 4 KiB files reports `workers 4`, and over a
single 8 MiB file reports `workers 1`; a Tallow script planning the same small-file tree with
`threads = 0` reads back the derived count, and an explicit count is never overridden.

### Bundling is a decision the plan reports, not a behaviour it imposes (verified 2026-10-08)

`CopyPlan::bundling_hint()` returns `{small_files, small_file_bytes, bundle_recommended}`, and the
`.tl` plan record carries the same three fields, so a script can act on the plan instead of
re-deriving the judgement. The rule is keyed to the DESTINATION's path class, not to file size
alone:

- many small files (>= 64 at <= 256 KiB) crossing an SMB/CIFS mount -> **recommended**: measured
  1000 files of 64 KiB at **2.1x** as a single tar versus file-by-file;
- the same tree to a same-volume destination -> **not** recommended, because the kernel already
  amortises the per-file cost and an archive would only add a copy.

The engine deliberately does not bundle by itself: an archive changes what lands on the far side
(a tar to extract rather than a tree), and that is the caller's decision - which is exactly the
split this document describes.

### State of those phases (verified 2026-10-08)

| Phase (`docs/plans/2026-05-12-tallow-ultrafast-copy-tool.md`) | State |
| --- | --- |
| 1-2 skeleton, streaming copy, real verification | done |
| 3 restartable jobs | partial: per-file resume from a deterministic partial sibling; no engine-side journal |
| 4 metadata fidelity, links, sparse | done (`SEEK_DATA`/`SEEK_HOLE`) |
| 5 scheduler and fast I/O backends | **partly started (2026-10-08)**: the engine's copier now tries reflink (`FICLONE`) and `copy_file_range` before its buffered loop, with the same chunking, progress and cancellation granularity as that loop. Measured on btrfs: 512 MiB through the engine at 2,088 MB/s buffered vs 18,202 MB/s with the kernel paths, **8.7x**. `io_uring` was **evaluated and declined** (2026-10-08): the buffered loop plateaus at 256 KiB-1 MiB, gets *worse* with larger buffers, and does not improve with threads, so syscall cost is not the bottleneck; the kernel fast path is 4.2-5.8x faster. See the audit document. A job in resume mode never takes a fast path: a reflink is atomic, so an interruption could leave neither a finished file nor a resumable partial. `TALLOW_COPY_DISABLE_FAST_PATH=1` forces the buffered path for measurement or as a workaround. |
| 6 Tallow API and compiled parity | done for this engine: the `.tl` surface reaches `plan()`/`execute()` through the binding (`tests/copy_engine_binding.rs`), including the derived worker count |
| 7 benchmark harness | Linux smoke exists (`crates/tallow-copy-engine/tests/perf_copy_engine.rs`); the protocol's Windows harnesses have never been run on this hardware |

## Error policy: BestEffort and Strict

`CopyJob::error_policy` decides what happens after a file fails, and it is honoured by both execution
paths. A caller learns the job failed from `CopyReport::errors` being non-empty (the CLI exits 5).

- `BestEffort` records the failure and copies the rest of the tree, so the destination is as complete
  as the tree allowed.
- `Strict` records the failure and stops: the sequential loop ends before the next action, and the
  parallel collector cancels the worker pool instead of draining its queue, so a Strict run can leave
  the destination only partly populated.

This knob was previously declared, defaulted and never read - every job behaved as `BestEffort`
whatever a caller asked for, *including the engine's own default of `Strict`*. The CLI sets
`BestEffort` explicitly (its long-standing warn-and-continue contract), which is why nothing in the
product could notice. `CopyJob::copy` still defaults to `Strict`, now truthfully. Pinned by
`strict_stops_at_the_first_failure_while_best_effort_finishes_the_tree`.


## The CLI's thread count is derived too (2026-10-08)

`delta-sync` used to default to eight workers. Eight is right for exactly one measured case and wrong
for the others, so the default is now `-j 0` (derive), resolved through
`recommended_threads_for_tree_path`:

| Case | Measured | Chosen |
| --- | --- | --- |
| Local tree of small files | 440 MiB/s at 1 thread, 801 at 4, 868 at 8 | 4 (the 4 -> 8 gain is ~8% and 8 was slower for large files) |
| Local large files | 3,556 MiB/s at 1 thread vs 581 at 8 | 1 |
| CIFS/SMB | 47.2 MB/s at 1 stream vs 35.1 at 4 | 1 |
| WAN | 25.20 MB/s at 2 streams vs 13.27 at 1 | 2 |

The path class answers the last three; the first needs to know what the tree holds, which is what the
planner already computes for a `CopyPlan`. `delta-sync` has no plan, so it uses `probe_small_files`:
a bounded probe that examines at most `SMALL_FILE_PROBE_ENTRIES` (4096) entries and stops as soon as it
has seen `BUNDLE_MIN_SMALL_FILES` (64) of them. Answering "is this a large-file tree?" must not cost a
full walk of a large-file tree, so the budget is an argument (`probe_small_files_within`) and is tested
rather than trusted. The probe does not follow links: it is a scheduling probe, not a security
boundary, but it must not traverse out of the tree it was pointed at.

An explicit `-j N` still wins untouched.

## Platform: Linux is the primary target

The engine and the CLI are developed, measured and verified on Linux, and that is the only platform
they are claimed for.

- **Verified here:** the buffered copier, the kernel fast paths (`FICLONE` reflink, `copy_file_range`,
  `SEEK_DATA`/`SEEK_HOLE` sparse handling), path-class classification from `statfs` magic numbers
  (including SMB's `fe534d42`), and every number quoted in this document.
- **Present but NOT verified:** the `#[cfg(windows)]` blocks in the CLI, the remaining Windows-first
  assumptions in the app (the diagnostics path was made platform-correct on 2026-10-08), the macOS
  paths, and the Windows harnesses in the benchmark protocol. They compile; nothing here has ever run
  them. Their presence is not support.
- **The app ships Linux bundles** (`deb`, `AppImage`) - both built and verified (see the bundle
  section of `tallow-copy-spec-audit-2026-10-08.md` for the packaging-tool caveat). A Windows build would need an explicit
  `--bundles msi`/`nsis` and is untested.

So a Windows or macOS defect report is a report against an unsupported configuration, and the honest
answer is "unverified" rather than "should work".
