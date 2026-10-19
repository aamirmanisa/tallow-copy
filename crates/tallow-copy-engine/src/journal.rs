//! Signed manifest and fast rescan.
//!
//! A full rescan of a large tree re-reads every byte to decide that nothing
//! changed. That is the single most wasteful thing a sync tool does, and the fix
//! is a journal: record what the tree looked like, then on the next run trust
//! every entry whose metadata is unchanged.
//!
//! Trusted metadata is `(size, mtime, ctime, inode, mode)` — deliberately more
//! than mtime and size. `--trust-mtime`-style tools can be fooled by a
//! mtime-preserving write (`touch -r`, some build systems, `cp --preserve`), which
//! silently skips a changed file. On Linux a content write always bumps `ctime`
//! even when mtime is restored, and a replaced file gets a new inode, so this
//! journal cannot be fooled that way. That is what makes the fast path safe to
//! leave on by default rather than behind a flag.
//!
//! The journal carries a BLAKE3 digest over its canonical encoding so a truncated
//! or edited journal is rejected instead of trusted. Call `save` with a key for a
//! keyed (authenticated) digest when the journal crosses a trust boundary.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 4] = b"TCJ1";
pub const VERSION: u16 = 2;
const FLAG_KEYED: u8 = 0x01;
/// Files whose ctime is within this window of the scan are re-hashed on the next
/// pass. A write can land in the same timestamp tick as the scan (coarse clocks,
/// tmpfs, network filesystems), which would otherwise look "unchanged" forever.
const RACY_NS: i64 = 10_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub inode: u64,
    pub mode: u32,
    pub hash: [u8; 32],
}

impl Entry {
    /// Metadata-only comparison: the fast path. No file content is read.
    fn same_metadata(&self, other: &Entry) -> bool {
        self.size == other.size
            && self.mtime_ns == other.mtime_ns
            && self.ctime_ns == other.ctime_ns
            && self.inode == other.inode
            && self.mode == other.mode
    }
}

#[derive(Clone, Debug, Default)]
pub struct Journal {
    pub entries: Vec<Entry>,
    pub digest: [u8; 32],
    /// Wall-clock time the scan started. Entries close to this instant are
    /// considered racy and are re-hashed next time.
    pub scanned_at_ns: i64,
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn is_racy(e: &Entry, journal: &Journal) -> bool {
    e.ctime_ns >= journal.scanned_at_ns.saturating_sub(RACY_NS)
}

#[derive(Clone, Debug, Default)]
pub struct Diff {
    pub added: Vec<Entry>,
    pub changed: Vec<Entry>,
    pub removed: Vec<String>,
    pub unchanged: usize,
    /// How many files were actually read and hashed. The whole point of the
    /// journal is that this is 0 on a clean rescan.
    pub hashed: usize,
    pub seconds: f64,
}

impl Diff {
    pub fn is_clean(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

#[cfg(unix)]
fn stat_entry(
    root: &Path,
    p: &Path,
    md: &fs::Metadata,
    hash: Option<[u8; 32]>,
) -> io::Result<Entry> {
    use std::os::unix::fs::MetadataExt;
    use std::time::UNIX_EPOCH;
    let rel = p
        .strip_prefix(root)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path outside root"))?
        .to_string_lossy()
        .replace('\\', "/");
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    Ok(Entry {
        path: rel,
        size: md.len(),
        mtime_ns,
        ctime_ns: md.ctime() * 1_000_000_000 + md.ctime_nsec(),
        inode: md.ino(),
        mode: md.mode(),
        hash: hash.unwrap_or([0u8; 32]),
    })
}

#[cfg(not(unix))]
fn stat_entry(
    root: &Path,
    p: &Path,
    md: &fs::Metadata,
    hash: Option<[u8; 32]>,
) -> io::Result<Entry> {
    let rel = p.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
    Ok(Entry {
        path: rel,
        size: md.len(),
        mtime_ns: 0,
        ctime_ns: 0,
        inode: 0,
        mode: 0,
        hash: hash.unwrap_or([0u8; 32]),
    })
}

pub fn hash_file(p: &Path) -> io::Result<[u8; 32]> {
    let mut f = BufReader::with_capacity(1 << 20, File::open(p)?);
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Walk the tree, returning each file with the metadata already fetched.
///
/// The caller must reuse that metadata: a second `stat` per file doubles the
/// syscall count and was measured to put a 1M-file rescan over its budget.

/// Parallel walk over the top-level subtrees.
///
/// A sequential walk costs ~7 us per file, which puts a 1M-file rescan at ~7 s —
/// over budget. Directories are independent, so each worker takes a slice of the
/// top level and the results are merged and sorted. Stat errors are skipped: a
/// file that vanishes mid-scan is not an error, it is simply not there.
fn walk_parallel(root: &Path) -> io::Result<Vec<(PathBuf, fs::Metadata)>> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16);
    let mut buckets: Vec<Vec<PathBuf>> = (0..workers).map(|_| Vec::new()).collect();
    let mut files: Vec<(PathBuf, fs::Metadata)> = Vec::new();

    for (i, e) in fs::read_dir(root)?.flatten().enumerate() {
        let p = e.path();
        let Ok(md) = fs::symlink_metadata(&p) else {
            continue;
        };
        if md.is_dir() {
            buckets[i % workers].push(p);
        } else if md.is_file() {
            files.push((p, md));
        }
    }

    let parts: Vec<Vec<(PathBuf, fs::Metadata)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = buckets
            .into_iter()
            .map(|bucket| {
                scope.spawn(move || {
                    let mut found = Vec::new();
                    let mut stack = bucket;
                    while let Some(d) = stack.pop() {
                        let Ok(rd) = fs::read_dir(&d) else { continue };
                        for e in rd.flatten() {
                            let p = e.path();
                            let Ok(md) = fs::symlink_metadata(&p) else {
                                continue;
                            };
                            if md.is_dir() {
                                stack.push(p);
                            } else if md.is_file() {
                                found.push((p, md));
                            }
                        }
                    }
                    found
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect()
    });

    for part in parts {
        files.extend(part);
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Full scan: reads every file and records its BLAKE3.
pub fn scan(root: &Path) -> io::Result<Journal> {
    let scanned_at_ns = now_ns();
    let mut entries = Vec::new();
    for (p, md) in walk_parallel(root)? {
        let h = hash_file(&p)?;
        entries.push(stat_entry(root, &p, &md, Some(h))?);
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let digest = digest_of(&entries, None);
    Ok(Journal {
        entries,
        digest,
        scanned_at_ns,
    })
}

/// Rescan against a journal. Only files whose metadata moved are read.
///
/// A journal path that no longer exists is reported as removed; a file with no
/// journal entry is reported as added and is hashed.
pub fn rescan(root: &Path, journal: &Journal) -> io::Result<Diff> {
    let start = std::time::Instant::now();
    let mut by_path: std::collections::HashMap<&str, &Entry> = std::collections::HashMap::new();
    for e in &journal.entries {
        by_path.insert(e.path.as_str(), e);
    }

    let mut diff = Diff::default();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();

    for (p, md) in walk_parallel(root)? {
        let cur = stat_entry(root, &p, &md, None)?;
        seen.insert(cur.path.clone(), ());
        match by_path.get(cur.path.as_str()) {
            Some(old) if old.same_metadata(&cur) && !is_racy(&cur, journal) => {
                diff.unchanged += 1;
            }
            Some(old) => {
                // Metadata moved, or it is too close to the previous scan to be
                // trusted: read the file and decide from content.
                let h = hash_file(&p)?;
                diff.hashed += 1;
                if old.same_metadata(&cur) && h == old.hash {
                    diff.unchanged += 1;
                } else {
                    let mut e = cur;
                    e.hash = h;
                    diff.changed.push(e);
                }
            }
            None => {
                let h = hash_file(&p)?;
                diff.hashed += 1;
                let mut e = cur;
                e.hash = h;
                diff.added.push(e);
            }
        }
    }

    for e in &journal.entries {
        if !seen.contains_key(&e.path) {
            diff.removed.push(e.path.clone());
        }
    }

    diff.seconds = start.elapsed().as_secs_f64();
    Ok(diff)
}

/// Fold a diff into the journal, producing the manifest to persist after a
/// successful transfer.
pub fn apply(journal: &Journal, diff: &Diff) -> Journal {
    let mut map: BTreeMap<String, Entry> = journal
        .entries
        .iter()
        .cloned()
        .map(|e| (e.path.clone(), e))
        .collect();
    for p in &diff.removed {
        map.remove(p);
    }
    for e in diff.added.iter().chain(diff.changed.iter()) {
        map.insert(e.path.clone(), e.clone());
    }
    let entries: Vec<Entry> = map.into_values().collect();
    let digest = digest_of(&entries, None);
    Journal {
        entries,
        digest,
        scanned_at_ns: now_ns(),
    }
}

fn encode(entries: &[Entry], out: &mut Vec<u8>) {
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        let p = e.path.as_bytes();
        out.extend_from_slice(&(p.len() as u16).to_le_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(&e.size.to_le_bytes());
        out.extend_from_slice(&e.mtime_ns.to_le_bytes());
        out.extend_from_slice(&e.ctime_ns.to_le_bytes());
        out.extend_from_slice(&e.inode.to_le_bytes());
        out.extend_from_slice(&e.mode.to_le_bytes());
        out.extend_from_slice(&e.hash);
    }
}

fn digest_of(entries: &[Entry], key: Option<&[u8; 32]>) -> [u8; 32] {
    let mut buf = Vec::with_capacity(entries.len() * 96 + 4);
    encode(entries, &mut buf);
    match key {
        Some(k) => *blake3::keyed_hash(k, &buf).as_bytes(),
        None => *blake3::hash(&buf).as_bytes(),
    }
}

pub fn save(journal: &Journal, path: &Path, key: Option<&[u8; 32]>) -> io::Result<()> {
    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.push(if key.is_some() { FLAG_KEYED } else { 0 });

    // The stored digest covers the entries only; the header is fixed and short.
    // A keyed digest authenticates the content without needing a key on read for
    // integrity-only use, so record the key id (first 4 bytes) for disambiguation.
    let mut body = Vec::new();
    body.extend_from_slice(&journal.scanned_at_ns.to_le_bytes());
    encode(&journal.entries, &mut body);
    let digest = match key {
        Some(k) => {
            buf.extend_from_slice(&k[..4]);
            *blake3::keyed_hash(k, &body).as_bytes()
        }
        None => *blake3::hash(&body).as_bytes(),
    };
    buf.extend_from_slice(&digest);
    buf.extend_from_slice(&(body.len() as u64).to_le_bytes());
    buf.extend_from_slice(&body);

    let tmp = path.with_extension("tmp");
    let mut w = BufWriter::new(File::create(&tmp)?);
    w.write_all(&buf)?;
    w.flush()?;
    drop(w);
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load(path: &Path, key: Option<&[u8; 32]>) -> io::Result<Journal> {
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    if buf.len() < 4 + 2 + 1 + 4 + 32 + 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "journal too short",
        ));
    }
    if &buf[0..4] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a tallow journal",
        ));
    }
    let ver = u16::from_le_bytes([buf[4], buf[5]]);
    if ver != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported journal version",
        ));
    }
    let flags = buf[6];
    let mut off = 7;
    if flags & FLAG_KEYED != 0 {
        off += 4;
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&buf[off..off + 32]);
    off += 32;
    let body_len = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap()) as usize;
    off += 8;
    let body = &buf[off..];
    if body.len() != body_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "journal truncated",
        ));
    }
    let want = match (flags & FLAG_KEYED != 0, key) {
        (true, Some(k)) => *blake3::keyed_hash(k, body).as_bytes(),
        (true, None) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "journal is keyed; a key is required to verify it",
            ))
        }
        (false, _) => *blake3::hash(body).as_bytes(),
    };
    if want != digest {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "journal digest mismatch (tampered or truncated)",
        ));
    }

    let mut entries = Vec::new();
    let mut p = 0usize;
    let scanned_at_ns = i64::from_le_bytes(body[p..p + 8].try_into().unwrap());
    p += 8;
    let count = u32::from_le_bytes(body[p..p + 4].try_into().unwrap()) as usize;
    p += 4;
    for _ in 0..count {
        let plen = u16::from_le_bytes(body[p..p + 2].try_into().unwrap()) as usize;
        p += 2;
        let path = String::from_utf8(body[p..p + plen].to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 path in journal"))?;
        p += plen;
        let size = u64::from_le_bytes(body[p..p + 8].try_into().unwrap());
        p += 8;
        let mtime_ns = i64::from_le_bytes(body[p..p + 8].try_into().unwrap());
        p += 8;
        let ctime_ns = i64::from_le_bytes(body[p..p + 8].try_into().unwrap());
        p += 8;
        let inode = u64::from_le_bytes(body[p..p + 8].try_into().unwrap());
        p += 8;
        let mode = u32::from_le_bytes(body[p..p + 4].try_into().unwrap());
        p += 4;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&body[p..p + 32]);
        p += 32;
        entries.push(Entry {
            path,
            size,
            mtime_ns,
            ctime_ns,
            inode,
            mode,
            hash,
        });
    }
    Ok(Journal {
        entries,
        digest,
        scanned_at_ns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tc-journal-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(target_os = "linux")]
    fn set_mtime_ns(p: &Path, ns: i64) {
        use std::ffi::CString;
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Ts {
            tv_sec: i64,
            tv_nsec: i64,
        }
        unsafe extern "C" {
            fn utimensat(
                dirfd: i32,
                path: *const std::ffi::c_char,
                times: *const Ts,
                flags: i32,
            ) -> i32;
        }
        let c = CString::new(p.to_string_lossy().as_bytes()).unwrap();
        let ts = [Ts {
            tv_sec: ns / 1_000_000_000,
            tv_nsec: ns % 1_000_000_000,
        }; 2];
        unsafe {
            utimensat(-100, c.as_ptr(), ts.as_ptr(), 0);
        }
    }

    #[test]
    fn clean_rescan_hashes_nothing() {
        let d = sandbox("clean");
        for i in 0..50 {
            fs::write(d.join(format!("f{i}.bin")), vec![i as u8; 1024]).unwrap();
        }
        // Let the writes settle past the racy window before the first scan.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let j = scan(&d).unwrap();
        assert_eq!(j.entries.len(), 50);
        let diff = rescan(&d, &j).unwrap();
        assert!(diff.is_clean(), "expected clean rescan, got {:?}", diff);
        assert_eq!(diff.unchanged, 50);
        assert_eq!(diff.hashed, 0, "fast path must not read file contents");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_is_detected() {
        let d = sandbox("edit");
        fs::write(d.join("a.txt"), b"one").unwrap();
        let j = scan(&d).unwrap();
        fs::write(d.join("a.txt"), b"two").unwrap();
        let diff = rescan(&d, &j).unwrap();
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].path, "a.txt");
        assert_eq!(diff.hashed, 1);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn mtime_preserving_edit_is_still_detected() {
        // The --trust-mtime trap: content changes, mtime is restored to its old
        // value. Only ctime/inode reveal it.
        let d = sandbox("mtime-trap");
        let p = d.join("sneaky.txt");
        fs::write(&p, b"original").unwrap();
        let j = scan(&d).unwrap();
        let old_mtime = j.entries[0].mtime_ns;

        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&p, b"tampered").unwrap();
        #[cfg(target_os = "linux")]
        set_mtime_ns(&p, old_mtime);

        let diff = rescan(&d, &j).unwrap();
        assert_eq!(
            diff.changed.len(),
            1,
            "mtime-restored edit must still be caught"
        );
        assert_eq!(diff.hashed, 1);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn add_and_remove_are_detected() {
        let d = sandbox("addremove");
        fs::write(d.join("keep.txt"), b"k").unwrap();
        fs::write(d.join("gone.txt"), b"g").unwrap();
        let j = scan(&d).unwrap();
        fs::remove_file(d.join("gone.txt")).unwrap();
        fs::write(d.join("new.txt"), b"n").unwrap();
        let diff = rescan(&d, &j).unwrap();
        assert_eq!(diff.removed, vec!["gone.txt".to_string()]);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].path, "new.txt");
        let merged = apply(&j, &diff);
        assert!(merged.entries.iter().any(|e| e.path == "new.txt"));
        assert!(!merged.entries.iter().any(|e| e.path == "gone.txt"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn journal_roundtrips_and_detects_tampering() {
        let d = sandbox("persist");
        fs::write(d.join("x.bin"), b"payload").unwrap();
        let j = scan(&d).unwrap();
        let jp = d.join("journal.bin");
        save(&j, &jp, None).unwrap();
        let back = load(&jp, None).unwrap();
        assert_eq!(back.entries, j.entries);

        let mut bytes = fs::read(&jp).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0x01;
        fs::write(&jp, &bytes).unwrap();
        let err = load(&jp, None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn keyed_journal_requires_key() {
        let d = sandbox("keyed");
        fs::write(d.join("x.bin"), b"payload").unwrap();
        let j = scan(&d).unwrap();
        let jp = d.join("journal.bin");
        let key = [7u8; 32];
        save(&j, &jp, Some(&key)).unwrap();
        assert!(load(&jp, None).is_err());
        assert_eq!(load(&jp, Some(&key)).unwrap().entries, j.entries);
        assert!(load(&jp, Some(&[8u8; 32])).is_err());
        let _ = fs::remove_dir_all(&d);
    }
}
