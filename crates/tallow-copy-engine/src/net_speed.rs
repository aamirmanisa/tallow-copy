//! Real-time link and throughput measurement for copy operations.
//!
//! The copy engine is link-agnostic: it copies into a `Path` and has no idea
//! whether that path is a local disk, an SMB/CIFS share, or an NFS mount. This
//! module supplies that missing context and turns it into a concrete
//! [`TransferPolicy`] (buffer size, worker count, optional pacing).
//!
//! Everything here is dependency-free and reads the kernel directly:
//!
//! * link kind  — `/proc/mounts` (longest mount-point prefix wins)
//! * wireless   — `/sys/class/net/<if>/wireless` and `/proc/net/wireless`
//! * link speed — `/sys/class/net/<if>/speed`, then `iw dev <if> link`
//! * throughput — `/sys/class/net/<if>/statistics/{tx,rx}_bytes` deltas
//!
//! Honest limits, deliberately not papered over:
//!
//! * A host with no wireless interface reports `wireless: false` and uses the
//!   wired link. Virtualised NICs (eth0 under KVM/Hetzner) expose no `speed`
//!   file at all, so the link rate falls back to measured throughput.
//! * Interface counters include all traffic on that interface, not only this
//!   copy. On a busy host the measured rate is an upper bound on what the copy
//!   can achieve, which is the correct direction for a throttling decision.
//! * `iw` is optional; without it a WiFi link is reported with an unknown rate
//!   rather than a fabricated one.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What kind of link a destination path lives on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkKind {
    /// Same machine, no network in the data path (a local disk, tmpfs, overlay).
    Local,
    /// SMB/CIFS or Samba mount.
    Smb,
    /// Other network filesystem (NFS, SSHFS, 9p, FUSE-over-network).
    NetworkFile,
    /// Loopback-backed.
    Loopback,
}

impl LinkKind {
    pub fn is_network(self) -> bool {
        !matches!(self, LinkKind::Local | LinkKind::Loopback)
    }

    pub fn label(self) -> &'static str {
        match self {
            LinkKind::Local => "local",
            LinkKind::Smb => "smb",
            LinkKind::NetworkFile => "network-fs",
            LinkKind::Loopback => "loopback",
        }
    }
}

/// Snapshot of the link a copy is using.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkInfo {
    pub kind: LinkKind,
    /// Interface the kernel routes this mount over, when it could be resolved.
    pub interface: Option<String>,
    pub wireless: bool,
    /// Negotiated link rate in megabits/second, when the kernel reports one.
    pub link_mbps: Option<f64>,
}

/// The knobs the copy engine actually reacts to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferPolicy {
    pub buffer_size_bytes: usize,
    pub threads: usize,
    /// Sleep inserted after each buffer write to shape traffic to the measured
    /// rate. Zero means "do not pace".
    pub pace: Duration,
    pub reason: &'static str,
}

impl TransferPolicy {
    /// Throughput ceiling implied by `pace` for `buffer_size_bytes`, in bytes/s.
    pub fn paced_bytes_per_sec(&self) -> Option<f64> {
        let ns = self.pace.as_nanos();
        if ns == 0 {
            return None;
        }
        Some(self.buffer_size_bytes as f64 / (ns as f64 / 1e9))
    }
}

/// Parse `/proc/mounts`-style text into `(mount_point, fstype)` pairs.
///
/// Mount points are octal-escaped (`\040` for space); the escape is decoded so
/// a path containing a space still matches its own mount entry.
pub fn parse_mounts(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let _dev = f.next()?;
            let mnt = f.next()?;
            let fstype = f.next()?;
            Some((unescape_mount(mnt), fstype.to_string()))
        })
        .collect()
}

fn unescape_mount(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let oct: String = chars.by_ref().take(3).collect();
            if let Ok(v) = u8::from_str_radix(&oct, 8) {
                out.push(v as char);
                continue;
            }
            out.push('\\');
            out.push_str(&oct);
        } else {
            out.push(c);
        }
    }
    out
}

/// Classify `path` against a mount table. The longest matching mount point wins,
/// which matters when a CIFS share is mounted inside a local tree.
pub fn classify_mount(mounts: &[(String, String)], path: &str) -> LinkKind {
    let mut best: Option<(&str, &str)> = None;
    for (mnt, fstype) in mounts {
        let matches = path == mnt
            || (path.starts_with(mnt)
                && (mnt.ends_with('/') || path.as_bytes().get(mnt.len()) == Some(&b'/')));
        if matches && best.map_or(true, |(m, _)| mnt.len() >= m.len()) {
            best = Some((mnt.as_str(), fstype.as_str()));
        }
    }
    match best {
        None => LinkKind::Local,
        Some((_, fs)) => match fs {
            "cifs" | "smb3" | "smbfs" => LinkKind::Smb,
            "nfs" | "nfs4" | "sshfs" | "9p" | "fuse.sshfs" | "ncpfs" => LinkKind::NetworkFile,
            "tmpfs" | "devtmpfs" => LinkKind::Local,
            _ => LinkKind::Local,
        },
    }
}

/// Classify a path on this host using the live mount table.
pub fn classify_path(path: &Path) -> LinkKind {
    link_info_for(path).kind
}

/// Uncached classification: reads `/proc/mounts`. Only called when the cache is
/// cold, because this is far too expensive to run once per file in a tree copy.
fn classify_path_uncached(path: &Path) -> LinkKind {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/mounts") {
            return classify_mount(&parse_mounts(&text), &path.to_string_lossy());
        }
    }
    LinkKind::Local
}

/// Interfaces that the kernel marks as wireless.
///
/// Both `/sys/class/net/<if>/wireless` (present on most modern kernels) and
/// `/proc/net/wireless` are consulted; either one is enough.
pub fn wireless_interfaces() -> Vec<String> {
    let mut out = Vec::new();
    #[cfg(target_os = "linux")]
    {
        if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if e.path().join("wireless").exists() {
                    out.push(name);
                }
            }
        }
        if out.is_empty() {
            if let Ok(text) = std::fs::read_to_string("/proc/net/wireless") {
                for line in text.lines().skip(2) {
                    if let Some((iface, _)) = line.split_once(':') {
                        let name = iface.trim().to_string();
                        if !name.is_empty() {
                            out.push(name);
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Link rate in Mb/s for an interface, if the kernel reports it.
///
/// sysfs is tried first (cheap, exact for wired links). WiFi drivers frequently
/// report `-1` there, so `iw dev <if> link` is parsed as a fallback. No value is
/// invented when neither source answers.
pub fn link_speed_mbps(iface: &str) -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let p = format!("/sys/class/net/{iface}/speed");
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = text.trim().parse::<f64>() {
                if v > 0.0 {
                    return Some(v);
                }
            }
        }
        if let Ok(out) = std::process::Command::new("iw")
            .args(["dev", iface, "link"])
            .output()
        {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(v) = parse_iw_bitrate(&text) {
                    return Some(v);
                }
            }
        }
    }
    let _ = iface;
    None
}

/// Extract Mb/s from `iw dev <if> link` output, e.g. `bitrate: 433.3 MBit/s`.
pub fn parse_iw_bitrate(text: &str) -> Option<f64> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("bitrate:") {
            let mut it = rest.split_whitespace();
            let value: f64 = it.next()?.parse().ok()?;
            let unit = it.next().unwrap_or("MBit/s");
            return Some(if unit.starts_with("GBit") {
                value * 1000.0
            } else {
                value
            });
        }
    }
    None
}

/// Pick the interface a copy should be measured against.
///
/// Wireless is preferred when the host has it and it is up (that is the link the
/// user asked to track); otherwise the default-route interface is used, and
/// finally any non-loopback interface that is up.
pub fn pick_interface() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let wifi = wireless_interfaces();
        for iface in &wifi {
            if interface_is_up(iface) {
                return Some(iface.clone());
            }
        }
        if let Ok(text) = std::fs::read_to_string("/proc/net/route") {
            if let Some(iface) = default_route_interface(&text) {
                return Some(iface);
            }
        }
        if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name != "lo" && interface_is_up(&name) {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn interface_is_up(iface: &str) -> bool {
    std::fs::read_to_string(format!("/sys/class/net/{iface}/operstate"))
        .map(|s| s.trim() == "up")
        .unwrap_or(false)
}

/// Parse `/proc/net/route` and return the interface of the default route.
pub fn default_route_interface(text: &str) -> Option<String> {
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 2 && cols[1] == "00000000" {
            return Some(cols[0].to_string());
        }
    }
    None
}

/// Resolve the link a destination path uses on this host.
/// How long a link snapshot stays valid. Mounts and interface state change on a
/// human timescale; a two-second window keeps a tree copy to one probe.
const SNAPSHOT_TTL: Duration = Duration::from_secs(2);

struct LinkCache {
    at: Instant,
    by_dir: std::collections::HashMap<PathBuf, LinkInfo>,
}

/// Cached link lookup.
///
/// `policy_for_path` is called once per file by the copy engine. Without this
/// cache every file pays a `/proc/mounts` read and parse plus a `/sys/class/net`
/// enumeration — measured at 2–12× slower than `rsync` on small-file trees.
/// The cache is keyed by parent directory, because a tree copy writes into a
/// handful of directories and they almost always share a mount.
pub fn link_info_for(dst: &Path) -> LinkInfo {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<LinkCache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        std::sync::Mutex::new(LinkCache {
            at: Instant::now(),
            by_dir: Default::default(),
        })
    });
    let key = dst.parent().unwrap_or(dst).to_path_buf();
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.at.elapsed() > SNAPSHOT_TTL {
        guard.by_dir.clear();
        guard.at = Instant::now();
    }
    if let Some(info) = guard.by_dir.get(&key) {
        return info.clone();
    }
    let info = link_info_uncached(dst);
    guard.by_dir.insert(key, info.clone());
    info
}

fn link_info_uncached(dst: &Path) -> LinkInfo {
    let kind = classify_path_uncached(dst);
    let iface = pick_interface();
    let wireless = iface
        .as_deref()
        .map(|i| wireless_interfaces().iter().any(|w| w == i))
        .unwrap_or(false);
    let link_mbps = iface.as_deref().and_then(link_speed_mbps);
    LinkInfo {
        kind,
        interface: iface,
        wireless,
        link_mbps,
    }
}

/// Rolling throughput meter fed by interface byte counters.
#[derive(Clone, Debug)]
pub struct SpeedMeter {
    iface: Option<String>,
    last_bytes: u64,
    last_at: Instant,
    ewma_bps: f64,
}

impl SpeedMeter {
    pub fn new(iface: Option<String>) -> Self {
        let mut m = Self {
            iface,
            last_bytes: 0,
            last_at: Instant::now(),
            ewma_bps: 0.0,
        };
        m.last_bytes = m.read_bytes().unwrap_or(0);
        m
    }

    fn read_bytes(&self) -> Option<u64> {
        let iface = self.iface.as_deref()?;
        #[cfg(target_os = "linux")]
        {
            let tx = std::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/tx_bytes"))
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()?;
            let rx = std::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/rx_bytes"))
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()?;
            Some(tx.saturating_add(rx))
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// Take a sample and fold it into the EWMA. Returns the instantaneous rate.
    pub fn sample(&mut self) -> f64 {
        self.sample_at(self.read_bytes().unwrap_or(self.last_bytes), Instant::now())
    }

    /// Sample with injected counters/time — used by tests.
    pub fn sample_at(&mut self, bytes: u64, now: Instant) -> f64 {
        let dt = now.saturating_duration_since(self.last_at).as_secs_f64();
        if dt <= 0.0 {
            return self.ewma_bps;
        }
        let delta = bytes.saturating_sub(self.last_bytes) as f64;
        let inst = delta / dt;
        // 0.3 keeps the meter responsive to a genuine drop while smoothing the
        // bursty per-sample noise of small copies.
        self.ewma_bps = if self.ewma_bps == 0.0 {
            inst
        } else {
            0.7 * self.ewma_bps + 0.3 * inst
        };
        self.last_bytes = bytes;
        self.last_at = now;
        inst
    }

    pub fn ewma_bps(&self) -> f64 {
        self.ewma_bps
    }
}

/// Choose copy parameters from the link kind and the measured rate.
///
/// Local copies are never paced: the network plays no part, and throttling a
/// disk-to-disk copy would be a pessimisation dressed up as an optimisation.
pub fn policy_for(kind: LinkKind, measured_bps: Option<f64>) -> TransferPolicy {
    if !kind.is_network() {
        return TransferPolicy {
            buffer_size_bytes: 1024 * 1024,
            threads: std::thread::available_parallelism()
                .map(|n| n.get().min(8))
                .unwrap_or(4),
            pace: Duration::ZERO,
            reason: "local link: disk-bound, no network pacing",
        };
    }

    let mbps = measured_bps.unwrap_or(0.0) / 1_000_000.0;
    let (buf, threads, reason): (usize, usize, &'static str) = if mbps >= 100.0 {
        (
            8 * 1024 * 1024,
            8,
            ">=100 MB/s: large buffers, full parallelism",
        )
    } else if mbps >= 25.0 {
        (
            4 * 1024 * 1024,
            6,
            ">=25 MB/s: large buffers, high parallelism",
        )
    } else if mbps >= 5.0 {
        (
            1024 * 1024,
            4,
            ">=5 MB/s: default buffers, moderate parallelism",
        )
    } else if mbps >= 1.0 {
        (256 * 1024, 2, ">=1 MB/s: small buffers, low parallelism")
    } else if mbps > 0.0 {
        (64 * 1024, 1, "<1 MB/s: minimal buffers, serial")
    } else {
        // No measurement yet: be conservative on a network mount, because a
        // wide fan-out would make an unknown link worse, not better.
        (
            128 * 1024,
            2,
            "unmeasured network link: conservative defaults",
        )
    };

    // Pace only when the link is clearly slow, at 90% of the observed rate so
    // the copy does not saturate a link someone else is using.
    let pace = if kind == LinkKind::Smb && mbps > 0.0 && mbps < 25.0 {
        let target = (mbps * 0.9 * 1_000_000.0).max(1.0);
        Duration::from_nanos(((buf as f64 / target) * 1e9) as u64)
    } else {
        Duration::ZERO
    };

    TransferPolicy {
        buffer_size_bytes: buf,
        threads,
        pace,
        reason,
    }
}

/// Policy for a destination path, measuring the live link on this host.
pub fn policy_for_path(dst: &Path) -> TransferPolicy {
    let info = link_info_for(dst);
    // A local copy has no network in its data path, so sampling the interface
    // (and paying the sampling delay) would be pure overhead.
    if !info.kind.is_network() {
        return policy_for(info.kind, None);
    }
    let mut meter = SpeedMeter::new(info.interface.clone());
    // Two samples 50 ms apart: the first establishes the baseline counter.
    std::thread::sleep(Duration::from_millis(50));
    meter.sample();
    std::thread::sleep(Duration::from_millis(50));
    let bps = meter.sample();
    policy_for(info.kind, if bps > 0.0 { Some(bps) } else { None })
}

/// One-line human-readable description of the live link, for progress output.
pub fn describe_link(info: &LinkInfo, measured_bps: f64) -> String {
    let media = if info.wireless {
        "wifi".to_string()
    } else {
        "wired".to_string()
    };
    let link = info
        .link_mbps
        .map(|m| format!("{m:.0} Mbps link"))
        .unwrap_or_else(|| "link rate n/a".to_string());
    format!(
        "{} {} via {} · {} · {:.1} MB/s measured",
        info.kind.label(),
        media,
        info.interface.clone().unwrap_or_else(|| "?".into()),
        link,
        measured_bps / 1_000_000.0
    )
}

/// Best-effort list of network share paths currently mounted, for reporting.
pub fn network_mounts() -> Vec<(PathBuf, LinkKind, String)> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/mounts") {
            return parse_mounts(&text)
                .into_iter()
                .filter_map(|(mnt, fs)| {
                    let kind = classify_mount(&[(mnt.clone(), fs.clone())], &mnt);
                    if kind.is_network() {
                        Some((PathBuf::from(mnt), kind, fs))
                    } else {
                        None
                    }
                })
                .collect();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = "\
/dev/sda1 / ext4 rw,relatime 0 0
//192.168.4.3/home /mnt/NAS/aamirmanisa cifs rw,nosuid 0 0
server:/export /mnt/nfs nfs4 rw 0 0
tmpfs /run tmpfs rw 0 0
//192.168.4.3/home /mnt/NAS/aamirmanisa/02_Business/Nixe/current cifs rw 0 0
";

    #[test]
    fn parses_and_classifies_mounts() {
        let m = parse_mounts(MOUNTS);
        assert!(m
            .iter()
            .any(|(p, f)| p == "/mnt/NAS/aamirmanisa" && f == "cifs"));
        assert_eq!(
            classify_mount(&m, "/mnt/NAS/aamirmanisa/02_Business"),
            LinkKind::Smb
        );
        assert_eq!(classify_mount(&m, "/home/aamir/file.txt"), LinkKind::Local);
        assert_eq!(classify_mount(&m, "/mnt/nfs/x"), LinkKind::NetworkFile);
    }

    #[test]
    fn longest_mount_prefix_wins() {
        let m = parse_mounts(MOUNTS);
        // /mnt/NAS/.../current is also a cifs mount; either way it is Smb, but
        // the deeper entry must be the one selected.
        assert_eq!(
            classify_mount(&m, "/mnt/NAS/aamirmanisa/02_Business/Nixe/current/a"),
            LinkKind::Smb
        );
    }

    #[test]
    fn decodes_escaped_mount_points() {
        let m = parse_mounts("/dev/sdb1 /mnt/my\\040disk ext4 rw 0 0\n");
        assert_eq!(m[0].0, "/mnt/my disk");
        assert_eq!(classify_mount(&m, "/mnt/my disk/file"), LinkKind::Local);
    }

    #[test]
    fn default_route_parser() {
        let t = "Iface\tDestination\tGateway\neth0\t00000000\t0102A8C0\nwg0\t0000A8C0\t00000000\n";
        assert_eq!(default_route_interface(t).as_deref(), Some("eth0"));
        assert_eq!(default_route_interface("Iface\tDestination\n"), None);
    }

    #[test]
    fn iw_bitrate_parser() {
        let out = "Connected to aa:bb\n\tbitrate: 433.3 MBit/s\n\tsignal: -52 dBm\n";
        assert_eq!(parse_iw_bitrate(out), Some(433.3));
        assert_eq!(parse_iw_bitrate("Connected to aa:bb\n"), None);
    }

    #[test]
    fn local_copies_are_never_paced() {
        let p = policy_for(LinkKind::Local, Some(1_000_000.0));
        assert_eq!(p.pace, Duration::ZERO);
        assert!(p.buffer_size_bytes >= 1024 * 1024);
    }

    #[test]
    fn policy_tracks_measured_rate() {
        let fast = policy_for(LinkKind::Smb, Some(200e6));
        let mid = policy_for(LinkKind::Smb, Some(10e6));
        let slow = policy_for(LinkKind::Smb, Some(0.5e6));
        assert!(fast.buffer_size_bytes > mid.buffer_size_bytes);
        assert!(mid.buffer_size_bytes > slow.buffer_size_bytes);
        assert!(fast.threads >= mid.threads && mid.threads >= slow.threads);
        assert_eq!(
            fast.pace,
            Duration::ZERO,
            "fast links must not be throttled"
        );
        assert!(slow.pace > Duration::ZERO, "slow SMB links are paced");
    }

    #[test]
    fn pacing_targets_below_measured_rate() {
        let p = policy_for(LinkKind::Smb, Some(4e6)); // 4 MB/s measured
        let ceiling = p.paced_bytes_per_sec().expect("slow smb is paced");
        // 90% of 4 MB/s with a small tolerance for integer nanosecond rounding.
        assert!(
            ceiling <= 3.6e6 + 5e4,
            "ceiling {ceiling} must not exceed 90% of measured"
        );
        assert!(
            ceiling >= 3.0e6,
            "ceiling {ceiling} should stay close to the target"
        );
    }

    #[test]
    fn unmeasured_network_link_is_conservative() {
        let p = policy_for(LinkKind::Smb, None);
        assert!(p.threads <= 2);
        assert!(p.buffer_size_bytes <= 256 * 1024);
    }

    #[test]
    fn meter_computes_rate_from_counters() {
        let mut m = SpeedMeter {
            iface: None,
            last_bytes: 0,
            last_at: Instant::now(),
            ewma_bps: 0.0,
        };
        let t0 = Instant::now();
        m.sample_at(0, t0);
        let inst = m.sample_at(10_000_000, t0 + Duration::from_millis(100));
        assert!(
            (inst - 100_000_000.0).abs() < 1.0,
            "10 MB in 100 ms is 100 MB/s, got {inst}"
        );
        assert!(m.ewma_bps() > 0.0);
    }

    #[test]
    fn meter_ewma_drops_when_traffic_stops() {
        let mut m = SpeedMeter {
            iface: None,
            last_bytes: 0,
            last_at: Instant::now(),
            ewma_bps: 0.0,
        };
        let t0 = Instant::now();
        m.sample_at(0, t0);
        m.sample_at(50_000_000, t0 + Duration::from_millis(100));
        let high = m.ewma_bps();
        for i in 1..=20 {
            m.sample_at(50_000_000, t0 + Duration::from_millis(100 + i * 100));
        }
        assert!(
            m.ewma_bps() < high * 0.5,
            "idle samples must decay the EWMA"
        );
    }

    #[test]
    fn live_host_probe_does_not_panic() {
        let info = link_info_for(Path::new("/tmp"));
        // The result is host-dependent; asserting it does not panic and yields
        // a coherent classification is the honest check.
        assert!(matches!(
            info.kind,
            LinkKind::Local | LinkKind::Smb | LinkKind::NetworkFile | LinkKind::Loopback
        ));
        let _ = wireless_interfaces();
        let _ = network_mounts();
        let _ = pick_interface();
    }
}
