# Tallow Copy Engine Benchmark Protocol

This protocol measures the native Tallow copy engine against Robocopy,
PowerShell `Copy-Item`, the current Tallow CLI, and optional FastCopy installs.
It separates raw full-copy throughput from the workflows where Tallow can make
large wins: no-op sync, incremental sync, sparse/clone paths, restart, and
verified copy.

**Platform twin (added 2026-10-08).** Everything below was written for Windows: the harnesses are
PowerShell and the baselines are Windows tools. The operational targets for this project are a
Linux workstation, an SMB/CIFS NAS and a remote host over SSH, where none of those baselines run
and none of those harnesses have ever been executed. The POSIX block under "Baselines" is the part
that has actually been measured here; until a Windows run is recorded, every Windows-only gate in
this document is **unverified**, not satisfied.

**POSIX baseline executed 2026-10-09.** The rsync baseline in this document has now been run on the Linux workstation. The numbers, the reflink control that explains the large-file rows, and the correctness check are in [`results-2026-10-09-linux-btrfs.md`](results-2026-10-09-linux-btrfs.md). Robocopy and PowerShell `Copy-Item` remain **unverified**: they are Windows-only and no Windows run has been recorded.

## Benchmark Classes

Run each class as its own result group. Do not combine full-copy throughput with
skip-heavy or delta-heavy results.

| Class | Purpose |
| --- | --- |
| Full large-file copy | Measures hardware-bound sequential read/write speed. |
| Many-small-file copy | Measures traversal, scheduling, directory creation, and metadata overhead. |
| Mixed tree copy | Measures normal project/media/archive trees. |
| Warm no-op sync | Measures unchanged-file detection and planning overhead. |
| One-percent changed files | Measures incremental tree update efficiency. |
| One-percent changed bytes | Measures block/delta capability inside large files. |
| Sparse file copy | Measures sparse preservation or clone/range acceleration. |
| Verify-only | Measures destination re-read and hash/manifest speed. |
| Resume after interruption | Measures journal correctness and avoided recopy. |

## Datasets

Generate deterministic datasets outside the repository, for example:
`C:\tmp\tallow-copy-bench`.

The minimal smoke harness is intentionally small and safe by default:

```powershell
.\scripts\generate_copy_benchmark_dataset.ps1
.\scripts\benchmark_copy_engine.ps1
```

The generator writes a marker file and refuses to write into a non-empty
unmarked directory. Its default root is `C:\tmp\tallow-copy-bench`; use `-Root`
for isolated smoke runs and `-Clean` to remove only known generated children
inside a marked dataset root. The benchmark runner creates a unique
`bench-runs\<timestamp>` directory, never mirrors into source paths, and emits
both JSON and Markdown reports under `reports\` unless `-OutputRoot` is set.

Small CI/developer smoke example:

```powershell
.\scripts\generate_copy_benchmark_dataset.ps1 `
  -Root C:\tmp\tallow-copy-bench-smoke `
  -LargeFileBytes 4096 `
  -DeltaFileBytes 4096 `
  -SmallFileCount 4 `
  -MixedFileCount 4 `
  -Clean

.\scripts\benchmark_copy_engine.ps1 `
  -DatasetRoot C:\tmp\tallow-copy-bench-smoke `
  -Runs 1 `
  -Threads 2
```

Minimum smoke dataset:

- one 1 GiB file;
- 10,000 files at 1-16 KiB;
- one mixed tree with nested directories and varied file sizes;
- one sparse file where the filesystem supports sparse allocation;
- one unchanged destination for warm no-op sync;
- one destination with 1 percent changed files;
- one large file with 1 percent changed bytes.

Full dataset:

- one large file at 1 GiB, 8 GiB, and optional 32 GiB;
- 10,000 and 100,000 small files;
- mixed trees with depth 2-8 and varied file sizes;
- symlink or junction tree;
- sparse file;
- same-volume, cross-volume, and SMB/NAS runs where available.

## Baselines

Windows baseline commands:

```powershell
robocopy <src> <dst> /E /MT:1
robocopy <src> <dst> /E /MT:8
robocopy <src> <dst> /MIR /MT:16
Copy-Item -Recurse <src> <dst>
.\target\release\tallow.exe delta-sync <src> <dst> --threads 8
```

The smoke benchmark script currently exercises:

- `tallow copy <src> <dst>` for mixed-tree full copy;
- `tallow delta-sync <src> <dst> --threads <n> --quiet` for full sync;
- `tallow delta-sync <src> <preseeded-dst> --threads <n> --quiet` for warm
  no-op sync;
- `robocopy <src> <dst> /E /MT:<n>` when `robocopy.exe` is available and
  `-SkipRobocopy` is not set.

Optional baseline:

```powershell
fastcopy.exe /cmd=diff /auto_close <src> /to=<dst>
```

POSIX baseline commands - the ones that run on the hardware this project actually uses:

```sh
cp -a <src> <dst>                         # plain tree copy
rsync -a --delete <src>/ <dst>/           # incremental tree sync
tar -C <src> -cf - . | tar -C <dst> -xf - # archive pipeline (the shape used for bulk NAS moves)
/usr/bin/time -v <cmd>                    # wall clock plus peak RSS
```

Measured reference points (2026-10-08; recorded so a new measurement can be judged plausible
without re-running the whole matrix):

- SMB/CIFS NAS over Wi-Fi, 1040 Mbit/s PHY: write 43.6 MB/s, read 50.9-55.3 MB/s; 1/2/3/4
  concurrent streams 47.2 / 45.2 / 35.1 / 38.4 MB/s - i.e. **no parallel scaling**, and four
  streams is slower than one; small files 7-11 ms each; 1000 x 64 KiB bundled into a tar archive
  2.1x faster than the same bytes file-by-file.
- WAN, a remote host over SSH forced-command pull, RTT 133.71 ms: 13.27 MB/s single stream;
  25.20 MB/s at two streams (1.90x); 25.16 MB/s at three (ceiling ~201 Mbit/s, reached with two
  lanes). Per-stream share barely drops at two lanes, then collapses at three.

These are reference points, not gates. They exist so a surprising result stands out.

Tallow engine commands should record the exact binary, profile, commit, and
policy used. The `max` profile is the competitive benchmark profile.

## Metrics

Record these values for each measured run:

- scan seconds;
- plan seconds;
- copy seconds;
- verify seconds;
- total seconds;
- logical source bytes;
- physical bytes written when available;
- bytes per second;
- files per second;
- skipped files;
- copied files;
- deleted files;
- verification failures;
- retry count;
- peak RSS;
- CPU percent;
- storage device and filesystem;
- source/destination path class: same volume, cross volume, SMB/NAS, or other.

Use at least five measured runs after one warm-up run. Report median and p95.

Since 2026-10-08 `CopyReport` carries `CopyTimings` (`plan_ms`, `copy_ms`, `verify_ms`), so scan,
copy and verify figures no longer need external instrumentation. Two honest qualifications:
scanning and planning are ONE phase in this engine (`plan_ms`) because the plan is built during
the walk, and `verify_ms` is a **subset** of `copy_ms` rather than an addition to it. Physical
bytes written and peak RSS still require caller-side sampling.

Run the in-repo smoke with `cargo test -p tallow-copy-engine --test perf_copy_engine -- --ignored --nocapture`.

## Acceptance Gates

Two preconditions apply to the gates below, both measured rather than assumed:

- **Change detection must be named.** The no-op gates ("warm no-op sync at least 100x", the
  manifest stretch gate) presuppose that unchanged files can be *recognised*. On the SMB/CIFS
  share used here they cannot be recognised by mtime: the server re-stamps mtime within seconds of
  a write, so a destination-mtime comparison never matches and no mtime-based no-op ever fires.
  A run of these gates must state its mechanism - source-keyed manifest, explicit change list, or
  preserved timestamps via `MetadataPolicy::Timestamps` - or the number is meaningless. The same
  effect means the engine's own warm pass only skips when timestamps were preserved.
- **Every gate carries a path class.** Measured on this hardware, SMB/CIFS shows no parallel
  scaling (47.2 -> 38.4 MB/s from 1 to 4 streams) while WAN SSH gets 1.90x from exactly two streams
  and nothing from three. A single global thread count is wrong for both classes, and a gate quoted
  without its path class cannot be compared to another run.

Initial gates:

- full large-file copy is within 10 percent of the fastest baseline on the same
  hardware when verification is disabled;
- many-small-file copy beats `robocopy /MT:8` on median for the smoke dataset;
- warm no-op sync is at least 100x faster than a full recopy;
- one-percent changed tree is at least 10x faster than full copy;
- resume does not recopy committed files;
- full-hash verification re-reads destination bytes and is reported separately
  from copy speed.

Stretch gates:

- sparse/reflink/copy-range cases can exceed 1000x against tools that physically
  copy bytes;
- no-op sync can exceed 1000x on large unchanged trees with a manifest cache;
- block-delta sync can exceed 1000x when the changed byte range is tiny compared
  to the file.

## Reporting Rules

Never advertise "1000x faster copy" as a universal raw-throughput result.
Hardware-bound full byte-for-byte copy cannot exceed the slowest source read,
destination write, network, filesystem metadata, and verification limit.

Report each class separately and include the policy that made it fast, such as
skip cache, manifest, sparse preservation, clone/range copy, or block delta.

## Rust Perf Smoke

The smoke generates its own tiny dataset (one 32 MiB file plus eight 4 KiB files), plans and
copies it, and re-plans a warm pass. It asserts that the report carries phase timings, that a warm
pass with no verification request plans only skips, and that a warm pass *with* verification
requested upgrades every unchanged file from Skip to Verify rather than skipping it silently.

```sh
cargo test -p tallow-copy-engine --test perf_copy_engine -- --ignored --nocapture
```

It is `#[ignore]`d because it writes ~32 MiB, and it needs `--nocapture` to print the metrics
block. Before 2026-10-08 this section named a test target that did not exist in the repository; the
command above is the one that runs.
