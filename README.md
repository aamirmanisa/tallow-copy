# Tallow Copy

A native copy engine, the CLI that drives it, and the desktop app built on both.

- `crates/tallow-copy-engine/` - the engine. Standalone: blake3, filetime and libc, nothing else.
  It plans and executes copies, mirrors and verifications, and is shared by the CLI, the Tallow
  scripting surface and the app, so a skip or verify verdict cannot differ between them.
- `apps/tallow-copy/` - the Tauri desktop app (its own README has the details).
- `docs/` - the engine architecture, the benchmark protocol and the audits, including the ones that
  found the gaps this repository has since closed.

## Building

Engine and CLI-side use:

    cargo build --release -p tallow-copy-engine

The desktop app:

    cd apps/tallow-copy
    npm ci
    npm run build
    npx tauri build          # deb + AppImage on Linux

Linux needs `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libfuse2` and `patchelf`. The engine itself
builds anywhere Rust does.

## Tests

    cargo test -p tallow-copy-engine          # engine
    cd apps/tallow-copy && npm run test:visual # UI layout, needs a browser

## Benchmarks

`docs/benchmarks/copy-benchmark-protocol.md` is the protocol; the numbers in it were measured on the
machine named there and are not portable claims. The engine's own perf smoke:

    cargo test -p tallow-copy-engine --test perf_copy_engine -- --ignored --nocapture

## License

MIT. See `LICENSE`.
