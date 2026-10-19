# Tallow Copy — implementation plan

**Version 2 · 2026-10-19.** Supersedes the plan derived from `1000X-INTELLIGENCE.md`.
Version 1 described work that has since been built; this version starts from what exists,
separates *measured* from *assumed*, and plans only what remains.

---

## 1. Status today

| | |
|---|---|
| Canonical tree | `/home/aamir/tallow-copy` |
| Version | `0.1.4`, commit `73bdf05`, branch `main` |
| Tests | **120** (104 unit · 15 policy · 1 smoke) |
| Engine modules | `lib`, `bundle`, `journal`, `transport`, `policy`, `chunk`, `delta`, `stream`, `net_speed` |
| Dependencies | `blake3`, `filetime`, `libc` |
| Published | `github.com/aamirmanisa/tallow-copy` — v0.1.0 … v0.1.4, releases with notes |
| Toolchain | pinned in-repo: `rust-toolchain.toml` → `1.98.1` |

Superseded and not to be developed: `/home/aamir/Nixe/crates/tallow-copy-engine`
(2,172-line `lib.rs`, 61 tests, branch with no upstream).

---

## 2. What is proven, and what is not

Every row is either measured or marked unrun. No row is inferred.

| Claim | Target | Status | Evidence |
|---|---|---|---|
| Small-file packing rate | ≥ 50k files/s | ✅ **51,271 files/s** | 100k × 1 KiB, 100,000/100,000 byte-identical |
| Byte-bound pack rate | ≥ 500 MB/s | ✅ **556 MB/s** | 400 × 256 KiB, single pass with hash back-patching |
| Rescan skips hashing | 0 files hashed | ✅ | 200k tree, clean pass |
| Rescan wall clock, 1M | ≤ 5 s | ⚠️ **PROJECTED** 4.84 s | extrapolated from 200k = 0.97 s; never measured at 1M |
| Multi-stream scaling | beats 1 stream | ✅ 0.78 → **0.99 GB/s** | 512 MB loopback, 8 streams, byte-identical |
| Shaped-link efficiency | ≥ 90 % of 50 Mbps | ❌ **UNRUN** | `tc netem` test never executed |
| 1M small files over WAN | ≥ 100× rsync | ❌ **UNRUN** | the headline claim has no measurement |
| Peak RSS, 10 GB tree | < 64 MB | ❌ **UNMEASURED** | streaming design, no instrumented run |
| Local copy vs rsync | parity | ❌ **0.87–0.92×** | measured; we are slower, and say so |

---

## 3. Architecture as built

```
control plane (small, auditable)              data plane (dumb, fast)
┌───────────────────────────────┐   ┌──────────────────────────────────────┐
│ policy: allowed roots,        │   │ bundle: packed entries + payload,    │
│ @legal annotation, refusals   │   │ no per-file round-trips              │
└───────────────────────────────┘   └──────────────────────────────────────┘
             │                                      │
      journal (trusted metadata)           N parallel TCP streams
```

**Bundle (`TCB1`)** — header `magic|version|flags|count|total`, per-entry
`path_len|path|mode|mtime_ns|size|hash`. Single pass over the data: hashes are written into
reserved header space rather than re-reading bytes. Unpack verifies each file's BLAKE3 and
renames atomically, so a corrupted bundle fails at the offending file.

**Journal (v2)** — trusts `(size, mtime, ctime, inode, mode)`; entries whose `ctime` falls inside
a 10 ms racy window are re-hashed. `inode`+`ctime` defeat a `touch -r` mtime-restoring edit.

**Transport (`TC01`)** — 32-byte header `magic|offset|length|total`, receiver preallocates and
writes at offsets, so streams cannot collide and a dropped connection retries alone. Stream count
is clamped to byte count.

**Policy** — destination allowlist + `@legal` gate, evaluated before any byte is read. An empty
allowlist is a closed door; `..` traversal and sibling-prefix confusion (`/root/foo-evil`) are
refused.

---

## 4. Delivered phases

| Phase | Gate | Result |
|---|---|---|
| D0 Harness | reproduces rsync ±15 % | ✅ and it caught a **12×** regression (per-file `/proc/mounts`) before any feature work |
| D1 Bundle | byte-identical, ≥500 MB/s | ✅ with the 1 KiB caveat recorded |
| D2 Journal | `touch -r` detected | ✅ ctime+inode; 6.2× faster rescan |
| D3 Transport | 8 streams byte-identical | ✅ loopback only |
| D4 Policy | refusals typed | ✅ 7/7 |
| D4b Release hygiene | build from a clean clone | ✅ v0.1.4 pins the toolchain, tests root-safe |

**A premise died in D0.** The plan justified packing with *"rsync: 3+ RTT per file"*. Measured:
**1,000 × 4 KB over 139 ms took 1.56 s (~640 files/s)**. rsync streams new files without waiting
per file. The 1000× case must now beat *that*, not a folk model.

---

## 5. Remaining work

### P1 — Measurement closure (3 days) ← do this first

Nothing else matters until the headline claim has a number. Each item is a gate that can fail.

| Gate | Test | Pass condition |
|---|---|---|
| G1.1 | `tc netem` 50 Mbps / 40 ms / 0.5 % loss in a container, 512 MB, 8 streams | ≥ 90 % of shaped capacity, byte-identical |
| G1.2 | 1M-file rescan, warm page cache | ≤ 5 s **measured**, not projected |
| G1.3 | **1M × 1 KiB over a real 100 ms path vs rsync** | the verdict gate — either ≥ 100× or the claim is withdrawn |
| G1.4 | 10 GB pack under `/usr/bin/time -v` | peak RSS < 64 MB |
| G1.5 | `benchmarks/RESULTS.md` committed, README claims mapped to rows | every claim has a command and a sha |

**If G1.3 fails, the plan changes shape**: the product becomes "verified integrity transfer"
rather than "1000× faster", and the README is rewritten to match. Deciding this before the run
is what keeps the result honest.

### P2 — Integrity beyond per-file hashes (3 days)

Per-file BLAKE3 proves each file; it does not prove the *set*, and a failed transfer restarts.

- **Merkle root over chunk hashes** → `Transfer.tree_hash`, already declared in `transfer.tl`.
- **Signed receipt** — receiver returns the root hash, so the sender learns what actually landed.
  This matters on the CIFS mount, where mtimes are not durable.
- **Chunk-level resume** — a killed transfer resumes at the first unverified chunk.

Gate: flip one byte mid-bundle → detected at that chunk with a path; kill at 50 % → second run
transfers only the remainder, and the final tree hash matches.

### P3 — Privacy Tier 1 (3–5 days)

The journal is the highest-risk artifact: 900k paths, sizes, mtimes and inodes in plaintext, and
unsalted BLAKE3 acting as a content oracle.

- XChaCha20-Poly1305 journal, key from a file or OS keyring.
- Keyed BLAKE3 for journal digests; keep unkeyed hashes only where content-addressing is required.
- Zeroize keys on drop; never log them.

Gate: `journal.bin` reveals no path or filename under `strings`; wrong key fails cleanly; a
tampered ciphertext fails authentication rather than silently yielding entries.

### P4 — Make the Tallow surface honest (2 days)

`stdlib/transfer.tl` declares **121 functions** across Transfer, FileCopy, FileVault, DeltaSync,
Compress, ErasureCode, NetworkPath, Peer, Swarm, TransferSession and TransferStream — with no
implementations. That is an API advertising capability it does not have.

- Implement the subset the engine can back (tree hash, manifest, session, stream).
- Mark the rest `unimplemented` so a call fails with a typed error instead of returning a benign lie.
- FFI shim so policy decisions can be expressed from Tallow and logged.

Gate: no declared function returns success without doing work; every stub raises a typed error.

### P5 — Language consolidation (blocking, 1 day)

Five trees exist; three report `1.1.1` while differing by ~8k lines:

| Tree | Version | LOC | Modules |
|---|---|---|---|
| `/home/aamir/Nixe` | 1.1.1 | 253,061 | 86 |
| `/home/aamir/tallowsphere` | 1.1.1 | 261,019 | 85 *(missing `gama_api.tl`)* |
| `/home/aamir/tallow-extraction-engine` | 1.1.1 | 257,856 | 89 |
| `/home/aamir/nixe-merge-2026-10-17` | 1.1.1 | 253,061 | 89 *(not a repo)* |
| `/home/aamir/tallow-local` | 0.4.0 | 164,397 | 87 |

`github.com/aamirmanisa/Tallow-Language` is **private and empty** — zero commits — so these five
are the only copies that exist.

- Pick one canonical tree; record it in this document.
- Stamp artifacts `1.1.1+<commit>`, because the version string identifies nothing today.
- Either populate `Tallow-Language` or archive it; an empty repo named "Tallow-Language" invites
  the belief that the language is backed up.

Gate: `tallow --version` reports the commit; one tree is named canonical here.

---

## 6. Sequencing

| Phase | Days | Depends on | Unblocks |
|---|---|---|---|
| P1 Measurement | 3 | — | the product claim itself |
| P2 Integrity | 3 | P1 (baseline numbers) | receipts, resume |
| P3 Privacy | 3–5 | P2 (journal already keyed-frame) | safe retention |
| P4 Tallow surface | 2 | P2 | policy from the language |
| P5 Consolidation | 1 | — | any future language work |

Critical path **~10 days**; P3 and P4 run in parallel after P2.

---

## 7. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Packing bug corrupts data | catastrophic | per-file BLAKE3, temp+rename, round-trip fuzz on random trees |
| `mtime` lies (CIFS, `touch -r`) | silent loss | ctime+inode in the journal; mtime never trusted alone |
| Bundle fails and restarts | slow retries | chunk-level resume (P2) |
| Memory blow-up on huge trees | OOM | streaming reader/writer; entry table spills to disk above 1M |
| Extra streams hurt a saturated link | worse than baseline | adaptive count, drop to 1 unless >10 % gain |
| Claiming 1000× where it does not exist | credibility | P1 runs first and publishes loss rows |

---

## 8. What we will not claim

- No 1000× on bulk single-file WAN — physical link cap.
- No improvement on local disk: measured **0.87–0.92× rsync**.
- No 1000× over WiFi — the 2.4 GHz radio caps the link at ~7 MB/s.
- No end-to-end WAN advantage for the bundle until a reachable receiver exists.
- No Tallow-only data path: no syscall primitive, no memory model, no threading primitive, and
  every `native fn` is a hole in the privacy proof.
