//! On-disk update manifest used to throttle GitHub release lookups.
//!
//! Both the PHPStan phar resolver and the bridge resolver download release
//! artifacts from GitHub. Without a cache, every cold start of the extension
//! would call `latest_github_release` and `download_file`. We keep a small
//! JSON manifest next to the downloaded binary recording the resolved
//! version and a Unix timestamp of when we last checked the remote. When the
//! manifest is younger than [`UPDATE_TTL_SECS`] the resolver short-circuits
//! and reuses the existing on-disk binary without contacting GitHub.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// File name written inside each `<artifact>-<version>/` directory.
pub const MANIFEST_FILENAME: &str = ".update-manifest.json";

/// How long a successful "is this still the latest?" check is trusted before
/// we ask GitHub again. 24 hours strikes a reasonable balance: users that
/// keep Zed open all day pay zero network cost; those that restart it pay
/// at most one extra API call per day.
pub const UPDATE_TTL_SECS: u64 = 24 * 60 * 60;

/// Persisted form of an update check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateManifest {
    /// The version string returned by the GitHub release at `checked_at`.
    pub version: String,
    /// Unix timestamp (seconds since epoch) of the most recent check.
    pub checked_at: u64,
}

impl UpdateManifest {
    pub fn new(version: impl Into<String>, checked_at: u64) -> Self {
        Self {
            version: version.into(),
            checked_at,
        }
    }

    /// `true` when the manifest was written within `ttl_secs` seconds of `now`.
    /// Future-dated manifests (clock skew) are also considered fresh — better
    /// to over-trust the cache than to thrash the network.
    pub fn is_fresh_at(&self, now: u64, ttl_secs: u64) -> bool {
        match now.checked_sub(self.checked_at) {
            Some(age) => age <= ttl_secs,
            None => true,
        }
    }
}

/// Current Unix time in seconds. Falls back to 0 if the host clock is
/// unavailable; that has the effect of marking every cache stale, which is
/// safe.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read the manifest stored under `dir/`. Returns `None` if the file is
/// missing or malformed.
pub fn read_manifest(dir: &Path) -> Option<UpdateManifest> {
    let bytes = fs::read(dir.join(MANIFEST_FILENAME)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write the manifest into `dir/`. Errors are returned so callers can decide
/// whether to surface them; in practice the resolver logs and ignores write
/// failures because they only degrade the cache to the previous behaviour.
pub fn write_manifest(dir: &Path, manifest: &UpdateManifest) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fs::create_dir_all(dir)?;
    fs::write(dir.join(MANIFEST_FILENAME), bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "phpstan-zed-cache-{label}-{}",
            now_unix_secs().wrapping_add(rand_seed())
        ));
        p
    }

    fn rand_seed() -> u64 {
        // Cheap process-local entropy without pulling in a dependency.
        std::process::id() as u64
    }

    #[test]
    fn manifest_is_fresh_within_ttl() {
        let m = UpdateManifest::new("1.11.0", 1_000);
        assert!(m.is_fresh_at(1_500, 1_000));
        assert!(m.is_fresh_at(2_000, 1_000));
    }

    #[test]
    fn manifest_is_stale_beyond_ttl() {
        let m = UpdateManifest::new("1.11.0", 1_000);
        assert!(!m.is_fresh_at(2_001, 1_000));
    }

    #[test]
    fn future_dated_manifest_is_treated_as_fresh() {
        let m = UpdateManifest::new("1.11.0", 5_000);
        assert!(m.is_fresh_at(1_000, 60));
    }

    #[test]
    fn read_returns_none_when_missing() {
        let dir = temp_dir("missing");
        assert!(read_manifest(&dir).is_none());
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = temp_dir("roundtrip");
        let m = UpdateManifest::new("2.0.0", 1_700_000_000);
        write_manifest(&dir, &m).expect("write");
        assert_eq!(read_manifest(&dir).expect("read"), m);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_returns_none_when_malformed() {
        let dir = temp_dir("malformed");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST_FILENAME), b"not json").unwrap();
        assert!(read_manifest(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
