//! Persistent volumes mounted into a sandbox, decoupled from its lifetime.
//!
//! See ADR-034. A sandbox may mount zero or more volumes at distinct paths,
//! each realized by its own driver — a working tree on a local POSIX volume,
//! a shared dataset on object storage, in the same sandbox.
//!
//! The driver split is load-bearing rather than cosmetic. A working tree
//! needs atomic rename, locking, and fast small-file IO: `git status` and a
//! package install writing ~10^5 small files depend on all three. Object
//! storage behind a FUSE layer provides none of them, but is a good fit for
//! the shared-asset case. Making the driver a property of the *mount* is
//! what lets both live on one sandbox without pretending they are the same
//! kind of storage.
//!
//! This module is pure: it defines the shapes and validates them. Realizing
//! a mount is the provider's job (`docker.rs`, `firecracker.rs`, …).

use std::collections::HashSet;

use super::user::SANDBOX_WORK_DIR;

/// Mount points a volume may never occupy. Mounting over these either
/// bricks the sandbox or hands the caller a escape primitive, and neither
/// failure is one the caller can debug from the outside.
const RESERVED_MOUNT_PATHS: &[&str] = &[
    "/", "/bin", "/boot", "/dev", "/etc", "/lib", "/lib64", "/proc", "/root", "/run", "/sbin",
    "/sys", "/usr", "/var",
];

/// How a volume is realized.
///
/// Variants are deliberately not interchangeable: see the module docs for
/// why a working tree cannot live on [`VolumeDriver::S3`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeDriver {
    /// Instance-local persistent volume (a Docker named volume today).
    /// POSIX semantics: atomic rename, locking, fast small-file IO. The
    /// only driver suitable for a working tree or a git repository.
    Local,
    /// Object storage presented as a filesystem. No atomic rename, no
    /// locking, a round trip per file. Appropriate for datasets and shared
    /// build assets; not for a working tree.
    S3 {
        bucket: String,
        /// Key prefix within the bucket. `None` mounts the bucket root.
        prefix: Option<String>,
        /// Reference to a stored credential, resolved through the egress
        /// proxy (ADR-013). Deliberately not the secret itself: inlining
        /// bucket credentials would put them in the container's
        /// environment, reopening the `/proc/<pid>/environ` exfiltration
        /// path that ADR closed.
        credential_ref: String,
    },
}

impl VolumeDriver {
    /// Stable identifier for persistence and API payloads.
    pub fn kind(&self) -> &'static str {
        match self {
            VolumeDriver::Local => "local",
            VolumeDriver::S3 { .. } => "s3",
        }
    }

    /// Whether this driver provides the POSIX guarantees a working tree
    /// needs. Callers mounting a repository should refuse a driver that
    /// returns `false` rather than discover it during a `bun install`.
    pub fn supports_posix_semantics(&self) -> bool {
        matches!(self, VolumeDriver::Local)
    }
}

/// One volume attached to a sandbox at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeMount {
    /// Volume identifier, unique within its driver. Caller-supplied and
    /// opaque to temps — the caller owns both the namespace and the
    /// lifetime, because only the caller knows whether the resource the
    /// volume belongs to still exists.
    pub volume: String,
    /// Absolute in-container mount point (`/home/temps/workspace`,
    /// `/shared`).
    pub mount_path: String,
    pub driver: VolumeDriver,
    /// Mount read-only. Required for a volume mounted by more than one
    /// concurrently running sandbox unless the driver guarantees safe
    /// concurrent writes — none of the current drivers do.
    pub read_only: bool,
}

impl VolumeMount {
    /// A local volume at the default work dir — the common case, and the
    /// shape that replaces the old single `workspace_volume` field.
    pub fn workspace(volume: impl Into<String>) -> Self {
        Self {
            volume: volume.into(),
            mount_path: SANDBOX_WORK_DIR.to_string(),
            driver: VolumeDriver::Local,
            read_only: false,
        }
    }

    /// Whether this mount covers the sandbox work dir, i.e. holds the
    /// working tree.
    pub fn is_workspace(&self) -> bool {
        self.mount_path == SANDBOX_WORK_DIR
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum VolumeMountError {
    EmptyVolumeName,
    InvalidVolumeName(String),
    RelativePath(String),
    UnnormalizedPath(String),
    ReservedPath(String),
    DuplicatePath(String),
    NestedPaths { outer: String, inner: String },
    WorkspaceNeedsPosix { driver: &'static str },
}

impl std::fmt::Display for VolumeMountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyVolumeName => write!(f, "Volume name must not be empty."),
            Self::InvalidVolumeName(name) => write!(
                f,
                "Volume name {name:?} is invalid — use letters, digits, dots, dashes, and \
                 underscores only."
            ),
            Self::RelativePath(path) => {
                write!(f, "Mount path {path:?} must be absolute.")
            }
            Self::UnnormalizedPath(path) => write!(
                f,
                "Mount path {path:?} must be normalized — no '.', '..', or empty segments."
            ),
            Self::ReservedPath(path) => write!(
                f,
                "Mount path {path:?} is reserved; mounting over it would break the sandbox."
            ),
            Self::DuplicatePath(path) => {
                write!(f, "Two volumes both mount at {path:?}.")
            }
            Self::NestedPaths { outer, inner } => write!(
                f,
                "Mount paths {outer:?} and {inner:?} overlap. Nested mounts resolve by order, so \
                 one would silently shadow part of the other — use disjoint paths."
            ),
            Self::WorkspaceNeedsPosix { driver } => write!(
                f,
                "The work dir cannot use the {driver:?} driver: it has no atomic rename or \
                 locking, which git and package installs require. Use a local volume for the \
                 working tree and mount {driver:?} elsewhere."
            ),
        }
    }
}

impl std::error::Error for VolumeMountError {}

/// Validate a sandbox's full mount set.
///
/// Checked as a set rather than per-mount because the interesting failures
/// are relational: two mounts at one path, or one nested inside another.
/// Both are rejected rather than resolved — a caller who gets mount
/// ordering wrong sees one volume silently shadow part of another, and a
/// silent shadow is far more expensive to diagnose than a create-time
/// error.
pub fn validate_mounts(mounts: &[VolumeMount]) -> Result<(), VolumeMountError> {
    let mut seen: HashSet<&str> = HashSet::new();

    for mount in mounts {
        validate_volume_name(&mount.volume)?;
        validate_mount_path(&mount.mount_path)?;

        if mount.is_workspace() && !mount.driver.supports_posix_semantics() {
            return Err(VolumeMountError::WorkspaceNeedsPosix {
                driver: mount.driver.kind(),
            });
        }

        if !seen.insert(mount.mount_path.as_str()) {
            return Err(VolumeMountError::DuplicatePath(mount.mount_path.clone()));
        }
    }

    for (i, outer) in mounts.iter().enumerate() {
        for inner in &mounts[i + 1..] {
            if let Some((outer, inner)) = overlapping(&outer.mount_path, &inner.mount_path) {
                return Err(VolumeMountError::NestedPaths {
                    outer: outer.to_string(),
                    inner: inner.to_string(),
                });
            }
        }
    }

    Ok(())
}

/// `Some((outer, inner))` when one path contains the other. Compares whole
/// segments: `/shared` contains `/shared/data` but not `/shared-cache`.
fn overlapping<'a>(a: &'a str, b: &'a str) -> Option<(&'a str, &'a str)> {
    let (shorter, longer) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let boundary = format!("{}/", shorter.trim_end_matches('/'));
    longer.starts_with(&boundary).then_some((shorter, longer))
}

fn validate_volume_name(name: &str) -> Result<(), VolumeMountError> {
    if name.is_empty() {
        return Err(VolumeMountError::EmptyVolumeName);
    }
    let valid = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !name.starts_with(['.', '-']);
    if !valid {
        return Err(VolumeMountError::InvalidVolumeName(name.to_string()));
    }
    Ok(())
}

fn validate_mount_path(path: &str) -> Result<(), VolumeMountError> {
    if !path.starts_with('/') {
        return Err(VolumeMountError::RelativePath(path.to_string()));
    }
    // Trailing slashes are normalized away rather than rejected, so
    // `/shared` and `/shared/` can't be smuggled past the duplicate check
    // as two distinct paths.
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(VolumeMountError::ReservedPath(path.to_string()));
    }
    for segment in trimmed.trim_start_matches('/').split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(VolumeMountError::UnnormalizedPath(path.to_string()));
        }
    }
    if RESERVED_MOUNT_PATHS.contains(&trimmed) {
        return Err(VolumeMountError::ReservedPath(path.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(volume: &str, path: &str) -> VolumeMount {
        VolumeMount {
            volume: volume.into(),
            mount_path: path.into(),
            driver: VolumeDriver::Local,
            read_only: false,
        }
    }

    fn s3(volume: &str, path: &str) -> VolumeMount {
        VolumeMount {
            volume: volume.into(),
            mount_path: path.into(),
            driver: VolumeDriver::S3 {
                bucket: "assets".into(),
                prefix: None,
                credential_ref: "cred_1".into(),
            },
            read_only: true,
        }
    }

    #[test]
    fn accepts_the_motivating_case_workspace_local_plus_shared_s3() {
        // The whole point of per-mount drivers: a POSIX working tree and an
        // object-store dataset on the same sandbox.
        let mounts = vec![local("app-7", SANDBOX_WORK_DIR), s3("datasets", "/shared")];
        assert!(validate_mounts(&mounts).is_ok());
    }

    #[test]
    fn accepts_no_mounts() {
        // Empty preserves today's bind-mounted behavior.
        assert!(validate_mounts(&[]).is_ok());
    }

    #[test]
    fn rejects_two_volumes_at_the_same_path() {
        let mounts = vec![local("a", "/shared"), local("b", "/shared")];
        assert_eq!(
            validate_mounts(&mounts),
            Err(VolumeMountError::DuplicatePath("/shared".into()))
        );
    }

    #[test]
    fn trailing_slash_does_not_smuggle_a_duplicate_past_the_check() {
        let mounts = vec![local("a", "/shared"), local("b", "/shared/")];
        assert!(
            validate_mounts(&mounts).is_err(),
            "'/shared' and '/shared/' are the same mount point"
        );
    }

    #[test]
    fn rejects_nested_mounts_in_either_order() {
        for mounts in [
            vec![local("a", "/shared"), local("b", "/shared/data")],
            vec![local("a", "/shared/data"), local("b", "/shared")],
        ] {
            assert!(
                matches!(
                    validate_mounts(&mounts),
                    Err(VolumeMountError::NestedPaths { .. })
                ),
                "nesting must be rejected regardless of declaration order"
            );
        }
    }

    #[test]
    fn sibling_paths_sharing_a_prefix_are_not_nested() {
        // `/shared-cache` is not inside `/shared`; a naive starts_with
        // would reject this valid pair.
        let mounts = vec![local("a", "/shared"), local("b", "/shared-cache")];
        assert!(validate_mounts(&mounts).is_ok());
    }

    #[test]
    fn rejects_reserved_system_paths() {
        for path in ["/", "/etc", "/proc", "/usr", "/dev", "/etc/"] {
            let mounts = vec![local("a", path)];
            assert!(
                matches!(
                    validate_mounts(&mounts),
                    Err(VolumeMountError::ReservedPath(_))
                ),
                "must reject {path}"
            );
        }
    }

    #[test]
    fn rejects_relative_and_unnormalized_paths() {
        assert!(matches!(
            validate_mounts(&[local("a", "shared")]),
            Err(VolumeMountError::RelativePath(_))
        ));
        for path in ["/a/../b", "/a/./b", "/a//b"] {
            assert!(
                matches!(
                    validate_mounts(&[local("a", path)]),
                    Err(VolumeMountError::UnnormalizedPath(_))
                ),
                "must reject {path}"
            );
        }
    }

    #[test]
    fn rejects_object_storage_at_the_work_dir() {
        // The failure this whole driver split exists to prevent.
        let err = validate_mounts(&[s3("assets", SANDBOX_WORK_DIR)]).unwrap_err();
        assert_eq!(err, VolumeMountError::WorkspaceNeedsPosix { driver: "s3" });
        // The message has to say what to do instead, not just "no".
        assert!(err.to_string().contains("local volume"));
    }

    #[test]
    fn allows_object_storage_anywhere_other_than_the_work_dir() {
        assert!(validate_mounts(&[s3("assets", "/shared")]).is_ok());
    }

    #[test]
    fn rejects_volume_names_that_are_empty_or_shell_unsafe() {
        for name in ["", "a b", "a/b", "a;rm -rf /", "-leading-dash", ".hidden"] {
            assert!(
                validate_mounts(&[local(name, "/shared")]).is_err(),
                "must reject volume name {name:?}"
            );
        }
        for name in ["app-7", "temps_wfrun_42", "vol.1"] {
            assert!(
                validate_mounts(&[local(name, "/shared")]).is_ok(),
                "should accept volume name {name:?}"
            );
        }
    }

    #[test]
    fn workspace_helper_targets_the_work_dir_with_a_posix_driver() {
        let mount = VolumeMount::workspace("app-7");
        assert!(mount.is_workspace());
        assert!(mount.driver.supports_posix_semantics());
        assert!(validate_mounts(&[mount]).is_ok());
    }
}
