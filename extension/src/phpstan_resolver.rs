//! Four-tier resolution strategy for the PHPStan binary itself.
//!
//! Order of precedence:
//! 1. `vendor/bin/phpstan` inside the worktree (Composer install).
//! 2. `phpstan` discoverable on `$PATH`.
//! 3. A user-pinned version downloaded from PHPStan's GitHub releases.
//! 4. The latest stable release downloaded from GitHub.
//!
//! The cached path is reused across invocations so we never download twice in
//! the same Zed session.

use std::fs;
use std::path::{Path, PathBuf};
use zed_extension_api::{
    self as zed, DownloadedFileType, GithubReleaseAsset, GithubReleaseOptions, LanguageServerId,
    Result, Worktree,
};

use crate::cache::{UPDATE_TTL_SECS, UpdateManifest, now_unix_secs, read_manifest, write_manifest};

/// File name of the phar artifact published by PHPStan's GitHub releases.
const PHAR_ASSET_NAME: &str = "phpstan.phar";

/// Owner/repo string for PHPStan releases.
const PHPSTAN_REPO: &str = "phpstan/phpstan";

/// Sentinel value (case-insensitive) that the user can supply for
/// `phpstan.pinnedVersion` to explicitly request the latest stable release.
/// This is equivalent to omitting the setting or setting it to `null`.
const LATEST_SENTINEL: &str = "latest";

/// Normalise a user-provided version string. Empty strings, whitespace, and
/// the [`LATEST_SENTINEL`] all collapse to `None` (== "latest stable").
fn normalize_version(version: Option<String>) -> Option<String> {
    let v = version?;
    let trimmed = v.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(LATEST_SENTINEL) {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub struct PhpStanResolver {
    cached_phar_path: Option<String>,
    /// Optional pinned version (e.g. "1.11.0"). When `Some`, the resolver
    /// downloads `phpstan.phar` from the matching GitHub release tag instead
    /// of consulting `latest_github_release`.
    pinned_version: Option<String>,
}

impl PhpStanResolver {
    pub fn new() -> Self {
        Self {
            cached_phar_path: None,
            pinned_version: None,
        }
    }

    /// Replace the pinned version. Passing `None`, an empty string, or
    /// `"latest"` (case-insensitive) reverts to the latest stable release.
    /// Used both by the consumer (driven by user settings) and by tests.
    pub fn set_pinned_version(&mut self, version: Option<String>) {
        let normalized = normalize_version(version);
        // Invalidate the cached download whenever the pinned version changes
        // so we re-resolve against the new tag.
        if self.pinned_version != normalized {
            self.cached_phar_path = None;
        }
        self.pinned_version = normalized;
    }

    /// Override the pinned version (builder form, used in tests).
    #[allow(dead_code)]
    pub fn with_pinned_version(mut self, version: impl Into<String>) -> Self {
        self.set_pinned_version(Some(version.into()));
        self
    }

    pub fn resolve(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<String> {
        // Tier 1: vendor/bin/phpstan
        let vendor = vendor_phpstan_path(&worktree.root_path());
        if fs::metadata(&vendor).map(|m| m.is_file()).unwrap_or(false) {
            return Ok(vendor);
        }

        // Tier 2: PATH lookup
        if let Some(path) = worktree.which("phpstan") {
            return Ok(path);
        }

        // Tier 3 & 4: download (with TTL-based update check).
        if let Some(cached) = self.cached_phar_path.as_deref()
            && fs::metadata(cached).map(|m| m.is_file()).unwrap_or(false)
        {
            return Ok(cached.to_string());
        }

        let path = self.ensure_phar(language_server_id)?;
        self.cached_phar_path = Some(path.clone());
        Ok(path)
    }

    /// Resolve (and download if necessary) the PHPStan phar, using the
    /// on-disk update manifest to avoid hitting GitHub on every cold start.
    ///
    /// Behaviour:
    /// - **Pinned version**: if `phpstan-<version>/phpstan.phar` already
    ///   exists, reuse it with no network call. Otherwise download from the
    ///   deterministic release URL.
    /// - **Latest**: if any `phpstan-*` directory has a fresh manifest and a
    ///   present phar, reuse it. Otherwise call `latest_github_release`. If
    ///   the resolved version matches an existing on-disk version, just
    ///   refresh the manifest's `checked_at`. Only download when the version
    ///   actually changes.
    fn ensure_phar(&self, language_server_id: &LanguageServerId) -> Result<String> {
        if let Some(version) = self.pinned_version.as_deref() {
            return self.ensure_pinned_phar(language_server_id, version);
        }
        self.ensure_latest_phar(language_server_id)
    }

    fn ensure_pinned_phar(
        &self,
        language_server_id: &LanguageServerId,
        version: &str,
    ) -> Result<String> {
        let version_dir = format!("phpstan-{version}");
        let phar_path = format!("{version_dir}/{PHAR_ASSET_NAME}");
        if fs::metadata(&phar_path)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            return Ok(phar_path);
        }

        let download_url = format!(
            "https://github.com/{PHPSTAN_REPO}/releases/download/{version}/{PHAR_ASSET_NAME}"
        );
        download_phar_to(language_server_id, &download_url, &version_dir)?;
        // Pinned versions don't need TTL bookkeeping but writing a manifest
        // keeps the on-disk layout uniform with the "latest" path.
        let _ = write_manifest(
            Path::new(&version_dir),
            &UpdateManifest::new(version, now_unix_secs()),
        );
        cleanup_old_versions(&version_dir);
        Ok(phar_path)
    }

    fn ensure_latest_phar(&self, language_server_id: &LanguageServerId) -> Result<String> {
        // Disk fast path: any existing `phpstan-*` dir whose manifest is
        // still fresh and whose phar is present.
        if let Some(path) = find_fresh_cached_phar(Path::new("."), now_unix_secs(), UPDATE_TTL_SECS)
        {
            return Ok(path);
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );
        let release = zed::latest_github_release(
            PHPSTAN_REPO,
            GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )
        .map_err(|e| {
            format!(
                "Could not find or download PHPStan: {e}. Install it via Composer \
                 (composer require --dev phpstan/phpstan) or ensure phpstan is in your PATH."
            )
        })?;

        let version_dir = format!("phpstan-{}", release.version);
        let phar_path = format!("{version_dir}/{PHAR_ASSET_NAME}");

        // Latest matches what we already have on disk: just refresh the
        // manifest's `checked_at` and skip the download.
        if fs::metadata(&phar_path)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            let _ = write_manifest(
                Path::new(&version_dir),
                &UpdateManifest::new(&release.version, now_unix_secs()),
            );
            cleanup_old_versions(&version_dir);
            return Ok(phar_path);
        }

        let asset = find_asset(&release.assets, PHAR_ASSET_NAME).ok_or_else(|| {
            format!(
                "PHPStan release {} has no '{PHAR_ASSET_NAME}' asset",
                release.version
            )
        })?;
        download_phar_to(language_server_id, &asset.download_url, &version_dir)?;
        let _ = write_manifest(
            Path::new(&version_dir),
            &UpdateManifest::new(&release.version, now_unix_secs()),
        );
        cleanup_old_versions(&version_dir);
        Ok(phar_path)
    }
}

fn download_phar_to(
    language_server_id: &LanguageServerId,
    download_url: &str,
    version_dir: &str,
) -> Result<()> {
    let phar_path = format!("{version_dir}/{PHAR_ASSET_NAME}");
    // Zed's `download_file` writes the response straight to disk without
    // creating parent directories for `Uncompressed` assets, so the version
    // folder must exist beforehand.
    fs::create_dir_all(version_dir)
        .map_err(|e| format!("Could not create PHPStan cache directory '{version_dir}': {e}"))?;
    zed::set_language_server_installation_status(
        language_server_id,
        &zed::LanguageServerInstallationStatus::Downloading,
    );
    zed::download_file(download_url, &phar_path, DownloadedFileType::Uncompressed).map_err(
        |e| {
            format!(
                "Could not find or download PHPStan: {e}. Install it via Composer \
                 (composer require --dev phpstan/phpstan) or ensure phpstan is in your PATH."
            )
        },
    )?;
    // The phar is invoked via `php phpstan.phar`; on POSIX systems making
    // it executable is harmless and keeps direct invocation working too.
    let _ = zed::make_file_executable(&phar_path);
    Ok(())
}

/// Scan `root` for any `phpstan-<version>/` folder containing both a phar
/// and a fresh update manifest. Returns the relative path to the phar if a
/// match is found.
fn find_fresh_cached_phar(root: &Path, now: u64, ttl_secs: u64) -> Option<String> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with("phpstan-") || name.starts_with("phpstan-lsp-bridge-") {
            continue;
        }
        let dir = entry.path();
        let phar = dir.join(PHAR_ASSET_NAME);
        if !phar.is_file() {
            continue;
        }
        let Some(manifest) = read_manifest(&dir) else {
            continue;
        };
        if !manifest.is_fresh_at(now, ttl_secs) {
            continue;
        }
        // Returned as a relative path so production callers (which run from
        // the extension's working directory) can use it directly.
        return Some(format!("{name}/{PHAR_ASSET_NAME}"));
    }
    None
}

pub(crate) fn vendor_phpstan_path(root: &str) -> String {
    let mut p = PathBuf::from(root);
    p.push("vendor");
    p.push("bin");
    p.push("phpstan");
    p.to_string_lossy().into_owned()
}

fn find_asset<'a>(assets: &'a [GithubReleaseAsset], name: &str) -> Option<&'a GithubReleaseAsset> {
    assets.iter().find(|a| a.name == name)
}

fn cleanup_old_versions(keep_dir: &str) {
    let Ok(entries) = fs::read_dir(".") else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        // Only touch our own version directories; the bridge resolver owns
        // anything starting with `phpstan-lsp-bridge-`.
        if name.starts_with("phpstan-")
            && !name.starts_with("phpstan-lsp-bridge-")
            && name != keep_dir
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_path_is_relative_to_root() {
        let path = vendor_phpstan_path("/Users/me/proj");
        // Use forward slashes regardless of host OS in the comparison so the
        // test stays portable.
        let normalized = path.replace('\\', "/");
        assert_eq!(normalized, "/Users/me/proj/vendor/bin/phpstan");
    }

    #[test]
    fn vendor_path_handles_trailing_slash() {
        let path = vendor_phpstan_path("/proj/");
        let normalized = path.replace('\\', "/");
        assert!(normalized.ends_with("/vendor/bin/phpstan"));
    }

    #[test]
    fn set_pinned_version_invalidates_cache_when_changed() {
        let mut r = PhpStanResolver::new();
        r.cached_phar_path = Some("/tmp/old".to_string());
        r.set_pinned_version(Some("1.11.0".to_string()));
        assert!(r.cached_phar_path.is_none());
    }

    #[test]
    fn set_pinned_version_keeps_cache_when_unchanged() {
        let mut r = PhpStanResolver::new().with_pinned_version("1.11.0");
        r.cached_phar_path = Some("/tmp/cached".to_string());
        r.set_pinned_version(Some("1.11.0".to_string()));
        assert_eq!(r.cached_phar_path.as_deref(), Some("/tmp/cached"));
    }

    #[test]
    fn latest_sentinel_is_normalised_to_none() {
        for raw in ["latest", "LATEST", "  Latest  ", ""] {
            assert_eq!(
                normalize_version(Some(raw.to_string())),
                None,
                "expected '{raw}' to normalise to None"
            );
        }
        assert_eq!(normalize_version(None), None);
    }

    #[test]
    fn concrete_version_is_preserved_and_trimmed() {
        assert_eq!(
            normalize_version(Some("  1.11.0 ".to_string())),
            Some("1.11.0".to_string())
        );
    }

    #[test]
    fn switching_from_pinned_to_latest_invalidates_cache() {
        let mut r = PhpStanResolver::new().with_pinned_version("1.11.0");
        r.cached_phar_path = Some("/tmp/pinned".to_string());
        r.set_pinned_version(Some("latest".to_string()));
        assert!(r.cached_phar_path.is_none());
        assert!(r.pinned_version.is_none());
    }

    fn temp_root(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "phpstan-zed-resolver-{label}-{}-{}",
            std::process::id(),
            now_unix_secs()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn seed_phar(root: &Path, version: &str, manifest: Option<&UpdateManifest>) {
        let dir = root.join(format!("phpstan-{version}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(PHAR_ASSET_NAME), b"phar").unwrap();
        if let Some(m) = manifest {
            write_manifest(&dir, m).unwrap();
        }
    }

    #[test]
    fn fresh_cache_is_discovered() {
        let root = temp_root("fresh");
        let now = 10_000;
        seed_phar(
            &root,
            "1.11.0",
            Some(&UpdateManifest::new("1.11.0", now - 100)),
        );
        let hit = find_fresh_cached_phar(&root, now, UPDATE_TTL_SECS).expect("hit");
        assert_eq!(hit, format!("phpstan-1.11.0/{PHAR_ASSET_NAME}"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_cache_is_ignored() {
        let root = temp_root("stale");
        let now = 1_000_000;
        seed_phar(&root, "1.10.0", Some(&UpdateManifest::new("1.10.0", 1)));
        assert!(find_fresh_cached_phar(&root, now, UPDATE_TTL_SECS).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cache_without_manifest_is_ignored() {
        let root = temp_root("nomanifest");
        seed_phar(&root, "1.11.0", None);
        assert!(find_fresh_cached_phar(&root, 0, UPDATE_TTL_SECS).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bridge_directories_are_skipped() {
        let root = temp_root("bridge");
        let dir = root.join("phpstan-lsp-bridge-1.0.0");
        fs::create_dir_all(&dir).unwrap();
        // Even if a bridge dir somehow contained a `phpstan.phar` it must not
        // be returned by the phpstan resolver.
        fs::write(dir.join(PHAR_ASSET_NAME), b"phar").unwrap();
        write_manifest(&dir, &UpdateManifest::new("1.0.0", 0)).unwrap();
        assert!(find_fresh_cached_phar(&root, 0, UPDATE_TTL_SECS).is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
