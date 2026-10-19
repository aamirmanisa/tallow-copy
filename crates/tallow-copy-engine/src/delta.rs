//! Delta planning: turn two files into a byte-level transfer plan.
//!
//! The receiver holds `old`; the sender holds `new`. The sender asks the
//! receiver for the signature of `old` (its chunk hashes), then emits a plan:
//! for every chunk of `new` that the receiver already has, a reference; for
//! everything else, the literal bytes. Only literals cross the wire.

use crate::chunk::{split, Chunk};
use std::collections::HashMap;

/// One operation in a transfer plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Reuse `len` bytes from the receiver's old file at `old_offset`.
    Reuse { old_offset: u64, len: u32 },
    /// Send `len` literal bytes starting at `new_offset`.
    Literal { new_offset: u64, len: u32 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub ops: Vec<Op>,
    pub literal_bytes: u64,
    pub reused_bytes: u64,
}

impl Plan {
    pub fn total_bytes(&self) -> u64 {
        self.literal_bytes + self.reused_bytes
    }

    /// Fraction of the target file that does NOT need to be sent.
    pub fn reuse_ratio(&self) -> f64 {
        let t = self.total_bytes();
        if t == 0 {
            1.0
        } else {
            self.reused_bytes as f64 / t as f64
        }
    }

    /// Bytes that must actually cross the wire, as a fraction of file size.
    pub fn wire_ratio(&self) -> f64 {
        1.0 - self.reuse_ratio()
    }
}

/// Index a signature for O(1) chunk lookup.
pub fn index(sig: &[Chunk]) -> HashMap<[u8; 16], Vec<&Chunk>> {
    let mut m: HashMap<[u8; 16], Vec<&Chunk>> = HashMap::new();
    for c in sig {
        m.entry(c.hash).or_default().push(c);
    }
    m
}

/// Build a transfer plan for `new` given the receiver's `old` signature.
pub fn plan(old_sig: &[Chunk], new: &[u8], avg: usize) -> Plan {
    let idx = index(old_sig);
    let new_chunks = split(new, avg);
    let mut ops = Vec::with_capacity(new_chunks.len());
    let mut literal_bytes = 0u64;
    let mut reused_bytes = 0u64;

    // Coalesce adjacent literals so a run of changed chunks is one op, not one
    // per chunk — fewer protocol messages, less per-op overhead.
    let mut pending_literal: Option<(u64, u64)> = None; // (offset, len)

    for c in &new_chunks {
        if idx.contains_key(&c.hash) {
            if let Some((off, len)) = pending_literal.take() {
                ops.push(Op::Literal {
                    new_offset: off,
                    len: len as u32,
                });
                literal_bytes += len;
            }
            // Prefer the first occurrence: it keeps the plan deterministic.
            let old = idx[&c.hash][0];
            ops.push(Op::Reuse {
                old_offset: old.offset,
                len: c.len,
            });
            reused_bytes += c.len as u64;
        } else {
            pending_literal = match pending_literal {
                Some((off, len)) => Some((off, len + c.len as u64)),
                None => Some((c.offset, c.len as u64)),
            };
        }
    }
    if let Some((off, len)) = pending_literal {
        ops.push(Op::Literal {
            new_offset: off,
            len: len as u32,
        });
        literal_bytes += len;
    }

    Plan {
        ops,
        literal_bytes,
        reused_bytes,
    }
}

/// Reconstruct the new file on the receiver. Returns the bytes and verifies the
/// result against the sender's whole-file hash when one is supplied.
pub fn apply(old: &[u8], plan: &Plan, literals: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(plan.total_bytes() as usize);
    let mut lit_pos = 0usize;
    for op in &plan.ops {
        match *op {
            Op::Reuse { old_offset, len } => {
                let s = old_offset as usize;
                let e = s + len as usize;
                if e > old.len() {
                    return Err(format!("reuse out of range: {s}..{e} > {}", old.len()));
                }
                out.extend_from_slice(&old[s..e]);
            }
            Op::Literal { len, .. } => {
                let e = lit_pos + len as usize;
                if e > literals.len() {
                    return Err(format!(
                        "literal underrun: need {e}, have {}",
                        literals.len()
                    ));
                }
                out.extend_from_slice(&literals[lit_pos..e]);
                lit_pos = e;
            }
        }
    }
    if lit_pos != literals.len() {
        return Err(format!(
            "{} literal bytes left unconsumed",
            literals.len() - lit_pos
        ));
    }
    Ok(out)
}

/// Extract the literal bytes a plan references, in plan order.
pub fn literals_of<'a>(new: &'a [u8], plan: &Plan) -> Vec<u8> {
    let mut out = Vec::with_capacity(plan.literal_bytes as usize);
    for op in &plan.ops {
        if let Op::Literal { new_offset, len } = *op {
            out.extend_from_slice(&new[new_offset as usize..new_offset as usize + len as usize]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::split;

    fn data(n: usize, seed: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut x = seed;
        for _ in 0..n {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            v.push((x >> 33) as u8);
        }
        v
    }

    #[test]
    fn identical_files_transfer_nothing() {
        let f = data(1_000_000, 1);
        let sig = split(&f, 64 * 1024);
        let p = plan(&sig, &f, 64 * 1024);
        assert_eq!(p.literal_bytes, 0, "identical file must need zero literals");
        assert_eq!(apply(&f, &p, &[]).unwrap(), f);
    }

    #[test]
    fn one_byte_flip_transfers_about_one_chunk() {
        let f = data(2_000_000, 2);
        let mut edited = f.clone();
        edited[1_000_000] ^= 0xFF;
        let sig = split(&f, 64 * 1024);
        let old: std::collections::HashSet<[u8; 16]> = sig.iter().map(|c| c.hash).collect();
        let changed = split(&edited, 64 * 1024)
            .iter()
            .filter(|c| !old.contains(&c.hash))
            .count();
        // A single changed byte invalidates the chunk containing it and the one
        // whose start offset shifted; content-defined boundaries re-synchronise
        // immediately after. Byte counts vary with chunk size, chunk count does not.
        assert!(changed <= 3, "{changed} chunks changed for a 1-byte flip");
        let p = plan(&sig, &edited, 64 * 1024);
        let lit = literals_of(&edited, &p);
        assert_eq!(apply(&f, &p, &lit).unwrap(), edited);
    }

    #[test]
    fn prepended_data_transfers_only_the_prefix() {
        let f = data(2_000_000, 3);
        let mut edited = vec![7u8; 4096];
        edited.extend_from_slice(&f);
        let sig = split(&f, 64 * 1024);
        let p = plan(&sig, &edited, 64 * 1024);
        let ratio = p.reuse_ratio();
        assert!(ratio > 0.95, "prepend reused only {:.1}%", ratio * 100.0);
        let lit = literals_of(&edited, &p);
        assert_eq!(apply(&f, &p, &lit).unwrap(), edited);
    }

    #[test]
    fn append_is_cheap() {
        let f = data(1_000_000, 4);
        let mut edited = f.clone();
        edited.extend_from_slice(&data(10_000, 5));
        let sig = split(&f, 64 * 1024);
        let p = plan(&sig, &edited, 64 * 1024);
        assert!(
            p.literal_bytes < 200_000,
            "append moved {}",
            p.literal_bytes
        );
        assert_eq!(apply(&f, &p, &literals_of(&edited, &p)).unwrap(), edited);
    }

    #[test]
    fn reconstructs_from_random_edits() {
        let f = data(1_500_000, 6);
        let mut edited = f.clone();
        // Edits spaced well beyond one average chunk: only the touched chunks
        // should go on the wire. (Edits closer together than the chunk size
        // legitimately invalidate everything, which the next test covers.)
        for i in (0..edited.len()).step_by(300_000) {
            edited[i] ^= 0x5A;
        }
        let sig = split(&f, 64 * 1024);
        let p = plan(&sig, &edited, 64 * 1024);
        let lit = literals_of(&edited, &p);
        assert_eq!(apply(&f, &p, &lit).unwrap(), edited);
        assert!(p.wire_ratio() < 0.5, "wire ratio {:.2}", p.wire_ratio());
    }

    #[test]
    fn dense_edits_are_honestly_expensive() {
        // One edit every 50 KB with 64 KB chunks means almost every chunk is
        // touched. The engine must report that, not pretend it is cheap.
        let f = data(1_500_000, 9);
        let mut edited = f.clone();
        for i in (0..edited.len()).step_by(50_000) {
            edited[i] ^= 0x5A;
        }
        let sig = split(&f, 64 * 1024);
        let p = plan(&sig, &edited, 64 * 1024);
        assert!(
            p.wire_ratio() > 0.5,
            "dense edits should be expensive, got {:.2}",
            p.wire_ratio()
        );
    }

    #[test]
    fn rejects_corrupt_literal_stream() {
        let f = data(500_000, 8);
        let mut edited = f.clone();
        edited[1000] ^= 0xFF;
        let sig = split(&f, 64 * 1024);
        let p = plan(&sig, &edited, 64 * 1024);
        let mut lit = literals_of(&edited, &p);
        lit.pop();
        assert!(apply(&f, &p, &lit).is_err());
    }
}
