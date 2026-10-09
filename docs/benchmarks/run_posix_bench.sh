#!/usr/bin/env bash
# POSIX baseline harness: Tallow Copy (engine) vs rsync vs cp.
#
#   TALLOW_BIN=./target/release/tallow ./run_posix_bench.sh [runs]
#
# The results this produces on a btrfs workstation are recorded in
# results-2026-10-09-linux-btrfs.md. Re-run it to reproduce or refute them.
#
# What it does NOT measure, and why:
#   * Robocopy and PowerShell Copy-Item - Windows-only. Under Wine you would measure Wine.
#   * Cold-cache rows - dropping the page cache needs root, so every row is warm-source.
#   * The engine's clone-free large-file path - on a copy-on-write filesystem the engine
#     clones, and no non-cloning destination big enough was available to isolate it.
#
# Read the clone rows with care: on CoW filesystems a "copy" is O(1) and is NOT a
# throughput figure. The tmpfs rows are the clone-free comparison.
set -u
T="${TALLOW_BIN:-./target/release/tallow}"
RUNS="${1:-3}"
B="${BENCH_DIR:-$PWD/tallow-copy-bench}"
TMP="${BENCH_TMP:-${TMPDIR:-/tmp}}"
OUT="$B/results.tsv"
[ -x "$T" ] || { echo "no tallow binary at $T (set TALLOW_BIN)"; exit 1; }
command -v rsync >/dev/null || { echo "rsync not found"; exit 1; }
mkdir -p "$B"; : > "$OUT"

now() { date +%s.%N; }
el()  { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.3f", b-a}'; }

# every row aborts the run on a nonzero exit, so a broken command cannot be recorded as fast
run_one() { # label dst cmd...
  local label="$1" dst="$2" mode="$3"; shift 3
  local i t0 t1 dt rc
  for i in $(seq 1 "$RUNS"); do
    case "$mode" in
      clean)  rm -rf "$dst"; sync ;;
      reseed) rm -rf "$dst"; cp -a "$SEED" "$dst"; sync ;;
      inplace) : ;;
    esac
    t0=$(now); "$@" >/dev/null 2>&1; rc=$?; t1=$(now); sync
    dt=$(el "$t0" "$t1")
    [ "$rc" -ne 0 ] && { echo "FAILED(rc=$rc) $label -> aborting, this row is not a measurement"; exit 1; }
    printf '%-34s run%s %8ss\n' "$label" "$i" "$dt" | tee -a "$OUT"
  done
}

echo "### datasets (override with BENCH_DIR)"
[ -d "$B/src_big" ]   || { mkdir -p "$B/src_big";   (cd "$B/src_big"   && for i in 1 2 3 4; do dd if=/dev/urandom of=blob$i.bin bs=1M count=1024 status=none & done; wait); }
[ -d "$B/src_small" ] || { mkdir -p "$B/src_small"; (cd "$B/src_small" && seq 1 20000 | xargs -P 4 -I{} sh -c 'head -c 20480 /dev/urandom > f{}.dat'); }
echo "  big: $(du -sh "$B/src_big" | cut -f1)  small: $(du -sh "$B/src_small" | cut -f1)"

echo; echo "### smoke: every surface must exit 0 before any timing counts"
rm -rf "$B"/smoke_*  "$B/src_smoke"; mkdir -p "$B/src_smoke"
for i in 1 2 3 4 5; do head -c 100000 /dev/urandom > "$B/src_smoke/s$i.bin"; done
smoke() { local l="$1"; shift; if out=$("$@" 2>&1); then echo "  ok: $l"; else echo "  FAILED: $l"; echo "$out" | tail -6; exit 1; fi; }
smoke "copy (tree)"            "$T" copy "$B/src_smoke" "$B/smoke_c"
smoke "delta-sync"             "$T" delta-sync "$B/src_smoke" "$B/smoke_d"
smoke "verify-transfer (size)" "$T" verify-transfer --source "$B/src_smoke" --target "$B/smoke_c" --verify size
smoke "verify-transfer (hash)" "$T" verify-transfer --source "$B/src_smoke" --target "$B/smoke_d" --verify hash
rm -rf "$B"/smoke_*

echo; echo "### 4 GiB large files (clone rows - NOT throughput)"
run_one "cp -a"                       "$B/d_big_cp" clean  cp -a "$B/src_big" "$B/d_big_cp"
run_one "cp -a --reflink=never"       "$B/d_big_no" clean  cp -a --reflink=never "$B/src_big" "$B/d_big_no"
run_one "rsync -a"                    "$B/d_big_rs" clean  rsync -a "$B/src_big/" "$B/d_big_rs/"
run_one "tallow copy"                 "$B/d_big_t"  clean  "$T" copy "$B/src_big" "$B/d_big_t"
run_one "tallow delta-sync -j 4"      "$B/d_big_d4" clean  "$T" delta-sync -j 4 "$B/src_big" "$B/d_big_d4"

echo; echo "### 20,000 small files, clone-free on tmpfs (the honest byte comparison)"
run_one "rsync -a (tmpfs)"            "$TMP/rs_bench" clean rsync -a "$B/src_small/" "$TMP/rs_bench/"
run_one "tallow copy (tmpfs)"         "$TMP/tc_bench" clean "$T" copy "$B/src_small" "$TMP/tc_bench"
rm -rf "$TMP/rs_bench" "$TMP/tc_bench"

echo; echo "### small files on the local filesystem"
run_one "cp -a"                       "$B/d_sm_cp" clean cp -a "$B/src_small" "$B/d_sm_cp"
run_one "rsync -a"                    "$B/d_sm_rs" clean rsync -a "$B/src_small/" "$B/d_sm_rs/"
run_one "tallow copy"                 "$B/d_sm_t"  clean "$T" copy "$B/src_small" "$B/d_sm_t"

echo; echo "### warm no-op"
run_one "rsync -a (no-op)"            "$B/d_sm_rs" inplace rsync -a "$B/src_small/" "$B/d_sm_rs/"
run_one "tallow delta-sync (no-op)"   "$B/d_sm_t"  inplace "$T" delta-sync "$B/src_small" "$B/d_sm_t"

echo; echo "### incremental: 1% changed and 1% deleted (snapshot first, reseed per run)"
rm -rf "$B/src_pre"; cp -a "$B/src_small" "$B/src_pre"; sync; SEED="$B/src_pre"
n=$(( $(find "$B/src_small" -type f | wc -l) / 100 ))
find "$B/src_small" -type f | head -n "$n" | while read -r f; do printf 'changed' >> "$f"; done
find "$B/src_small" -type f | tail -n "$n" | while read -r f; do rm -f "$f"; done
sync
run_one "rsync -a --delete (1% chg+del)" "$B/d_inc_rs" reseed rsync -a --delete "$B/src_small/" "$B/d_inc_rs/"
run_one "tallow delta-sync --mirror"     "$B/d_inc_t"  reseed "$T" delta-sync --mirror "$B/src_small" "$B/d_inc_t"

echo; echo "### verification (hash both sides)"
run_one "rsync -rc --dry-run"         "$B/d_sm_rs" inplace rsync -rc --dry-run "$B/src_small/" "$B/d_sm_rs/"
run_one "tallow verify-transfer hash" "$B/d_sm_t"  inplace "$T" verify-transfer --source "$B/src_small" --target "$B/d_sm_t" --verify hash

echo; echo "### correctness (a fast wrong copy is worthless)"
for pair in "src_small d_sm_t" "src_small d_sm_rs" "src_small d_inc_t" "src_small d_inc_rs" "src_big d_big_t"; do
  set -- $pair
  "$T" verify-transfer --source "$B/$1" --target "$B/$2" --verify hash 2>&1 | tail -1 | sed "s|^|  $2: |"
done
echo; echo "RESULTS=$OUT"
