//! Transfer policy: the small, auditable control plane.
//!
//! The data plane is deliberately dumb — it moves bytes as fast as the link
//! allows. Everything that decides *whether* a transfer may happen lives here, so
//! that the security-relevant logic is small enough to read in one sitting.
//!
//! The model is a destination allowlist plus an optional legal annotation. A
//! transfer is refused unless the destination resolves inside an allowed root and
//! the annotation (when required) is present. Refusal happens before any byte is
//! read, so a rejected transfer costs nothing and cannot leak a file's existence
//! through partial output.

use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct TransferPolicy {
    /// Destinations must resolve inside one of these roots. Empty means nothing is
    /// allowed — an empty allowlist is a closed door, never an open one.
    pub allowed_roots: Vec<PathBuf>,
    /// Free-form `@legal` annotation recorded with the transfer.
    pub legal_annotation: Option<String>,
    /// When true, a transfer without an annotation is refused.
    pub require_annotation: bool,
    /// Optional ceiling on total bytes for a single transfer.
    pub max_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyError {
    OutsideAllowedRoots { path: String },
    MissingAnnotation,
    ExceedsMaxBytes { requested: u64, max: u64 },
    UnsafePath { path: String },
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::OutsideAllowedRoots { path } => {
                write!(f, "destination {path} is outside the allowed roots")
            }
            PolicyError::MissingAnnotation => write!(f, "transfer requires a @legal annotation"),
            PolicyError::ExceedsMaxBytes { requested, max } => {
                write!(
                    f,
                    "transfer of {requested} bytes exceeds the limit of {max}"
                )
            }
            PolicyError::UnsafePath { path } => write!(f, "unsafe path {path}"),
        }
    }
}

impl std::error::Error for PolicyError {}

/// Lexically normalise a path: resolve `.` and fold `..` without touching the
/// filesystem. Symlinks are not followed, because following them would let a
/// destination escape the allowlist through a link.
fn normalize(p: &Path) -> Result<PathBuf, PolicyError> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(PolicyError::UnsafePath {
                        path: p.display().to_string(),
                    });
                }
            }
            Component::Normal(s) => out.push(s),
        }
    }
    Ok(out)
}

impl TransferPolicy {
    pub fn new(allowed_roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            allowed_roots: allowed_roots.into_iter().collect(),
            ..Default::default()
        }
    }

    pub fn with_annotation(mut self, annotation: impl Into<String>) -> Self {
        self.legal_annotation = Some(annotation.into());
        self
    }

    pub fn requiring_annotation(mut self) -> Self {
        self.require_annotation = true;
        self
    }

    pub fn with_max_bytes(mut self, max: u64) -> Self {
        self.max_bytes = Some(max);
        self
    }

    /// Authorise a transfer of `bytes` to `dst`. Checks run cheapest-first and
    /// every check must pass.
    pub fn check(&self, dst: &Path, bytes: u64) -> Result<(), PolicyError> {
        if self.require_annotation
            && self
                .legal_annotation
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
        {
            return Err(PolicyError::MissingAnnotation);
        }
        if let Some(max) = self.max_bytes {
            if bytes > max {
                return Err(PolicyError::ExceedsMaxBytes {
                    requested: bytes,
                    max,
                });
            }
        }
        let dst = normalize(dst)?;
        let inside = self.allowed_roots.iter().any(|root| {
            normalize(root)
                .map(|r| dst.starts_with(&r))
                .unwrap_or(false)
        });
        if !inside {
            return Err(PolicyError::OutsideAllowedRoots {
                path: dst.display().to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> TransferPolicy {
        TransferPolicy::new([PathBuf::from("/mnt/Personal/hetzner-server")])
    }

    #[test]
    fn allows_destination_inside_root() {
        let p = policy();
        assert!(p
            .check(
                Path::new("/mnt/Personal/hetzner-server/home/aamir/x.txt"),
                10
            )
            .is_ok());
    }

    #[test]
    fn refuses_destination_outside_root() {
        let p = policy();
        let err = p.check(Path::new("/etc/passwd"), 10).unwrap_err();
        assert!(matches!(err, PolicyError::OutsideAllowedRoots { .. }));
    }

    #[test]
    fn refuses_traversal_escape() {
        let p = policy();
        // Lexically inside the root, but the `..` folds out of it.
        let err = p
            .check(
                Path::new("/mnt/Personal/hetzner-server/../../../etc/shadow"),
                10,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            PolicyError::OutsideAllowedRoots { .. } | PolicyError::UnsafePath { .. }
        ));
    }

    #[test]
    fn refuses_sibling_prefix_confusion() {
        let p = policy();
        // "/mnt/Personal/hetzner-server-evil" must not match the root prefix.
        let err = p
            .check(Path::new("/mnt/Personal/hetzner-server-evil/x"), 10)
            .unwrap_err();
        assert!(matches!(err, PolicyError::OutsideAllowedRoots { .. }));
    }

    #[test]
    fn empty_allowlist_is_closed() {
        let p = TransferPolicy::default();
        assert!(p.check(Path::new("/tmp/x"), 1).is_err());
    }

    #[test]
    fn annotation_can_be_required() {
        let p = policy().requiring_annotation();
        assert_eq!(
            p.check(Path::new("/mnt/Personal/hetzner-server/a"), 1)
                .unwrap_err(),
            PolicyError::MissingAnnotation
        );
        let ok = policy()
            .requiring_annotation()
            .with_annotation("@legal:internal-only");
        assert!(ok
            .check(Path::new("/mnt/Personal/hetzner-server/a"), 1)
            .is_ok());
    }

    #[test]
    fn byte_ceiling_is_enforced() {
        let p = policy().with_max_bytes(100);
        assert!(p
            .check(Path::new("/mnt/Personal/hetzner-server/a"), 101)
            .is_err());
        assert!(p
            .check(Path::new("/mnt/Personal/hetzner-server/a"), 100)
            .is_ok());
    }
}
