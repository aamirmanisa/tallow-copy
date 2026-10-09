# Tallow Copy

Tallow Copy is a Tauri v2 desktop shell for a premium copy, mirror, sync,
and verify utility. The current app turns the approved circular premium design
into a working desktop console (Linux primary) with typed job planning, native copy
execution, native progress events, plan review, history, needs-review, and
Tallow preset surfaces.

The byte-copy hot path is not in the frontend. TypeScript owns the UI and
state, Tauri commands own desktop integration and validation, and Rust owns the
native planning/copy engine.

## Local Development

Install dependencies:

```sh
cd apps/tallow-copy
npm ci          # or: npm install
```

Run the browser-only frontend:

```sh
cd apps/tallow-copy
npm run dev
```

Build the frontend:

```sh
cd apps/tallow-copy
npm run build
```

Run the desktop app:

```sh
cd apps/tallow-copy
npm run tauri dev
```

Run the frontend build plus visual/layout checks:

```sh
cd apps/tallow-copy
npm run check
```

Run the Rust command/store tests:

```sh
cd apps/tallow-copy/src-tauri
$env:CARGO_HOME='E:\Nixe\tmp\cargo_home'
cargo test --offline
```

## Architecture

- `src/ui/` renders the app shell: active console, plan review, history, needs
  review, and script presets.
- `src/state/jobStore.ts` holds the frontend job request, plan, progress,
  history, review items, and preset state.
- `src/api/tauriClient.ts` wraps the Tauri command API and reports command
  errors when the UI is opened outside the Tauri runtime.
- `src-tauri/src/commands.rs` exposes the Tauri command boundary for planning,
  execution, state transitions, settings, path selection, and capabilities.
- `src-tauri/src/jobs/` contains shared Rust job types, the in-memory store,
  production native engine adapter. (A simulator module used to be compiled into release builds here; it was dead code and has been removed.)
- `crates/tallow-copy-engine/` contains the native Rust copy planner/executor
  used by the desktop app and the Tallow stdlib copy-engine re-export.

## Current Behavior

- Job planning validates empty, identical, and dangerous mirror paths.
- Destructive mirror deletes require explicit plan review before execution.
- Native execution performs real filesystem copy/sync/mirror operations,
  emits progress, completion, cancellation, and runtime error events, and
  supports bounded worker threads plus configurable buffers.
- Script presets are trusted built-ins that update production-supported
  transfer settings and leave source/target selection to the user.
- Visual checks cover the active console, compact desktop layout, plan review,
  and needs-review surfaces.

## Current Limitations

- Arbitrary Tallow script execution is intentionally not enabled yet.
- History and review data are in-memory only.
- Direct I/O (Linux-verified) and Windows security descriptors and IOCP (present, unverified) are
  not exposed; both are Windows-only or declined on this platform.
- Speed claims against Robocopy, FastCopy, or other tools require the benchmark
  harness from the native engine plan. The UI should not make raw throughput
  claims until that data exists.
- What the UI does expose, since the 2026-10-09 parity work: resume for plain copies, a fail-fast error
  policy, sampled and full-hash verification, `manifest` verification with a manifest path, the engine's
  bundling advice, a read-only Verify action, and history persisted beside the logs. Tallow script
  execution remains deliberately unavailable.

## Verification Notes

`scripts/playwright-browsers.mjs` resolves the browser install for `playwright.config.ts` and
`scripts/run-visual-tests.mjs` by looking for a runnable binary - the vendored `tmp/ms-playwright`
first, then `~/.cache/ms-playwright` - and leaves `PLAYWRIGHT_BROWSERS_PATH` unset if neither can
launch, so Playwright reports its own missing-browser error. A bare `npm run test:visual` works; an
explicit `PLAYWRIGHT_BROWSERS_PATH` is never overridden. If no browser is installed, run:

```sh
cd apps/tallow-copy
$env:PLAYWRIGHT_BROWSERS_PATH='E:\Nixe\tmp\ms-playwright'
npx playwright install chromium
```
