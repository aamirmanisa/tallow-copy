//! Multi-stream transport — the WAN lever.
//!
//! A single TCP stream on a high-latency path is bounded by its congestion
//! window: on this network 6.4 MB/s at 139 ms RTT. Splitting one file across N
//! TCP connections gives N independent congestion windows and lets the
//! aggregate track the real link capacity.
//!
//! Measured caveat, stated up front: on a link that is already saturated, more
//! streams buy almost nothing (8 streams over the current 66 Mbps path gave
//! +16%). Parallelism is what turns a *fast* link into a fast transfer; it
//! cannot manufacture bandwidth that is not there.
//!
//! Wire format per stream (32-byte header, no framing after it):
//!
//! ```text
//! magic  "TC01"   4 bytes
//! offset u64 LE   8 bytes   file offset this stream writes at
//! length u64 LE   8 bytes   bytes carried by this stream
//! total  u64 LE   8 bytes   full file size, so the receiver preallocates
//! reserved        4 bytes
//! ```
//!
//! The receiver only ever writes at `offset`, so streams cannot collide, and no
//! stream depends on any other — a dropped connection can be retried alone.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

const MAGIC: &[u8; 4] = b"TC01";
const HEADER: usize = 32;
pub const DEFAULT_STREAMS: usize = 8;
/// 4 MiB per stream write: big enough to amortise syscalls, small enough that
/// one slow stream does not stall the aggregate for long.
pub const STREAM_BUF: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct SendStats {
    pub bytes: u64,
    pub seconds: f64,
    pub streams: usize,
}

impl SendStats {
    pub fn mb_per_sec(&self) -> f64 {
        if self.seconds <= 0.0 {
            0.0
        } else {
            self.bytes as f64 / 1e6 / self.seconds
        }
    }
}

fn header(offset: u64, len: u64, total: u64) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[0..4].copy_from_slice(MAGIC);
    h[4..12].copy_from_slice(&offset.to_le_bytes());
    h[12..20].copy_from_slice(&len.to_le_bytes());
    h[20..28].copy_from_slice(&total.to_le_bytes());
    h
}

fn parse_header(h: &[u8; HEADER]) -> std::io::Result<(u64, u64, u64)> {
    if &h[0..4] != MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad magic: not a tallow-copy stream",
        ));
    }
    let offset = u64::from_le_bytes(h[4..12].try_into().unwrap());
    let len = u64::from_le_bytes(h[12..20].try_into().unwrap());
    let total = u64::from_le_bytes(h[20..28].try_into().unwrap());
    Ok((offset, len, total))
}

/// Receive `streams` connections and write them into `out`. Blocks until the
/// expected number of connections has been served. Returns bytes written.
pub fn serve(listener: &TcpListener, out: &Path, streams: usize) -> std::io::Result<u64> {
    let file = Arc::new(std::sync::Mutex::new(
        OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(out)?,
    ));
    let written = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(streams);

    for _ in 0..streams {
        let (sock, _peer) = listener.accept()?;
        let file = Arc::clone(&file);
        let written = Arc::clone(&written);
        handles.push(std::thread::spawn(move || -> std::io::Result<()> {
            let mut sock = sock;
            let mut h = [0u8; HEADER];
            sock.read_exact(&mut h)?;
            let (offset, len, total) = parse_header(&h)?;
            {
                let f = file.lock().unwrap();
                if f.metadata()?.len() < total {
                    f.set_len(total)?; // preallocate so offsets are valid
                }
            }
            let mut buf = vec![0u8; STREAM_BUF];
            let mut done = 0u64;
            while done < len {
                let want = STREAM_BUF.min((len - done) as usize);
                sock.read_exact(&mut buf[..want])?;
                let mut f = file.lock().unwrap();
                f.seek(SeekFrom::Start(offset + done))?;
                f.write_all(&buf[..want])?;
                drop(f);
                done += want as u64;
                written.fetch_add(want as u64, Ordering::Relaxed);
            }
            // Acknowledge what this stream actually persisted. The sender waits
            // for this: without it, "sent" only means the bytes reached the local
            // socket buffer, so a peer that dies mid-transfer still looks like a
            // success. That is how a 5 MB send over a 139 ms path once reported
            // 2,685 MB/s while the receiver held nothing.
            sock.write_all(&done.to_le_bytes())?;
            sock.flush()?;
            Ok(())
        }));
    }
    for h in handles {
        h.join()
            .map_err(|_| std::io::Error::other("stream thread panicked"))??;
    }
    Ok(written.load(Ordering::Relaxed))
}

/// Split `src` across `streams` connections to `peer` and send concurrently.
pub fn send(peer: &str, src: &Path, streams: usize) -> std::io::Result<SendStats> {
    let meta = std::fs::metadata(src)?;
    let total = meta.len();
    if streams == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "streams must be > 0",
        ));
    }
    // Never open more connections than there are bytes to carry: a 4-byte file
    // split 8 ways would otherwise open four streams that send nothing and make
    // the receiver wait for connections that will never come. The caller must
    // use the same effective count when it starts `serve`.
    let streams = (streams as u64).min(total.max(1)) as usize;
    let per = total.div_ceil(streams as u64);
    let t = Instant::now();

    let mut handles = Vec::with_capacity(streams);
    for i in 0..streams {
        let offset = i as u64 * per;
        if offset >= total {
            break;
        }
        let len = per.min(total - offset);
        let path = src.to_path_buf();
        let peer = peer.to_string();
        handles.push(std::thread::spawn(move || -> std::io::Result<u64> {
            let mut f = std::fs::File::open(&path)?;
            f.seek(SeekFrom::Start(offset))?;
            let mut sock = TcpStream::connect(&peer)?;
            sock.set_nodelay(true)?;
            sock.write_all(&header(offset, len, total))?;
            let mut buf = vec![0u8; STREAM_BUF];
            let mut left = len;
            while left > 0 {
                let want = STREAM_BUF.min(left as usize);
                f.read_exact(&mut buf[..want])?;
                sock.write_all(&buf[..want])?;
                left -= want as u64;
            }
            sock.flush()?;
            // Wait for the receiver to confirm this range landed. Timings taken
            // before the ack measure the local kernel buffer, not the network.
            let mut ack = [0u8; 8];
            sock.read_exact(&mut ack)?;
            let acked = u64::from_le_bytes(ack);
            if acked != len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("receiver acknowledged {acked} of {len} bytes"),
                ));
            }
            Ok(len)
        }));
    }

    let mut bytes = 0u64;
    let mut started = 0usize;
    for h in handles {
        bytes += h
            .join()
            .map_err(|_| std::io::Error::other("stream thread panicked"))??;
        started += 1;
    }
    Ok(SendStats {
        bytes,
        seconds: t.elapsed().as_secs_f64(),
        streams: started,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tc-transport-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    #[test]
    fn header_roundtrips() {
        let h = header(1234, 5678, 9999);
        assert_eq!(parse_header(&h).unwrap(), (1234, 5678, 9999));
    }

    #[test]
    fn rejects_foreign_protocol() {
        let mut h = [0u8; HEADER];
        h[0..4].copy_from_slice(b"HTTP");
        assert!(parse_header(&h).is_err());
    }

    #[test]
    fn multi_stream_roundtrip_is_byte_exact() {
        let payload: Vec<u8> = (0..3_000_017u32).map(|i| (i % 253) as u8).collect();
        let src = temp_file("src.bin");
        let dst = temp_file("dst.bin");
        std::fs::write(&src, &payload).unwrap();
        let _ = std::fs::remove_file(&dst);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let dst2 = dst.clone();
        let server = std::thread::spawn(move || serve(&listener, &dst2, 4).unwrap());

        let stats = send(&addr, &src, 4).unwrap();
        let written = server.join().unwrap();

        assert_eq!(stats.bytes, payload.len() as u64);
        assert_eq!(written, payload.len() as u64);
        assert_eq!(
            std::fs::read(&dst).unwrap(),
            payload,
            "reassembled file must be byte-identical"
        );
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn single_stream_roundtrip() {
        let payload = vec![0xABu8; 65537];
        let src = temp_file("s1.bin");
        let dst = temp_file("d1.bin");
        std::fs::write(&src, &payload).unwrap();
        let _ = std::fs::remove_file(&dst);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let dst2 = dst.clone();
        let server = std::thread::spawn(move || serve(&listener, &dst2, 1).unwrap());
        send(&addr, &src, 1).unwrap();
        server.join().unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), payload);
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn more_streams_than_bytes_still_works() {
        let src = temp_file("tiny.bin");
        std::fs::write(&src, b"tiny").unwrap();
        let dst = temp_file("tiny-out.bin");
        let _ = std::fs::remove_file(&dst);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let dst2 = dst.clone();
        // 4 bytes requested as 8 streams -> clamped to 4 effective streams.
        let server = std::thread::spawn(move || serve(&listener, &dst2, 4).unwrap());
        let st = send(&addr, &src, 8).unwrap();
        let written = server.join().unwrap();
        assert_eq!(
            st.streams, 4,
            "stream count must be clamped to the byte count"
        );
        assert_eq!(st.bytes, 4);
        assert_eq!(written, 4);
        assert_eq!(std::fs::read(&dst).unwrap(), b"tiny");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }
}
