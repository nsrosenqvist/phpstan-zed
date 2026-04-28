//! Resolves the LSP bridge binary and assembles the language-server command.
//!
//! Responsibilities:
//! 1. Find an existing `phpstan-lsp-bridge` on `$PATH` (developer convenience).
//! 2. Cache a previously downloaded bridge across language-server restarts.
//! 3. Otherwise, download the platform-specific release asset from GitHub.
//! 4. Resolve the PHP interpreter and PHPStan binary, then assemble a
//!    [`zed::Command`].

use crate::cache::{UPDATE_TTL_SECS, UpdateManifest, now_unix_secs, read_manifest, write_manifest};
use crate::phpstan_resolver::PhpStanResolver;
use crate::settings::PhpStanSettings;
use std::fs;
use std::path::Path;
use zed_extension_api::{
    self as zed, Architecture, DownloadedFileType, GithubReleaseAsset, GithubReleaseOptions,
    LanguageServerId, Os, Result, Worktree,
};

/// Convert a path that the resolvers returned (which is typically relative
/// to the extension's work directory) into an absolute path. Subprocesses
/// spawned by the bridge run with the project root as their CWD, so any
/// relative path would be resolved against the wrong directory.
fn absolutize(path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        return path.to_string();
    }
    match std::env::current_dir() {
        Ok(cwd) => {
            let mut full = cwd;
            full.push(p);
            full.to_string_lossy().into_owned()
        }
        Err(_) => path.to_string(),
    }
}

/// Fully owns the bridge binary and the PHPStan resolver. Lifetime spans the
/// host extension instance.
pub struct PhpStanServer {
    cached_bridge_path: Option<String>,
    phpstan_resolver: PhpStanResolver,
}

impl PhpStanServer {
    pub const LANGUAGE_SERVER_ID: &'static str = "phpstan";

    /// GitHub repository hosting the bridge release artifacts.
    const BRIDGE_RELEASE_REPO: &'static str = "nsrosenqvist/phpstan-zed";

    /// Binary name (without platform suffix) of the bridge executable.
    const BRIDGE_BINARY: &'static str = "phpstan-lsp-bridge";

    pub fn new() -> Self {
        Self {
            cached_bridge_path: None,
            phpstan_resolver: PhpStanResolver::new(),
        }
    }

    pub fn command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<zed::Command> {
        let settings = PhpStanSettings::load(language_server_id, worktree);

        let bridge_path = self.resolve_bridge(language_server_id, worktree)?;
        self.phpstan_resolver
            .set_pinned_version(settings.pinned_version.clone());
        let phpstan_path = self
            .phpstan_resolver
            .resolve(language_server_id, worktree)?;
        let php_path = resolve_php(worktree)?;

        // Subprocesses launched by the bridge inherit the project root as
        // their CWD. Relative paths returned from our resolvers (which live
        // under the extension work dir) must be promoted to absolute paths
        // before they cross that boundary.
        let bridge_path = absolutize(&bridge_path);
        let phpstan_path = absolutize(&phpstan_path);

        let args = build_bridge_args(&phpstan_path, &php_path, worktree.root_path(), &settings);

        Ok(zed::Command {
            command: bridge_path,
            args,
            env: Default::default(),
        })
    }

    fn resolve_bridge(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<String> {
        if let Some(path) = worktree.which(Self::BRIDGE_BINARY) {
            return Ok(path);
        }

        if let Some(cached) = self.cached_bridge_path.as_deref()
            && fs::metadata(cached).map(|m| m.is_file()).unwrap_or(false)
        {
            return Ok(cached.to_string());
        }

        let path = self.ensure_bridge(language_server_id)?;
        self.cached_bridge_path = Some(path.clone());
        Ok(path)
    }

    /// Resolve (and download if necessary) the bridge binary, throttling
    /// GitHub release lookups via the on-disk update manifest. Mirrors the
    /// behaviour of [`PhpStanResolver::ensure_latest_phar`]: a fresh manifest
    /// short-circuits the network entirely; a stale manifest still skips the
    /// download when the resolved version matches the on-disk binary.
    fn ensure_bridge(&self, language_server_id: &LanguageServerId) -> Result<String> {
        let (os, _arch) = zed::current_platform();
        let asset_name = bridge_asset_name(os, _arch);

        if let Some(path) = find_fresh_cached_bridge(
            Path::new("."),
            os,
            &asset_name,
            now_unix_secs(),
            UPDATE_TTL_SECS,
        ) {
            return Ok(path);
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );
        let release = zed::latest_github_release(
            Self::BRIDGE_RELEASE_REPO,
            GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )
        .map_err(|e| format!("failed to fetch latest bridge release: {e}"))?;

        let version_dir = format!("phpstan-lsp-bridge-{}", release.version);
        let final_path = bridge_binary_path(&version_dir, &asset_name, os);

        // Resolved version already on disk: refresh manifest and skip
        // download.
        if fs::metadata(&final_path)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            let _ = write_manifest(
                Path::new(&version_dir),
                &UpdateManifest::new(&release.version, now_unix_secs()),
            );
            cleanup_old_versions(&version_dir);
            return Ok(final_path);
        }

        let asset = find_asset(&release.assets, &asset_name).ok_or_else(|| {
            format!(
                "bridge release {} has no asset named '{asset_name}'",
                release.version
            )
        })?;

        let download_path = format!("{version_dir}/{asset_name}");
        // Ensure the version directory exists; Zed's `download_file` does
        // not create parent directories for `Uncompressed` assets and is
        // not guaranteed to do so for archive assets either.
        fs::create_dir_all(&version_dir)
            .map_err(|e| format!("failed to create bridge cache dir '{version_dir}': {e}"))?;
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );
        zed::download_file(
            &asset.download_url,
            &download_path,
            file_type_for_asset(&asset_name),
        )
        .map_err(|e| format!("failed to download bridge asset {asset_name}: {e}"))?;

        zed::make_file_executable(&final_path)
            .map_err(|e| format!("failed to mark bridge executable: {e}"))?;
        let _ = write_manifest(
            Path::new(&version_dir),
            &UpdateManifest::new(&release.version, now_unix_secs()),
        );
        cleanup_old_versions(&version_dir);
        Ok(final_path)
    }
}

/// Final on-disk path of the bridge binary inside `<version_dir>/`. For
/// `.tar.gz`/`.zip` archives the binary is extracted to a stable file name;
/// for an uncompressed asset the downloaded file *is* the binary.
fn bridge_binary_path(version_dir: &str, asset_name: &str, os: Os) -> String {
    if file_type_for_asset(asset_name) == DownloadedFileType::Uncompressed {
        format!("{version_dir}/{asset_name}")
    } else {
        format!(
            "{version_dir}/{}{}",
            PhpStanServer::BRIDGE_BINARY,
            if matches!(os, Os::Windows) {
                ".exe"
            } else {
                ""
            }
        )
    }
}

/// Disk fast-path for the bridge resolver. Returns the binary path inside
/// the freshest `phpstan-lsp-bridge-<version>/` directory whose manifest has
/// not yet expired.
fn find_fresh_cached_bridge(
    root: &Path,
    os: Os,
    asset_name: &str,
    now: u64,
    ttl_secs: u64,
) -> Option<String> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with("phpstan-lsp-bridge-") {
            continue;
        }
        let dir = entry.path();
        let relative_binary = bridge_binary_path(&name, asset_name, os);
        // Existence is checked relative to `root`; the returned path stays
        // relative because production callers operate from the extension's
        // working directory.
        let absolute_binary = root.join(&relative_binary);
        if !absolute_binary.is_file() {
            continue;
        }
        let Some(manifest) = read_manifest(&dir) else {
            continue;
        };
        if !manifest.is_fresh_at(now, ttl_secs) {
            continue;
        }
        return Some(relative_binary);
    }
    None
}

/// Public helper kept module-private intentionally; exposed for testing only.
pub(crate) fn bridge_asset_name(os: Os, arch: Architecture) -> String {
    let os_part = match os {
        Os::Mac => "apple-darwin",
        Os::Linux => "unknown-linux-gnu",
        Os::Windows => "pc-windows-msvc",
    };
    let arch_part = match arch {
        Architecture::Aarch64 => "aarch64",
        Architecture::X8664 => "x86_64",
        Architecture::X86 => "i686",
    };
    let ext = if matches!(os, Os::Windows) {
        ".zip"
    } else {
        ".tar.gz"
    };
    format!("phpstan-lsp-bridge-{arch_part}-{os_part}{ext}")
}

fn file_type_for_asset(asset_name: &str) -> DownloadedFileType {
    if asset_name.ends_with(".tar.gz") {
        DownloadedFileType::GzipTar
    } else if asset_name.ends_with(".gz") {
        DownloadedFileType::Gzip
    } else if asset_name.ends_with(".zip") {
        DownloadedFileType::Zip
    } else {
        DownloadedFileType::Uncompressed
    }
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
        if name.starts_with("phpstan-lsp-bridge-") && name != keep_dir {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Build the CLI invocation forwarded to the bridge. Pure function so the
/// argument shape is unit-testable without a worktree.
fn build_bridge_args(
    phpstan_path: &str,
    php_path: &str,
    working_directory: String,
    settings: &PhpStanSettings,
) -> Vec<String> {
    let mut args = vec![
        format!("--phpstan-path={phpstan_path}"),
        format!("--php-path={php_path}"),
        format!("--working-directory={working_directory}"),
        format!("--trigger={}", settings.diagnostic_trigger.as_cli()),
        format!("--memory-limit={}", settings.memory_limit.as_str()),
        format!("--show-progress={}", settings.show_progress),
    ];
    if let Some(level) = settings.level {
        args.push(format!("--level={level}"));
    }
    if let Some(cfg) = settings.config_path.as_deref() {
        args.push(format!("--config-path={cfg}"));
    }
    args
}

fn resolve_php(worktree: &Worktree) -> Result<String> {
    worktree.which("php").ok_or_else(|| {
        "PHPStan requires PHP 8.0+ to be installed and available in your PATH".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_asset_name_matrix() {
        assert_eq!(
            bridge_asset_name(Os::Mac, Architecture::Aarch64),
            "phpstan-lsp-bridge-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            bridge_asset_name(Os::Mac, Architecture::X8664),
            "phpstan-lsp-bridge-x86_64-apple-darwin.tar.gz"
        );
        assert_eq!(
            bridge_asset_name(Os::Linux, Architecture::X8664),
            "phpstan-lsp-bridge-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            bridge_asset_name(Os::Linux, Architecture::Aarch64),
            "phpstan-lsp-bridge-aarch64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            bridge_asset_name(Os::Windows, Architecture::X8664),
            "phpstan-lsp-bridge-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn file_type_dispatch_by_extension() {
        assert_eq!(
            file_type_for_asset("foo.tar.gz"),
            DownloadedFileType::GzipTar
        );
        assert_eq!(file_type_for_asset("foo.zip"), DownloadedFileType::Zip);
        assert_eq!(file_type_for_asset("foo.gz"), DownloadedFileType::Gzip);
        assert_eq!(file_type_for_asset("foo"), DownloadedFileType::Uncompressed);
    }

    #[test]
    fn build_bridge_args_includes_required_flags_with_defaults() {
        let settings = PhpStanSettings::default();
        let args = build_bridge_args(
            "/p/vendor/bin/phpstan",
            "/usr/bin/php",
            "/p".to_string(),
            &settings,
        );
        assert_eq!(
            args,
            vec![
                "--phpstan-path=/p/vendor/bin/phpstan".to_string(),
                "--php-path=/usr/bin/php".to_string(),
                "--working-directory=/p".to_string(),
                "--trigger=on-save".to_string(),
                "--memory-limit=512M".to_string(),
                "--show-progress=true".to_string(),
            ]
        );
    }

    #[test]
    fn build_bridge_args_appends_optional_flags_when_set() {
        let settings = PhpStanSettings {
            level: Some(7),
            memory_limit: crate::settings::MemoryLimit::new("1G"),
            config_path: Some("/p/phpstan.neon".to_string()),
            diagnostic_trigger: crate::settings::DiagnosticTrigger::OnChange,
            pinned_version: Some("1.11.0".to_string()),
            show_progress: false,
        };
        let args = build_bridge_args("/bin/phpstan", "/usr/bin/php", "/p".to_string(), &settings);
        assert!(args.contains(&"--trigger=on-change".to_string()));
        assert!(args.contains(&"--memory-limit=1G".to_string()));
        assert!(args.contains(&"--level=7".to_string()));
        assert!(args.contains(&"--config-path=/p/phpstan.neon".to_string()));
        assert!(args.contains(&"--show-progress=false".to_string()));
    }

    fn temp_root(label: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "phpstan-zed-bridge-{label}-{}-{}",
            std::process::id(),
            now_unix_secs()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn bridge_binary_path_uses_extracted_filename_for_archives() {
        let p = bridge_binary_path(
            "phpstan-lsp-bridge-1.0.0",
            "phpstan-lsp-bridge-x86_64-unknown-linux-gnu.tar.gz",
            Os::Linux,
        );
        assert_eq!(p, "phpstan-lsp-bridge-1.0.0/phpstan-lsp-bridge");
    }

    #[test]
    fn bridge_binary_path_uses_exe_suffix_on_windows() {
        let p = bridge_binary_path(
            "phpstan-lsp-bridge-1.0.0",
            "phpstan-lsp-bridge-x86_64-pc-windows-msvc.zip",
            Os::Windows,
        );
        assert_eq!(p, "phpstan-lsp-bridge-1.0.0/phpstan-lsp-bridge.exe");
    }

    #[test]
    fn fresh_bridge_cache_is_discovered() {
        let root = temp_root("fresh");
        let dir = root.join("phpstan-lsp-bridge-1.0.0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("phpstan-lsp-bridge"), b"binary").unwrap();
        write_manifest(&dir, &UpdateManifest::new("1.0.0", 9_900)).unwrap();
        let hit = find_fresh_cached_bridge(
            &root,
            Os::Linux,
            "phpstan-lsp-bridge-x86_64-unknown-linux-gnu.tar.gz",
            10_000,
            UPDATE_TTL_SECS,
        )
        .expect("hit");
        assert!(hit.ends_with("phpstan-lsp-bridge-1.0.0/phpstan-lsp-bridge"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_bridge_cache_is_ignored() {
        let root = temp_root("stale");
        let dir = root.join("phpstan-lsp-bridge-0.9.0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("phpstan-lsp-bridge"), b"binary").unwrap();
        write_manifest(&dir, &UpdateManifest::new("0.9.0", 0)).unwrap();
        assert!(
            find_fresh_cached_bridge(
                &root,
                Os::Linux,
                "phpstan-lsp-bridge-x86_64-unknown-linux-gnu.tar.gz",
                1_000_000,
                UPDATE_TTL_SECS,
            )
            .is_none()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bridge_cache_without_binary_is_ignored() {
        let root = temp_root("nobinary");
        let dir = root.join("phpstan-lsp-bridge-1.0.0");
        fs::create_dir_all(&dir).unwrap();
        write_manifest(&dir, &UpdateManifest::new("1.0.0", 0)).unwrap();
        assert!(
            find_fresh_cached_bridge(
                &root,
                Os::Linux,
                "phpstan-lsp-bridge-x86_64-unknown-linux-gnu.tar.gz",
                0,
                UPDATE_TTL_SECS,
            )
            .is_none()
        );
        let _ = fs::remove_dir_all(&root);
    }
}
