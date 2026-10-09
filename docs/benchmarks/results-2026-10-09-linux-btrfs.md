# Measured results: Tallow Copy vs rsync (POSIX baseline)

Provenance, not marketing. Every number here was produced by `run_posix_bench.sh` in this
directory, on one Linux workstation, on its own storage. Treat them as "what this machine
did", not as portable claims. Re-run the harness to reproduce or refute them.

- **Date:** 2026-10-09
- **Machine:** 8-core x86_64, 15 GiB RAM, NVMe, btrfs (`/home`), warm page cache
- **Tools:** `tallow` (release build, this commit), `rsync 3.5.1`, GNU `cp`
- **Method:** destination deleted and `sync`d before every run; 3 samples per row, median
  reported; every command aborts the run on a nonzero exit, so a broken command cannot be
  recorded as a fast one

## Status of this document

The project's benchmark protocol forbids throughput claims against other tools until a
harness produces that data. **That harness has now been run against rsync on Linux, and the
results are below.** The protocol's Windows baselines — Robocopy and PowerShell `Copy-Item` —
remain **unverified**: they are Windows-only, and nothing in this document measures them.

## Why the large-file rows need a caveat before they are read

On a copy-on-write filesystem, Tallow Copy uses reflink cloning, so a "copy" can be O(1)
rather than a byte transfer. Both `cp` and Tallow Copy beat rsync by a wide margin on 4 GiB
**for that reason**, not because of throughput:

| Row | Time | Read as |
| --- | --- | --- |
| `cp -a` (default) | 0.012 s | clone, O(1) |
| `tallow copy` | 0.038 s | clone, O(1) |
| `tallow delta-sync` | 0.028 s | clone, O(1) |
| `cp -a --reflink=never` | 3.112 s | **real bytes, 1316 MB/s** |
| `rsync -a` | 5.452 s | no clone support, ~751 MB/s |

The `cp --reflink=never` row is the control: same tool, same data, same disk, 259x slower.
That difference can only be the clone. **A clone row must never be quoted as a throughput
figure.** On a target that cannot clone — ext4, NTFS, a NAS over CIFS/SMB — the engine has no
clone path, and this document does not measure its byte throughput at 4 GiB.

## 4 GiB of large files (4 x 1 GiB)

| Command | Median | Notes |
| --- | --- | --- |
| `tallow copy` | 0.038 s | clone-assisted |
| `tallow delta-sync` (derived threads) | 0.028 s | clone-assisted |
| `tallow delta-sync -j 4` | 0.026 s | clone-assisted |
| `rsync -a` | 5.452 s | ~751 MB/s |
| `cp -a --reflink=never` | 3.112 s | ~1316 MB/s, byte-copy reference |

## 20,000 small files (20 KiB each, 391 MB)

Clone-free comparison, both tools writing to tmpfs where no reflink is possible and both
targets are identical:

| Command | Median | Throughput |
| --- | --- | --- |
| `tallow copy` | 0.566 s | **691 MB/s** |
| `rsync -a` | 1.224 s | 319 MB/s |

That is **2.16x in favour of Tallow Copy** on a byte-for-byte, clone-free workload.

On btrfs, where cloning is available, the same rows were `tallow copy` 1.059 s and
`rsync -a` 1.502 s. The small-file clone control: `cp -a` took 0.651 s but
`cp -a --reflink=never` took 9.434 s, so cloning was in play for the CoW-aware tools there.

## Warm no-op (nothing to copy)

| Command | Median |
| --- | --- |
| `tallow delta-sync` | 0.052 s |
| `rsync -a` | 0.103 s |

## Incremental: 1% of files changed, 1% deleted (400 of 20,000)

| Command | Median |
| --- | --- |
| `tallow delta-sync --mirror` | 0.120 s |
| `rsync -a --delete` | 0.152 s |

The destinations were then hash-verified: the mirror had correctly removed the 200 files
deleted at the source, reported as `in sync`.

## Verification: re-read both sides and compare (391 MB)

| Command | Median |
| --- | --- |
| `rsync -rc --dry-run` | 0.305 s |
| `tallow verify-transfer --verify hash` | 2.256 s |

rsync's checksum pass is roughly 7x faster here. The algorithms differ (`-c` uses MD5,
`verify-transfer --verify hash` uses BLAKE3), and BLAKE3 should be the faster of the two, so
this reads as per-file overhead on a many-small-file tree rather than an algorithm
difference. Recorded as measured; not explained away.

**After the fix on `main` (not in `v0.1.0`).** `audit_trees` pinned its thread count to one, so
every file was hashed on a single thread. With the count derived the same pair now measures
**1.456 s** (median of three), against `rsync -rc --dry-run` at **0.381 s** re-measured the same
evening - the gap narrows from ~7.4x to ~3.8x. The released `v0.1.0` still carries the serial
version; the site and README tables quote that released pair.


## Correctness

Speed figures here are for copies that were verified, not assumed:

- all six destinations report `result: in sync` under `verify-transfer --verify hash`
- all four 1 GiB blobs are byte-identical under `cmp`
- the incremental destinations were verified after the mutations

## Clone-free byte copy of 2 GiB (both tools to tmpfs)

No reflink is possible on tmpfs and both tools write real bytes into an identical target, so this
is the clone-free head-to-head. The target is RAM-backed, so these figures measure per-byte
overhead rather than disk speed. Every destination was compared byte-for-byte afterwards.

| Command | Median | Throughput |
| --- | --- | --- |
| `tallow delta-sync -j 4` | 4.226 s | **485 MB/s** |
| `tallow copy` | 4.949 s | **414 MB/s** |
| `rsync -a` | 8.698 s | 235 MB/s |
| `cp -a --reflink=never` (reference) | 7.671 s | 267 MB/s |

Single-threaded, the engine is **1.76x** rsync here; at four threads, **2.06x**. All four
destinations were byte-identical to the source (`cmp`). This is the row that answers "how fast
is it when it cannot clone" — the one the earlier version of this document left open.

## Not measured

- **Robocopy** — Windows-only. Running it under Wine would measure Wine.
- **PowerShell `Copy-Item`** — same reason.
- **Byte throughput of a 4 GiB disk-to-disk copy** — every disk target available on the measuring
  machine supports cloning, so that exact shape could not be isolated. The clone-free path was
  instead measured on tmpfs at 2 GiB, above.
- **Cold cache** — dropping the page cache requires root, which the measuring session did
  not have. Every row is warm-source.
