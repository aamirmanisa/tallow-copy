//! Streaming transfer path — no chunking, no signatures, no hashing.
//!
//! Chunking exists only to compute a *delta*: it answers "which parts of this
//! file does the receiver already have?". When the receiver has nothing, that
//! question is pointless and its cost is pure loss. This module is the fast path
//! for exactly that case, and [`decide`] is the rule that picks between them.
//!
//! Two streaming implementations, because "stream" means different things:
//!
//! * **Local destination** — `fs::copy`, which on Linux uses `copy_file_range`
//!   and keeps the data in the kernel. No userspace buffer helps here; adding one
//!   is how a copy ends up slower than `cp`.
//! * **Network destination** (SMB/CIFS, NFS) — an explicit read/write loop with a
//!   buffer sized from the measured link ([`crate::net_speed`]) and optional
//!   pacing, because the kernel copy path does not know about a slow link.

use crate::net_speed;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Instant;

/// What the transfer did, measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamStats {
    pub bytes: u64,
    pub seconds: f64,
    pub buffer_bytes: usize,
    /// True when the destination was cloned instantly (btrfs/XFS reflink)
    /// instead of copied byte by byte.
    pub reflink: bool,
}

impl StreamStats {
    pub fn bytes_per_sec(&self) -> f64 {
        if self.seconds <= 0.0 {
            0.0
        } else {
            self.bytes as f64 / self.seconds
        }
    }
    pub fn gbps_decimal(&self) -> f64 {
        self.bytes_per_sec() / 1e9
    }
}

/// Which transfer strategy to use, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Send the whole file. No chunking.
    Stream { reason: &'static str },
    /// Compute a delta; only worth it when a usable base exists.
    Delta { reason: &'static str },
}

impl Strategy {
    pub fn label(&self) -> &'static str {
        match self {
            Strategy::Stream { .. } => "stream",
            Strategy::Delta { .. } => "delta",
        }
    }
    pub fn reason(&self) -> &'static str {
        match self {
            Strategy::Stream { reason } | Strategy::Delta { reason } => reason,
        }
    }
}

/// Decide between streaming and delta for one file.
///
/// The rules are deliberately conservative — delta must earn its cost:
///
/// * no destination file            -> Stream (there is nothing to reuse)
/// * size differs by more than 25%   -> Stream (little of the base can match)
/// * destination empty or tiny       -> Stream
/// * otherwise                       -> Delta
///
/// A caller that knows better can override by calling `delta::plan` directly.
pub fn decide(src: &Path, dst: &Path) -> Strategy {
    let src_meta = match fs::metadata(src) {
        Ok(m) => m,
        Err(_) => {
            return Strategy::Stream {
                reason: "source unreadable; attempt a plain stream",
            }
        }
    };
    let dst_meta = match fs::metadata(dst) {
        Ok(m) => m,
        Err(_) => {
            return Strategy::Stream {
                reason: "no destination file: nothing to reuse",
            }
        }
    };
    if !dst_meta.is_file() {
        return Strategy::Stream {
            reason: "destination is not a regular file",
        };
    }
    let s = src_meta.len();
    let d = dst_meta.len();
    if d < 4096 {
        return Strategy::Stream {
            reason: "destination too small for delta to pay off",
        };
    }
    if s == d {
        return Strategy::Delta {
            reason: "same size: in-place edits are likely",
        };
    }
    let (lo, hi) = if s < d { (s, d) } else { (d, s) };
    if (hi - lo) as f64 / hi as f64 > 0.25 {
        return Strategy::Stream {
            reason: "size differs >25%: base matches too little",
        };
    }
    Strategy::Delta {
        reason: "size within 25%: delta likely to win",
    }
}

/// Stream `src` into `dst`, choosing the right mechanism for the destination.
pub fn copy(src: &Path, dst: &Path) -> std::io::Result<StreamStats> {
    let policy = net_speed::policy_for_path(dst);
    let local = !net_speed::classify_path(dst).is_network();

    if local {
        let t = Instant::now();
        // Zero-copy attempt first: on btrfs/XFS `FICLONE` shares extents and
        // returns immediately regardless of file size. On ext4 it fails with
        // EOPNOTSUPP and we fall through to the kernel copy path.
        match try_reflink(src, dst) {
            Ok(bytes) => {
                return Ok(StreamStats {
                    bytes,
                    seconds: t.elapsed().as_secs_f64(),
                    buffer_bytes: 0,
                    reflink: true,
                })
            }
            Err(_) => {
                let bytes = fs::copy(src, dst)?;
                return Ok(StreamStats {
                    bytes,
                    seconds: t.elapsed().as_secs_f64(),
                    buffer_bytes: 0,
                    reflink: false,
                });
            }
        }
    }

    let buf_size = policy.buffer_size_bytes.max(64 * 1024);
    let t = Instant::now();
    let mut input = File::open(src)?;
    let mut output = File::create(dst)?;
    let mut buf = vec![0u8; buf_size];
    let mut bytes = 0u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n])?;
        bytes += n as u64;
        if !policy.pace.is_zero() {
            std::thread::sleep(policy.pace);
        }
    }
    output.flush()?;
    Ok(StreamStats {
        bytes,
        seconds: t.elapsed().as_secs_f64(),
        buffer_bytes: buf_size,
        reflink: false,
    })
}

/// Clone a file with `FICLONE` (Linux). Fails cleanly where the filesystem does
/// not support reflinks, which is the caller's cue to do a real copy.
#[cfg(target_os = "linux")]
fn try_reflink(src: &Path, dst: &Path) -> std::io::Result<u64> {
    use std::os::unix::io::AsRawFd;
    // FICLONE = _IOW(0x94, 9, int)
    const FICLONE: u64 = 0x4004_9409;
    unsafe extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }
    let s = File::open(src)?;
    let d = File::create(dst)?;
    let rc = unsafe { ioctl(d.as_raw_fd(), FICLONE, s.as_raw_fd()) };
    if rc == 0 {
        Ok(s.metadata()?.len())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn try_reflink(_src: &Path, _dst: &Path) -> std::io::Result<u64> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "reflink is Linux-only",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tc-stream-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::create_dir_all(&p);
        p
    }

    #[test]
    fn stream_copies_bytes_exactly() {
        let d = tmp();
        let src = d.join("a.bin");
        let dst = d.join("b.bin");
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &payload).unwrap();

        let st = copy(&src, &dst).unwrap();
        assert_eq!(st.bytes, payload.len() as u64);
        assert_eq!(fs::read(&dst).unwrap(), payload);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_destination_streams() {
        let d = tmp();
        let src = d.join("a.bin");
        fs::write(&src, b"hello").unwrap();
        let s = decide(&src, &d.join("nope.bin"));
        assert_eq!(s.label(), "stream");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn same_size_prefers_delta() {
        let d = tmp();
        let src = d.join("a.bin");
        let dst = d.join("b.bin");
        fs::write(&src, vec![1u8; 100_000]).unwrap();
        fs::write(&dst, vec![2u8; 100_000]).unwrap();
        assert_eq!(decide(&src, &dst).label(), "delta");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn very_different_size_streams() {
        let d = tmp();
        let src = d.join("a.bin");
        let dst = d.join("b.bin");
        fs::write(&src, vec![1u8; 1_000_000]).unwrap();
        fs::write(&dst, vec![2u8; 100_000]).unwrap();
        assert_eq!(decide(&src, &dst).label(), "stream");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn tiny_destination_streams() {
        let d = tmp();
        let src = d.join("a.bin");
        let dst = d.join("b.bin");
        fs::write(&src, vec![1u8; 4000]).unwrap();
        fs::write(&dst, vec![2u8; 10]).unwrap();
        assert_eq!(decide(&src, &dst).label(), "stream");
        let _ = fs::remove_dir_all(&d);
    }
}
