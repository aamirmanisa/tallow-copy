# Tallow Copy — Specification Audit (2026-10-08)

Audit of the Tallow Copy specification corpus against the implementation as it stands today
(`master`, after the Tier-1/2/3 optimisation work and the policy-reconciliation commits
`b35a486`, `2275051`, `0fda74c`, `daa6f35`).

Every factual claim below was checked against source or against a measurement taken on this
workstation. Where a check could not be run, it says so instead of assuming.

## 1. Corpus and method

| Document | Lines | Role |
|---|---|---|
| `docs/architecture/tallow-copy-engine.md` | 76 | Architecture contract |
| `docs/benchmarks/copy-benchmark-protocol.md` | 173 | Metric + gate protocol |
| `docs/TALLOW-CAPABILITIES.md` (§5, CLI table) | 897 | Capability claims |
| `docs/plans/2026-05-12-tallow-ultrafast-copy-tool.md` | 602 | Implementation plan (phases 0-7) |
| `docs/plans/2026-05-12-tallow-copy-tauri-app.md` | 763 | Desktop app task plan |
| `docs/plans/2026-05-12-tallow-copy-tauri-app-design.md` | 222 | Desktop app design |
| `apps/tallow-copy/README.md` | 106 | App README |
| `TALLOW_SPEC.md` | 246 | Language/compiler spec |

Method: each normative claim was traced to the code that should implement it (engine, runtime
binding, CLI, `.tl` surface, app) or to a fresh measurement. Two searches in this audit returned
false negatives that were corrected before use — `rg --vss` and `rg --audit-transfers` matched
nothing because `rg` parsed the leading `--` as an option terminator, and `rg 'tallow copy'`
missed the subcommand because the CLI declares it as a clap variant (`main.rs:476`), not as that
literal string. Both were re-run correctly; the corrected results are the ones reported here.

## 2. Verdict summary

| Spec | Verdict | Headline defect |
|---|---|---|
| Architecture contract | **Partly inaccurate** | Goal 1 ("one implementation shared by…") is not true of the CLI or `.tl`; `sync` mode is not what the table says |
| Benchmark protocol | **Unrunnable as written here** | Names a test target that does not exist; demands phase timings the report cannot produce; baselines are Windows-only |
| Capability claims | **Overstated, then corrected** | "104 Functions" was stale (fixed); `audit-transfers` does not exist (documented); `--vss` was parsed and discarded - **now refused with exit 64** instead of silently ignored |
| Ultrafast-copy plan | **Sound, superseded in part** | Its own 1000x framing is honest; phases 1-4 are largely delivered, 5 is not started |
| Tauri app plan/design | **Honest, historically stale** | Task 6 still describes a simulated-progress stub; the app now drives the real engine |
| Language spec | **Not contradicted** | Interpreter-first reality vs an LLVM build spec; relevant only to the integration plan |

## 3. Findings

**F1 — The architecture's first goal is not met: there are three copy stacks, not one.**
The contract says one implementation is shared by `copy.file`, `copy.dir`, `transfer.fast_copy`,
CLI `copy`, CLI `delta-sync`, and the app. Verified reality:

- engine `plan()`/`execute()` — driven by the **desktop app only**
  (`apps/tallow-copy/src-tauri/src/jobs/engine.rs:167,201,224,424`); zero references from
  `src/main.rs` or `src/stdlib/`.
- `.tl` `FileCopy.copy` → `src/stdlib/transfer.rs::copy_file_with_fallback`, which uses the
  engine's single-file helper but none of its planning.
- CLI `tallow copy` / `delta-sync` → `transfer.rs`'s own planners
  (`twoway_compute_plan`, `twoway_execute_plan`, `delta_scan`), which duplicate skip and verify
  semantics. The >1 GiB lazy-hash defect fixed this session existed **in both** copies, which is
  exactly the cost of that duplication.

*Correction to my own earlier statement:* I previously said `plan()`/`execute()` were driven by
nothing. That is wrong — the app drives them. The accurate statement is that they are unreachable
from `.tl` and the CLI.

**F2 — `sync` is an alias, not a mode.** `lib.rs:316` documents it as "one-way sync without
deletions (same as Copy)", yet the mode table claims "incrementally update destination according
to skip policy". Incrementality is the skip policy's behaviour; the mode adds nothing. One of the
two must change.

**F3 — The policy tables do not distinguish implemented from refused.** `Verify=manifest` and
`Metadata=security|owner` are now refused with an `Unsupported` error (`validate_policies`, called
from `plan()` and `execute()`), and `Skip=manifest` still aliases `hash`. The table lists them as
if all values were live. This is the documentation half of the honesty work — a reader cannot tell
which selections are honoured.

**F4 — Pipeline step 1 ("Normalize and validate paths") is not implemented.** The engine
validates existence (symlink-aware), but performs no normalisation: it contains exactly one
`canonicalize`, added by the symlink loop guard. Relative paths, `..` segments, trailing slashes
and symlinked parents are passed through as given.

**F5 — The plan's own Phase 5 is untouched.** The architecture doc promises "adaptive I/O
backends"; the plan's Phase 5 is "Scheduler And Fast IO Backends". There is no `io_uring`,
`copy_file_range` or `FICLONE` anywhere in `src/` or `crates/` — those strings appear only in
`apps/tallow-copy/docs/research/*`. Sparse handling (`SEEK_DATA`/`SEEK_HOLE`) *is* implemented, so
the sparse half of Phase 4 is done and the clone/range half is not.

**F6 — The benchmark protocol names a harness that does not exist.**
`cargo test --test perf_copy_engine copy_engine_perf_smoke_tiny_dataset -- --ignored` cannot run:
there is no `tests/perf_copy_engine.rs`, and the test name appears nowhere in the repository except
this protocol (plus its copy under `artifacts/` and a claims ledger). `tests/` contains
`copy_engine_basic.rs`, `copy_mtime_sparse.rs`, `perf_binary_startup.rs`, `perf_install.rs` — the
copy-engine smoke described in the protocol is missing.

**F7 — The metric list asks for values the data contract cannot produce.** The protocol requires
scan/plan/copy/verify/total seconds, physical bytes, peak RSS and CPU percent. `CopyReport`
exposes seven fields — `copied_files`, `skipped_files`, `verified_files`, `deleted_files`,
`bytes_copied`, `worker_threads_used`, `errors` — with no timing or resource data. As written the
protocol cannot be completed by any run of the engine.

**F8 — The protocol is Windows-shaped while the operative environment is not.** Its datasets,
baselines (`robocopy`, `Copy-Item`, `fastcopy`) and harness scripts are PowerShell and Windows
paths. This workstation is Linux; the real transfer targets are an SMB/CIFS NAS and a a remote server box
over SSH. The `.ps1` scripts exist (`scripts/generate_copy_benchmark_dataset.ps1`,
`scripts/benchmark_copy_engine.ps1`) but have never been executed here, and none of the measured
work in this session could use them.

**F9 — Two gates are unreachable on the share they are most likely to be run against.** "Warm
no-op sync ≥100x" and "no-op sync >1000x with a manifest cache" assume destination-side change
detection works. On the CIFS mount, mtime writes are accepted and then re-stamped by the server
within seconds (measured, recorded in the `nas-backup` skill): no mtime-based no-op can
ever fire there, and a plain rsync push re-sends the whole tree every time. The gates are
achievable only via a source-keyed manifest or an explicit change list. The protocol should state
that mechanism as a precondition rather than leave it implied.

**F10 — Capability claims that are not implemented.**
- "TallowTransfer (104 Functions)": the surface declares **121 functions and 91 structs**
  (212 typed items) in `stdlib/transfer.tl`.
- "`tallow audit-transfers` — show transfer audit log": no such subcommand exists in the CLI.
- "VSS Locked Files": `DeltaSync { vss: bool }` exists (`main.rs:579`) and WAS **discarded** —
  `vss: _vss`. **Fixed 2026-10-08:** passing `--vss` now prints why it cannot work and exits 64
  (USAGE_ERROR) before touching either tree, with a regression test in `sync_safety_guards`.
  Previously the flag was accepted and did nothing, the same inert-surface
  defect class as the engine's old verify/link policies. `VssCopy` appears nowhere in source.
- "Air-Gap QR" / "Air-Gap Audio": research documents only. `--copy-acls` is honest: it is
  implemented for Windows and warns loudly when `icacls` is absent.

**F11 — The ultrafast plan is honest and largely delivered.** It explicitly refuses the universal
"1000x" framing ("no software can be 1000x faster than well-tuned robocopy/FastCopy" for
byte-for-byte copy) and confines 1000x claims to sparse/reflink/copy-range and manifest-cached
no-op classes. Phases 1-2 (skeleton, streaming copy, real verification) and Phase 4 (metadata,
links, sparse) are delivered; Phase 3 (restartable jobs) is delivered at the *per-file* level
(resume from a deterministic partial sibling) but the engine still has no journal; Phase 5 is
untouched (F5); Phase 6 (Tallow API parity) is partial — see §6.

**F12 — The app plan is stale in the direction of understating.** Its Task 6 ("Add Simulated
Progress Events… For the stub executor, simulate…") describes the stub era. The app now drives
`native::plan` / `native::execute` / `native::execute_with_control` against the real engine. The
plan should be marked superseded rather than left to read as current state.

## 4. Recommended specification updates

**`docs/architecture/tallow-copy-engine.md`**
1. Replace goal 1 with the truth plus a target: state that the engine is driven by the app today,
   that `.tl` and the CLI still use `transfer.rs` planners, and that consolidation is an explicit
   open item (see §6).
2. Mode table: mark `sync` as an alias of `copy` (or define the distinct semantics intended).
3. Policy tables: add an "Implemented?" column; mark `verify:manifest` and
   `metadata:security|owner` as *refused*, `skip:manifest` as *alias of `hash`*, and give each
   skip policy its cost model — `size_mtime` = O(files) stats; `size_mtime_hash` = stats + full
   hash of both files **on the matching path** (the assurance variant, and the only one that
   catches a same-size/same-mtime impostor); `hash` = O(bytes) always.
4. Pipeline: either implement step 1 or restate it as "validate paths; normalisation is the
   caller's responsibility".
5. Add a short "State of the optimisation phases" section pointing at the plan's phases and which
   are done (1-4), partial (3, 6) and not started (5).

**`docs/benchmarks/copy-benchmark-protocol.md`**
1. Delete or implement the `perf_copy_engine` smoke (F6). If implemented, it should be the Linux
   entry point, not a Windows one.
2. Add a POSIX/Linux baseline column: `cp -a`, `rsync -a`, `tar | tar` pipeline, plus the
   measured reference numbers already established here — SMB write 43.6 MB/s / read 50.9-55.3
   MB/s; concurrency 1/2/3/4 = 47.2/45.2/35.1/38.4 MB/s (no scaling); small files 7-11 ms each;
   bundling 1000 × 64 KiB = 2.1x; WAN single stream 13.27 MB/s, two streams 25.20 MB/s aggregate
   (1.90x, ceiling ~201 Mbit/s, two lanes sufficient).
3. State the change-detection mechanism as a precondition for the no-op gates (F9).
4. Add a "path class" dimension to every gate: same-volume, cross-volume, SMB/NAS, WAN. The
   measured scaling behaviour differs per class, and a single global thread count is wrong for all
   of them.
5. Make the metric list satisfiable: either the engine emits timings (recommended, see §5) or the
   protocol declares that the harness measures them externally.

**`docs/TALLOW-CAPABILITIES.md`**
1. Correct the TallowTransfer count (121 functions, 91 structs) or drop the number and link the
   surface.
2. Remove or implement `audit-transfers`; mark `--vss` as *accepted and ignored on Linux* until it
   either refuses loudly or is implemented; keep the air-gap QR/audio material clearly labelled as
   research, not capability.

**`docs/plans/2026-05-12-tallow-copy-tauri-app.md`** — add a header noting Tasks 1-11 were
executed and that the app now runs the real engine, so the stub-era tasks read as history.

## 5. Metric adjustments

Add to the engine's data contract (small, unblocks the whole protocol):

- `CopyTimings { scan_ms, plan_ms, copy_ms, verify_ms }` on `CopyReport`, and a `total_ms`
  accessor. Phase boundaries already exist in the pipeline; they simply are not recorded.
- `peak_rss_bytes` (or `Option<u64>`) — optional, sampled by the caller on platforms without a
  cheap in-process read.
- `physical_bytes_written` where the platform reports allocation (sparse copies make
  logical ≠ physical, and the protocol asks for both).
- `streams_used` (already have `worker_threads_used`) and `path_class` as a caller-supplied label,
  so a run is self-describing.

Change the reported units where the measurement demands it:

- Report **verify cost as a fraction of copy time**, not as a separate headline: measured
  read-back saving is ~11 % (cache-resident), not the 30-45 % once assumed.
- Report **files per second** separately for small-file trees: throughput on a 1 KiB tree is
  ~0.14 MB/s while the meaningful figure is ~141 files/s.
- Report the **bundling ratio** (archives vs file-by-file) as a first-class number for
  small-file-dominated trees.

## 6. Integration strategy for the Tallow language

The architecture doc already states the intended split: *Tallow scripts own policy and
orchestration; Rust owns scanning, planning, byte movement, verification, metadata and I/O.* That
split is currently aspirational, because `.tl` cannot reach the engine's plan/execute at all.

**Stage 1 — expose the engine to `.tl` (the real gap). — DONE 2026-10-08.**
`src/stdlib/copy_engine.rs` binds `CopyEngine.plan` / `.execute` / `.copy`; the types
(`CopyEnginePlan`, `CopyEngineAction`, `CopyEngineReport`) are declared in `stdlib/transfer.tl`.
The plan crosses the boundary as an ordinary value, so Tallow can inspect it (actions, totals,
policies) before deciding to execute, and **no engine state is parked between calls** — the plan
record is rebuilt into a `CopyJob`/`CopyPlan` on the way back in, where the engine re-validates
every policy, so a hand-written plan cannot smuggle past what a Rust caller would be refused.

Verified end to end by `tests/copy_engine_binding.rs` (4 tests through real Tallow source against
a real temp directory): plan inspection, execution with the bytes checked on disk, a dry run that
writes nothing, an unsupported policy refused, and an unknown policy name refused with the valid
values listed. `tallow doctor --stdlib-drift` covers the declaration surface.

Pitfall worth knowing before adding another stdlib module: a new capitalized module needs **three**
registrations, and missing any one produces a confusing "undefined variable: <Module>" instead of
a routing error — the dispatch arm in `src/stdlib/mod.rs`, the `__module_<Name>__` namespace arm
in `src/interp.rs` (the one that makes `CopyEngine.plan(...)` parse as a namespace method call),
and the declarations in `stdlib/*.tl`.

**Stage 2 — move orchestration to `.tl`. — PARTLY DONE 2026-10-08.**
`scripts/nixe-workstation-to-nas.tl` gained `NAS_PLAN_ONLY=1`: it builds the engine's plan for
every top-level entry and reports the engine's own numbers (files, bytes, the small-file split and
the bundling recommendation) instead of counting members with the shell, then stops without
creating the destination or building an archive. Verified end to end on a local fixture:
`dirA files=80 ... small=80 actions=81 bundle=no`, `dirB files=2 bytes=3145729 ... bundle=no`,
`PLAN ONLY: files=82 bytes=3145809 entries_recommending_bundle=0 (nothing was written)`, with the
destination directory confirmed absent afterwards. `bundle=no` is correct there and is the rule
working: the destination was local, where bundling buys nothing.

NOT done, deliberately: the driver still archives one tar per top-level directory. Replacing that
with direct tree copies would change what lands on the NAS (a tar to extract rather than a tree -
the convention that lets DSM File Station extract directly), which is a data-layout decision for
the owner, not a refactor. The inspection half is the half that was safe to take.

**Stage 3 — collapse the CLI onto the same engine. — FIRST SLICE DONE 2026-10-08.** The duplicated
skip/hash semantics are now single-owner: `transfer.rs`'s streaming BLAKE3 is deleted in favour of
the engine's `hash_file_hex`, and the engine publishes `destination_matches(source, destination,
skip)` so a scanning caller can reach the engine's verdict instead of re-deriving it. Proof of
equivalence, not assertion: the `delta_scan` output (path, size, BLAKE3) for a fixture containing a
text file, a 3 MiB random file and an empty file was captured with the OLD binary, then re-captured
with the rebuilt one - identical on all four lines. The empty file's digest is the published BLAKE3
empty-input vector, so the hasher is externally confirmed rather than self-referential. Still
outstanding below.

**`tallow copy` now runs the engine's plan/execute (2026-10-08).** The local directory branch used
to carry its own tree walk, its own `len != size || mtime != mtime` comparison (a third copy of
`SkipPolicy::SizeMtime`) and its own per-file copy loop. It now builds a `CopyJob`, plans, and
executes - so the CLI gains the engine's policies, phase timings, kernel fast paths and the
path-class stream count, and one fewer implementation of walking and skipping exists.

Every policy is pinned to what the command already did, verified against a before/after run rather
than assumed: `skip = SizeMtime` (the comparison its output named), `metadata = Timestamps`,
`error_policy = BestEffort`, `verify = None`. The metadata choice came from the BASELINE, not from
reading the code: the old copy preserved mtimes nanosecond-exactly (run 2 skipped all 3 files), and
the engine's `CopyJob::copy` default is `DataOnly` - adopting the default would have silently
broken re-run skipping.

Two behaviour changes, both intentional and both flagged: the engine creates empty directories the
old walk skipped (`diff -r` is now clean), and a failed copy reports its errors and exits non-zero
(previously `WARN` plus exit 0).

**The rest of stage 3 is scoped, not started.**
Doing this needs three things the engine does not have, and the order matters because reversing it
regresses `delta-sync`:

1. **A two-way reconciliation capability.** `delta_scan` / `twoway_scan` / `twoway_compute_plan`
   in `src/stdlib/transfer.rs` compute added/removed/modified sets on BOTH sides and can apply a
   delta; the engine's `Mirror` only deletes destination entries that are missing from the source,
   which is strictly one-directional. The engine must be able to express "changed over there" before
   the CLI can depend on it.
2. **Block-level delta, or an explicit decision not to move it.** `transfer.rs` carries block delta
   via the `fast_rsync` path. The measured position is that block delta is a NETWORK optimisation
   and is ~100x slower than a plain copy for a local destination, so the honest options are to
   implement it behind a path-class gate or to leave it in `transfer.rs` and say why - not to
   silently drop it when the CLI is repointed.
3. **Flag mapping with no silent losses.** `delta-sync`'s `--block-level`, `--threads`, deletion
   policy and `--quiet` must map onto engine policies one-for-one; any flag with no engine
   equivalent has to refuse loudly rather than be ignored, which is the same rule that caught
   `--vss` and `MetadataPolicy::Security`.

**The duplication that actually hurt is narrower than "move delta-sync".** The costly defect (the
>1 GiB lazy-hash skip that existed in BOTH the engine and `transfer.rs`) came from duplicated
SKIP/VERIFY SEMANTICS, not from the two-way planner itself. The cheapest safe slice is therefore to
have `transfer.rs`'s scan delegate its skip/verify decisions to the engine's policy code, leaving
the two-way planner where it is - removing the duplicated semantics without relocating a
reconciler. Do that first, measure nothing regressed, and only then decide whether the planner
itself should move.

**Stage 4 — `.tl` as the benchmark harness.**
A `.tl` harness driving the engine (with `process.exec` now correctly argv-spreading after this
session's fix) replaces the Windows-only `.ps1` path for Linux runs and makes the protocol
executable here.

**Stage 5 (optional, large) — compiled parity.**
`TALLOW_SPEC.md` describes an LLVM/inkwell build; the shipping runtime is an interpreter with a
codegen module. Compiled `.tl` copy drivers are a project, not a step, and nothing in the copy
work depends on them today.

**Compatibility constraints that must hold through all stages**

- **Never rename a declared `.tl` field or parameter** — the runtime keys on the exact string.
- **Per-Interpreter state only.** The MLS registry was moved onto `Interpreter` for exactly this
  reason; any new copy state (job journals, plan caches) must live there too, not in a new global.
  `MIX_CHANNELS` remains the one known instance of the old pattern.
- **No public engine API changes are needed.** The binding adapts to the engine, not the reverse;
  the app's call sites must keep compiling untouched.
- **Platform features fail loudly, never silently.** `--vss` (inert) and the old
  `MetadataPolicy::Security|Owner` (ignored) are the two instances already found; new bindings
  must refuse rather than no-op.
- **The CLI's existing behaviour is a compatibility surface** — `delta-sync`'s two-way semantics
  must be preserved or deliberately versioned.

## 7. Prioritised roadmap

| # | Action | Impact | Effort |
|---|---|---|---|
| 1 | Add `CopyTimings` (+ optional RSS/physical bytes) to `CopyReport` | Unblocks the entire metric protocol | S |
| 2 | Fix the three capability/spec inaccuracies (F2 sync, F3 refusal column, F10 counts/flags) | Removes claims a reader cannot trust | S |
| 3 | Delete or implement the `perf_copy_engine` smoke reference (F6) | Removes a check that cannot run | S |
| 4 | Add the POSIX baseline + path-class gates and the change-detection precondition (F8, F9) | Makes the protocol executable where the work happens | M |
| 5 | Implement path normalisation or amend pipeline step 1 (F4) | **Done (2026-10-08)**: `normalise_path` runs inside `plan()` - absolute, `.`/`..` collapsed lexically, trailing slashes gone, symlinked PARENTS resolved via canonicalize (nearest existing ancestor for a not-yet-existing destination), final component left to `LinkPolicy`. Verified by two engine tests and a real CLI copy through `./inner/../inner` into `out/./nested/../flat/`. | M |
| 6 | Stage 1 `.tl` binding of plan/execute | Closes the architecture's stated split; enables §6 stages 2-4 | M |
| 7 | Clone/range fast paths (`copy_file_range`, `FICLONE`) + io_uring evaluation (F5) | **Done for the fast paths, measured 8.7x on a same-filesystem 512 MiB copy** (2,088 -> 18,202 MB/s); io_uring now measured and declined - see the io_uring evaluation section: the loop plateaus at 256 KiB-1 MiB and does not improve with threads, so syscall cost is not the bottleneck; io_uring still not evaluated | L |
| 8 | Stream count derived from path class (LAN = 1, WAN = 2) | **Done (2026-10-08)**: `recommended_threads` + `threads = 0` = auto in the `.tl` binding; SMB detected by both magics after a live mount corrected the first attempt | S |
| 9 | Collapse the CLI onto the engine (needs a delta capability first) | **Done for the copy path (2026-10-08).** `tallow copy` directories now plan/execute through the engine (policies pinned to the old behaviour and baseline-verified); `delta-sync`'s per-file copies were ALREADY engine-backed via `copy_file_with_fallback`, and its stream count now comes from the engine's measured rule with `-j 0` (default 8 left alone: changing it is the owner's call). The two-way PLANNER stays in `transfer.rs` deliberately - reconciliation against remembered state is a different capability from copying, and the harmful part (duplicated skip/verify semantics) is now single-owner. | L |
| 10 | Small-file bundling as a first-class strategy in the engine/spec | **Done engine-side (2026-10-08)**: `CopyPlan::bundling_hint()` + the three fields on the `.tl` plan record, keyed to the destination path class; verified against the live SMB mount (80 small files -> recommended). The archiving itself stays with the caller, since a tar changes what lands on the far side | M |

## 8. Not verified / out of scope

- The Windows baselines (`robocopy`, `fastcopy`, `Copy-Item`) cannot be executed on this
  workstation, and the `.ps1` harnesses have never been run here. Every Windows-only gate is
  therefore **unverified**, not satisfied.
- `apps/tallow-copy` cannot currently build (missing untracked `icons/icon.png`), so no app-level
  benchmark or UI verification was possible in this audit.
- `TALLOW_SPEC.md`'s grammar and compliance passes (`@legal`, privacy qualifiers) were read for
  integration constraints only; no line-level audit of its claims was performed.
- No claims are made here about `tallow-search/`, `hackthecart/` or other trees.

## App integration (verified 2026-10-08)

The Tauri app is the engine's only other caller, and its side of the seam was **gated out of test
builds** (`#[cfg(not(test))]` on `NativeTallowEngine`, its request mapping and its plan
translation), with `SimulatedEngine` - a stand-in that touches no disk and no engine - covering the
tests instead. So the integration had never been compiled in a test build, let alone exercised, and
that is why no app-side verification was possible (together with the missing `icons/icon.png`, which
broke `tauri::generate_context!` for every cargo command).

Now: every `#[cfg(not(test))]` gate in `jobs/engine.rs` is removed, so the real integration compiles
and is tested. App crate `cargo test`: **30 passed / 0 failed** (the suite could not run at all
before). Covered - the mapping table (including Mirror-without-delete-permission degrading to Sync
rather than Mirror, and the concrete thread count never reaching the engine as `0`, where 0 means
"derive from the path class"); a real plan + execute through the app's own mapping with the bytes
checked on disk; the incrementality of a repeat Sync (incremental, because the app's declared default
metadata mode is `Timestamps`); the `Update`-vs-`Copy` translation; the progress/summary builders;
and engine-error categorisation.

Found and fixed by that pass: `progress_from_report` and `summary_from_report` computed
`files_copied = copied_files + verified_files`. On a copy-with-verification job the same files are in
both counts, so a two-file copy under the app's default Size verification reported **4 of 2 files**
(measured: `verified_files == copied_files == 2`). Both now use `copied_files`; verified files are a
subset of copied files for every mode the app can request.

Also confirmed honest: `capabilities()` declares `supports_resume`, `supports_manifest`,
`supports_security_metadata` and `supports_direct_io` all **false**, matching what the engine does.

Still not covered initially: the Tauri event plumbing (the `emit` calls and the worker thread in
`run_native_execution`). Driving it needed an `AppHandle<Wry>` - Tauri's mock runtime is a different
runtime parameter, and making the trait runtime-generic would break its object safety.

That is now closed by a port rather than a mock: the worker takes `Arc<dyn JobHost>` - one method to
emit a payload, one to record completion, one to record failure - which the app implements over its
Tauri handle and managed `JobStore`, and which a test implements with a recorder. The worker is
synchronous itself (the spawn happens in `execute`), so a test runs the whole execution and asserts
the event sequence, the payloads, and the terminal state with no display and no webview. App crate
suite: **31 passed / 0 failed**.

What is still unverified is only the app's *window*: real rendering, real user interaction, and the
glue in `execute` that wraps the handle and spawns the thread. The icon fix is what made running the
app possible at all if that is ever wanted.

## io_uring evaluation (2026-10-08)
**Verdict: not justified for the engine's data path on the measured evidence.** The project's own
research already set that bar - `apps/tallow-copy/docs/research/agent-09-implementation-stack-and-tallow-rust-integration.md`
says "do not make io_uring an MVP dependency", "optimize with large buffers, batching, platform copy
APIs, and worker scheduling **before** io_uring", and "consider io_uring backend on Linux only if
benchmarks justify".

Method: `crates/tallow-copy-engine/tests/io_uring_evaluation.rs` (ignored by default; dataset outside
the repository per `docs/benchmarks/copy-benchmark-protocol.md`). Two runs of the same 512 MiB dataset
on btrfs/nvme - the buffered loop with `TALLOW_COPY_DISABLE_FAST_PATH=1`, then the same dataset with
fast paths enabled as the ceiling - because that loop is exactly what an async backend would replace.

| case | MiB/s |
| --- | --- |
| buffered, 1 thread, 64 KiB / 256 KiB / 1 MiB / 4 MiB / 16 MiB buffers | 2681 / 3765 / 3631 / 2612 / 2008 |
| buffered, 1 MiB buffer, 1 / 2 / 4 threads | 3556 / 3531 / 3556 |
| kernel fast path, 1 MiB, 1 thread | 20480 |
| small files (2000 x 32 KiB), buffered, 1 / 4 / 8 threads | 440 / 801 / 868 |
| small files, fast path | 590 |

Reading it: the loop **plateaus at 256 KiB-1 MiB and degrades with larger buffers**, and adding threads
does not raise the ~3.5 GiB/s plateau - so per-operation syscall cost is not the limit, and that is the
only cost io_uring removes. The kernel fast path is **4.2-5.8x faster** (its own measurement varied
between 15,058 and 20,480 MiB/s across runs, against the loop's ~3.5 GiB/s plateau) precisely because
it never enters userspace at all. The operational targets are the NAS (47 MB/s measured) and the a remote server link
(25 MB/s), one to two orders of magnitude below the loop's plateau, so their bottleneck is the network
and no syscall interface changes that. The one place concurrency clearly pays is many small files
(440 -> 868 MiB/s at 8 threads) - already available through `threads`, and it is concurrency, not
asynchronous I/O, that buys it.

Honest limits: one machine (btrfs on nvme), one run each, and the numbers carry noise on both sides -
a repeated 1-thread row read 356 MiB/s instead of 3556 (a transient writeback stall the following run
absorbed), and the fast-path ceiling itself came out 15,058 MiB/s in one run and 20,480 in another.
That is why the comparison is quoted as the range 4.2-5.8x rather than a single figure. So this is "not justified here", not a universal claim. Tallow's own io_uring runtime
(`src/runtime/io_uring_runtime.rs`, feature `io-uring-backend`) is not reusable by the engine in any
case: the engine crate is standalone and publishable, so it must not depend on the `tallow` crate;
io_uring there would mean a new optional dependency in the engine itself.

## Derived worker count now follows the tree (2026-10-08)

The io_uring sweep above produced this as a side finding: many-small-file copies scale with
concurrency (440 -> 801 -> 868 MiB/s at 1 / 4 / 8 threads) while the auto thread derivation knew
only the path class, and returned 1 for a local volume. `threads = 0` is now resolved by `plan`
through `recommended_threads_for_plan` (baseline, raised to `SMALL_FILE_THREADS` = 4, capped by the
machine, when the plan found >= 64 small files on a local volume), so the plan carries the count
that will be used and every caller - CLI, `.tl` binding, app - reports the same number. Large-file
plans and explicit counts are untouched. Verified end to end: `workers 4` on 80 x 4 KiB files,
`workers 1` on a single 8 MiB file, and the `.tl` binding test reads the derived value back.

## The app's log is no longer Windows-only (2026-10-08)

Found while running the app for the first time: `diagnostics_dir()` read `LOCALAPPDATA` and fell back
to the temp directory. That is right on Windows and wrong everywhere else - on Linux every log landed
in `/tmp` and died with the next reboot, so a user reporting a problem had nothing to attach, and the
path the UI shows them pointed at a file that would not survive the night.

Now: `LOCALAPPDATA` on Windows, `XDG_DATA_HOME` (or `~/.local/share`) on Linux, `~/Library/Application
Support` on macOS, with the temp directory still the fallback when the environment offers no data root
- refusing to log is worse than logging to temp. `reveal_diagnostics_log` had the same assumption,
hard-coding `explorer.exe`; it now uses `explorer.exe /select,` / `open -R` / `xdg-open <folder>` per
platform, which is the closest each one has to "show me this file".

Verified: app suite 35 passed (4 new diagnostics tests pinning the platform rule, the fallback, the
"not in temp" regression, and the reveal invocation), and a real run on a virtual display writes
`~/.local/share/Tallow Copy/logs/tallow-copy.log` with nothing left in the temp directory, still
logging `starting Tallow Copy application` then `frontend event subscriptions ready`.

## `tallow verify-transfer`: auditing a finished transfer, read-only (2026-10-08)

The engine could always answer "what would a synchronisation do?"; nothing could answer "what did the
transfer I already ran actually leave behind?" without running a sync and hoping. `verify-transfer`
compares a target against a source and reports three things: files the policy considers identical,
files missing or different (to copy), and files the target holds that the source does not (extra).
`--verify size` uses the same size+mtime rule a real transfer uses, `hash` confirms matches by reading
both sides, `hash-all` ignores size and mtime. Exit code 0 when in sync, 9 for differences, 64 for an
unknown `--verify`.

It is implemented as **the engine's own plan**, taken in mirror mode with `dry_run`, and never
executed - mirror planning is what surfaces the target's extras, which a copy plan cannot see. That is
the design point: the audit cannot disagree with the transfer it audits, because it is the same skip
rule. `execute` is not called, so it is read-only by construction rather than by promise, and a test
snapshots both trees (paths, sizes, mtimes) before and after an audit that found differences.

What it does not prove, and the help says so: under the default size+mtime policy a file whose
contents changed while keeping its size AND its modification time looks identical. That pair is pinned
by a test which asserts the blindness of `--verify size` and the detection by `--verify hash`, so the
limitation cannot quietly disappear from the help while the test stays green.

Verified: 8 end-to-end tests through the real binary (clean after a genuine `tallow copy`; missing file;
extra file; the content-change pair; nothing written to either tree; the JSON verdict; exit 64 on a bad
mode) plus a library-level check that a sync's own `.tallow-sync.db` is not reported as an extra. The
`is_sync_bookkeeping` predicate was promoted from a nested function in the delta-sync arm to one shared
owner in the library, used by the CLI walkers, the recursive scanner and this audit alike; the
delta-sync guard suite stays at 16/16.

Naming note: `audit-transfers` already existed as the transfer **log** viewer. This command was first
added under that name and collided with it - an earlier note in `docs/TALLOW-CAPABILITIES.md` claiming
the log command did not exist was simply wrong (it had been checked with a lowercase/hyphenated grep,
which cannot match clap's CamelCase variant). The claim is corrected there and the collision is why
this one is called `verify-transfer`.

## Inert policy knob: `ErrorPolicy` (2026-10-08)

Sweeping every policy enum against its uses found one that was declared, settable and never read:
`ErrorPolicy`. Three references existed in the entire engine - the enum, the struct field, and the
default value - so every job behaved as `BestEffort` regardless of what a caller chose, including the
engine's own default of `Strict`. The CLI's copy arm sets `BestEffort` explicitly, so the command line
could never have exposed it; an embedder asking for `Strict` got `BestEffort` in silence, which is the
same failure mode the `--vss` refusal exists to prevent.

Fixed by implementing the policy rather than refusing it: both the sequential loop and the parallel
collector stop when `Strict` meets its first error (the collector cancels the pool), and the enum
finally carries doc comments describing both behaviours. `cargo test -p tallow-copy-engine` 35 passed,
including a new test that pins both halves with a mode-`000` file named to sort first.

Method note: the CLI-side sweep used the compiler as the authority instead of name matching - a flag
bound and never read is an unused-variable warning, and none of the 151 flags produced one (the
warnings that do exist are pre-existing `src/browser/*`). That found nothing; the enum sweep found
this. A binding read *only* into a no-op field would pass the compiler check, which is how
`error_policy` stayed invisible.

## The app window, actually looked at (2026-10-08)

Previous app verification only established that the process runs without crashing. The window is now
captured and read: it is a 960x680 "Tallow Copy - Production Transfer Console" (dark theme), rendering
a left sidebar (JOBS: Active/Plan; REPORTS: History/Needs review; TEMPLATES: Transfer Presets), a
toolbar with Plan (primary) and disabled Execute/Cancel, a Copy/Mirror/Sync segmented control showing
"Copy" selected with an "AUTO - 16 workers" badge, empty SOURCE/TARGET fields with Browse buttons, a
file table reading "No files planned yet.", and an inspector with SPEED/ETA/FILES/ERRORS cards, a NEXT
UP prompt, and OPTIONS showing Full hash verify off, **Mirror deletes off**, buffer 32 MB, threads 16,
backend Auto, over a status bar reading "Ready / 0% / 0 runtime errors / AUTO idle - No transfer
planned". Every region is styled and none is blank or unstyled: this is a real UI in a clean
pre-flight empty state, not a broken render.

Two environment traps made this look impossible first, and both are worth keeping:

- **A GUI app on this box may not map its window onto an Xvfb display at all unless the toolkit backend
  is forced.** Under `xvfb-run` alone the process ran, loaded its frontend (the frontend itself logged
  "frontend event subscriptions ready"), and spawned WebKit's process tree - while the X server held
  exactly one window (the root). With `GDK_BACKEND=x11` set, the 960x680 window appears immediately.
- **This machine has a live Wayland session** (`niri --session`, `wayland-1`, Xwayland on `:0`).
  Capturing "the display" with a hard-coded `:0.0` therefore reads the user's real Xwayland root, not
  the test display - it produced a blank black frame, which was mistaken for a rendering failure. The
  capture must use the display `xvfb-run` actually handed the child (`$DISPLAY`), and the app must be
  given `GDK_BACKEND=x11` so it cannot land on the live session. Nothing leaked onto that session
  (`niri msg windows` shows no Tallow Copy window) and nothing sensitive was captured - the wrong-display
  frames were pure black with only a pointer - but the capture was still aimed at the wrong screen.

Recipe: `xvfb-run -a --server-args="-screen 0 1440x900x24" bash -c 'export GDK_BACKEND=x11
LIBGL_ALWAYS_SOFTWARE=1; unset WAYLAND_DISPLAY; ./target/debug/tallow-copy & sleep 25;
ffmpeg -f x11grab -video_size 1440x900 -i "$DISPLAY" -frames:v 1 out.png'`.


## Platform scope: Linux primary (2026-10-08)

Decided: Linux is the primary and only verified target. The capability table previously listed x86-64
Windows as primary, which was never true of the work - the Windows paths compile but have never been
run, and the benchmark protocol's Windows harnesses are explicitly unverified. The table and the app
README now say Linux primary with Windows/macOS marked best-effort and unverified. The app's bundle
targets changed from "all" to `deb` + `AppImage`, with the PNG icon listed (a Linux bundle cannot use
the .ico), so a `tauri build` produces something installable rather than attempting every platform's
bundler. This is a scope statement, not a portability claim: an unverified platform's bug report is a
report against an unsupported configuration.


Verified the Linux bundle rather than assuming it: `npx tauri build --bundles deb` produced
`Tallow Copy_0.1.0_amd64.deb` (3,242,734 bytes) whose `ar` members are `debian-binary`,
`control.tar.gz` and `data.tar.gz`, whose control file names Package `tallow-copy`, Version `0.1.0`,
Architecture `amd64` and Depends `libwebkit2gtk-4.1-0, libgtk-3-0`, and whose payload ships
`usr/bin/tallow-copy`, a `.desktop` entry and hicolor icons. The first attempt failed for an
environment reason worth noting: the local `@tauri-apps/cli` was missing its Linux native binding (the
npm optional-dependencies bug), fixed with `npm i --no-save @tauri-apps/cli-linux-x64-gnu@<version>`.
**AppImage: built and verified - but not by `npx tauri build --bundles appimage` on this host.**
`Tallow_Copy-x86_64.AppImage` (109,300,216 bytes, a static-pie ELF) was produced and checked. Its payload
extracts to `AppRun`, `apprun-hooks`, `AppRun.wrapped`, `Tallow Copy.desktop`, `tallow-copy.png` /
`Tallow Copy.png` and `usr/` holding the 13,359,393-byte binary and 175 deployed libraries; the desktop
entry names Exec `tallow-copy`, StartupWMClass `tallow-copy` and the correct icon. Run on a test X display
(`GDK_BACKEND=x11`, Xvfb `:99`) it renders the full console - title "Tallow Copy - Production Transfer
Console", roughly 950x680, matching the window verified from the deb - with Mirror deletes and Full hash
verify off by default. The capture was deleted after being read.

Tauri's own invocation fails before that, and its message misleads: `failed to run linuxdeploy`.
`linuxdeploy --version` runs fine here and libfuse2 is present, so this is neither FUSE nor a failure to
start. The bundle aborts inside linuxdeploy, whose **bundled `strip` cannot read `.relr.dyn` sections**
(RELR relative relocations, which modern system libraries use): `Strip call failed: ... unknown type
[0x13] section '.relr.dyn'` for each library it stages. Tauri pins a linuxdeploy built 2024-07-26; the
current build (369, 2026-10-01) handles them, so bundling succeeds when that linuxdeploy is invoked
directly, with the plugin directory pointed at Tauri's downloads:

    LINUXDEPLOY_PLUGIN_DIR=~/.cache/tauri linuxdeploy --appdir "Tallow Copy.AppDir" --plugin gtk --output appimage

`APPIMAGE_EXTRACT_AND_RUN=1` does not help - the pinned tool is executed, not mounted - recorded here so
the next person does not spend the attempt. Nothing was wrong with the application; the defect is in the
pinned packaging tool.
