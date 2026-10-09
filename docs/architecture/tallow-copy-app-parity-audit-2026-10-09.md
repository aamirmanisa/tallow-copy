# Tallow Copy - app / code parity audit (2026-10-09)

Question asked: does the app have everything the code has? **No.** The engine, CLI and `.tl` surface are
ahead of the app in six places, and two of the app's own capability flags say so out loud. This is the
inventory, with the evidence at the time of writing.

## Method

Four surfaces were enumerated by extraction, not by reading docs, then compared:

- **engine** - 46 public items in `crates/tallow-copy-engine/src/lib.rs`
- **CLI** - 73 `Command` variants in `src/main.rs` (17 transfer-related)
- **`.tl`** - 125 declarations across `stdlib/transfer.tl` and `stdlib/copy_engine.tl`
- **app** - 11 Tauri commands in `apps/tallow-copy/src-tauri/src/commands.rs` plus the UI's controls, and
  the `JobRequest` shape the app can actually hand the engine (`jobs/types.rs:134`)

`JobRequest` is the decisive map: source, target, mode, filters, verify_mode, metadata_mode, backend_mode,
thread_count, buffer_size_bytes, delete_policy. Anything not expressible there cannot reach the engine,
whatever the UI implies.

## What the app does cover

Copy / Mirror / Sync modes (`TransferMode`, mapped in `jobs/engine.rs:315-320`); verify modes including
sampled hash, full hash and read-after-write; metadata modes; backend auto vs thread pool; thread count and
buffer size; delete policy with a mandatory review gate for mirror deletes
(`allow_mirror_deletes_without_review: false`, `jobs/store.rs:338`, refusal pinned by
`planning_rejects_mirror_deletes_without_review`); plan / execute / pause / resume / cancel; progress,
timings and ETA; job history and needs-review views; transfer presets; source/target selection.

## Gaps - code has it, the app does not

**G1. "AUTO · 16 workers" is a label, not a derivation.** The frontend's `threadCount` default is the
literal `16` (`src/state/jobStore.ts:105`) and it is sent as a fixed count. The engine's derivation
(`recommended_threads_for_tree_path`, the `-j 0` default the CLI uses) is never called from the app, and
no app code mentions `recommended_threads`. The UI therefore claims an automatic choice it is not making -
the same advertised-but-inert class as the `ErrorPolicy` knob. Cheap to fix: send `0` and let the engine
derive, or call the deriver and display its answer.

**G2. Resume is unreachable.** `CopyMode::Resume` exists in the engine, `tallow copy --resume` exposes it,
and `.tl` can select `"resume"`. The app has no `TransferMode::Resume`, no UI control, and its capability
struct says `supports_resume: false` (`jobs/engine.rs:186`).

**G3. The verification audit is unreachable.** `tallow verify-transfer` and `Transfer.verify_transfer` both
exist. The app contains **zero** references to `audit` or `verify_transfer`; REPORTS -> *Needs review* is a
plan review, not a read-only comparison of two trees. So the app cannot answer "did the transfer actually
land?" - the one question the audit exists for.

**G4. Manifest verification is an inert surface, and now a naming collision too.** `VerifyMode::Manifest`
is in the app's type (`jobs/types.rs:28`, serialised and asserted in a test) but the adapter reports
`supports_manifest: false` (`jobs/engine.rs:187`) and `manifest_path` is always `None`
(`jobs/engine.rs:737`). Meanwhile the engine now ships a real manifest (`create_manifest`,
`verify_manifest`, `MANIFEST_HEADER`) - which is a *file format*, whereas the engine's
`VerifyPolicy::Manifest` is a documented hash substitution. Two different things under one word; wiring the
app to the file manifest means picking the word apart first.

**G5. Bundling hints are never surfaced.** The engine computes `BundlingHint` and probes the tree
(`probe_small_files`, `BUNDLE_MIN_SMALL_FILES`). The app has **zero** references to bundling, so a plan's
recommendation never reaches the person who could act on it.

**G6. Error policy is not selectable.** The engine honours `ErrorPolicy::Strict` (stop the pool on first
error) versus best-effort. The app cannot choose, so every app-run job is best-effort and a user who wants
fail-fast cannot get it.

**G7. Job history does not survive a restart.** History is built from live progress events
(`historyEntryFromProgress`) and held in memory. A surface labelled REPORTS that empties on relaunch is a
gap in its own right, independent of G3.

**G8 (not a gap).** `supports_security_metadata: false`, `supports_direct_io: false` and the absent IOCP
backend are Windows-only capabilities the adapter advertises honestly rather than pretending. Same for
`s3://` paths, which are CLI-only by design.

## Smaller asymmetry

The `.tl` surface does not expose the tree manifest; `scripts/nixe-manifest.tl` implements it over
`Transfer.hash` and `fs.*`. A first-class binding should be added when the app gets G4, so the app, the
CLI and scripts all move through one implementation.

## Suggested order

1. **G1** - it is a lie in the UI for the price of a one-line change (or a real derivation, which is barely
   more).
2. **G3 + G7** - the app's whole point is answering "did it land?", and today it cannot.
3. **G2** - resume already exists everywhere else; the app is the only surface missing it.
4. **G4** - resolve the naming collision, then wire, then add the `.tl` binding.
5. **G5, G6** - both small, both real.

None of these is a defect in the engine, the CLI or the scripts: those four surfaces agree with each other
and are covered by tests (engine 45, integration 39, full library 6349 at `a2f86a0`).

## Status: closed (2026-10-09, same day)

All six gaps below are implemented and committed. The audit above is kept as written, because it is
the record of what was found; this section is what happened to it.

- **G1 threads** - `0` means derive; `effective_thread_count` is the single owner, used by both the job
  builder and the plan, so what runs and what the UI shows cannot disagree (`0cf4367`).
- **G2 audit** - the walk moved into the engine and the standard library now delegates to it (60 lines
  of duplicated counting removed). The app has a Verify action, a command and a report in Needs review
  (`beebeb9`).
- **G3 resume** - mapped to the engine's resume mode for plain copies only, with `supports_resume`
  true at last (`0cf4367`).
- **G4 manifest** - the collision is resolved by making `VerifyPolicy::Manifest` do what its name says:
  a manifest is required, validated before the transfer, and checked once over the finished result.
  The app can supply a manifest path and advertises the mode; `.tl` gained `manifest_create` and
  `manifest_check` (`2140368`, `bf8758d`, `1cf866a`).
- **G5 bundling** - the hint travels on the plan and through the warnings the UI already renders
  (`0cf4367`).
- **G6 error policy and history** - a fail-fast switch selects the engine's strict policy, and finished
  jobs are persisted beside the logs, written by the UI from what it observed so a job that failed
  halfway cannot be filed as complete (`0cf4367`, `2755dd4`).

Verified at `1cf866a`, and by real runs rather than by reading: engine 52, app 44, tallow integration
43 across six binaries, frontend build clean, visual suite 4 - all exit 0.

Two acknowledged limits, neither of which is a closed gap pretending otherwise:

- The GUI's Verify button and Manifest path field are verified by types, build and the visual suite's
  rendered assertions, not by a click-through in a running window - both need a folder/file dialog.
- `scripts/nixe-manifest.tl` still emits the manifest format itself rather than calling the new
  binding. Worth consolidating now that the surface exists; outside this audit's scope.
