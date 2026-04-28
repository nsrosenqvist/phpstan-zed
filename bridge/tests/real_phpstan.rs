//! End-to-end integration test that drives a *real* PHPStan invocation
//! against the fixture project under `tests/fixtures/sample/`.
//!
//! The test is **opt-in by environment**: it requires both `php` and a
//! PHPStan binary (a `phpstan.phar`, the `phpstan` shim, or anything pointed
//! at by `PHPSTAN_BIN`) to be available. When either is missing the test
//! prints a skip message and returns successfully so contributors without a
//! local PHP toolchain can still run `cargo test --workspace`.
//!
//! CI installs PHP + PHPStan and exports `PHPSTAN_BIN` so the test runs for
//! real on every PR.

use phpstan_lsp_bridge::phpstan::{
    AnalyseRequest, CliPhpStanRunner, PhpStanRunner, ProgressUpdate,
};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::sync::mpsc;

const PHPSTAN_BIN_ENV: &str = "PHPSTAN_BIN";
const PHP_BIN_ENV: &str = "PHP_BIN";

/// Resolve a binary by checking `<env_var>` first, then falling back to a
/// `which`-style PATH lookup. Returns `None` when nothing is found.
fn resolve_binary(env_var: &str, default_name: &str) -> Option<PathBuf> {
    if let Ok(explicit) = env::var(env_var)
        && !explicit.trim().is_empty()
    {
        return Some(PathBuf::from(explicit));
    }
    which_on_path(default_name)
}

fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample")
}

/// Quick sanity probe: confirm the resolved PHP can actually launch. Avoids
/// false positives where `PHP_BIN` points at a stale path.
fn php_is_runnable(php: &Path) -> bool {
    Command::new(php)
        .arg("-v")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skip with a printed reason. Returning `Ok` rather than panicking keeps
/// the test green for contributors without a PHP toolchain. CI sets up the
/// environment so the skip path is not taken there.
fn skip(reason: &str) {
    eprintln!("[real_phpstan] skipping: {reason}");
}

#[tokio::test]
async fn real_phpstan_reports_diagnostics_for_bug_fixture() {
    let Some(php) = resolve_binary(PHP_BIN_ENV, "php") else {
        skip("`php` not on PATH and PHP_BIN unset");
        return;
    };
    if !php_is_runnable(&php) {
        skip(&format!("php at {} is not runnable", php.display()));
        return;
    }
    let Some(phpstan) = resolve_binary(PHPSTAN_BIN_ENV, "phpstan") else {
        skip("`phpstan` not on PATH and PHPSTAN_BIN unset");
        return;
    };

    let fixture = fixture_root();
    let bug_file = fixture.join("src").join("Bug.php");
    assert!(
        bug_file.is_file(),
        "fixture missing: {}",
        bug_file.display()
    );

    let runner = CliPhpStanRunner::new();
    let output = runner
        .analyse(AnalyseRequest {
            php_path: &php,
            phpstan_path: &phpstan,
            working_directory: Some(&fixture),
            level: None,
            memory_limit: Some("512M"),
            config_path: Some(&fixture.join("phpstan.neon")),
            progress_tx: None,
        })
        .await
        .expect("phpstan invocation should succeed (non-zero exit on errors is normal)");

    // Project-wide analysis: PHPStan's `files` map keys may be absolute or
    // relative depending on the working directory. Match by path suffix to
    // stay robust across PHPStan versions.
    let bug_entry = output
        .files
        .iter()
        .find(|(path, _)| path.ends_with("src/Bug.php"))
        .map(|(_, f)| f);
    let messages = bug_entry.map(|f| f.messages.as_slice()).unwrap_or_default();
    assert!(
        !messages.is_empty(),
        "expected PHPStan to report at least one error for the bug fixture, \
         got totals={:?}, files={:?}, errors={:?}",
        output.totals,
        output.files.keys().collect::<Vec<_>>(),
        output.errors,
    );
    assert!(
        messages.iter().any(|m| m.line.is_some()),
        "expected at least one diagnostic with a line number; got {messages:?}"
    );
}

#[tokio::test]
async fn real_phpstan_clean_file_has_no_diagnostics() {
    let Some(php) = resolve_binary(PHP_BIN_ENV, "php") else {
        skip("`php` not on PATH and PHP_BIN unset");
        return;
    };
    if !php_is_runnable(&php) {
        skip(&format!("php at {} is not runnable", php.display()));
        return;
    }
    let Some(phpstan) = resolve_binary(PHPSTAN_BIN_ENV, "phpstan") else {
        skip("`phpstan` not on PATH and PHPSTAN_BIN unset");
        return;
    };

    let fixture = fixture_root();

    let runner = CliPhpStanRunner::new();
    let output = runner
        .analyse(AnalyseRequest {
            php_path: &php,
            phpstan_path: &phpstan,
            working_directory: Some(&fixture),
            level: None,
            memory_limit: Some("512M"),
            config_path: Some(&fixture.join("phpstan.neon")),
            progress_tx: None,
        })
        .await
        .expect("phpstan invocation should succeed");

    // The clean fixture must not contribute any diagnostics. (The bug
    // fixture in the same project will, but we don't assert on that here.)
    let clean_messages: Vec<_> = output
        .files
        .iter()
        .filter(|(path, _)| path.ends_with("src/Clean.php"))
        .flat_map(|(_, f)| f.messages.iter())
        .collect();
    assert!(
        clean_messages.is_empty(),
        "expected no diagnostics for the clean fixture, got: {clean_messages:?}"
    );
}

/// Spawn a real PHPStan with `progress_tx` attached and assert that we
/// observe at least one fully-formed [`ProgressUpdate`]. PHPStan emits its
/// progress bar on stderr using ANSI cursor-control sequences in non-TTY
/// mode; this test guards against regressions in our stderr parser.
#[tokio::test]
async fn real_phpstan_streams_progress_updates() {
    let Some(php) = resolve_binary(PHP_BIN_ENV, "php") else {
        skip("`php` not on PATH and PHP_BIN unset");
        return;
    };
    if !php_is_runnable(&php) {
        skip(&format!("php at {} is not runnable", php.display()));
        return;
    }
    let Some(phpstan) = resolve_binary(PHPSTAN_BIN_ENV, "phpstan") else {
        skip("`phpstan` not on PATH and PHPSTAN_BIN unset");
        return;
    };

    let fixture = fixture_root();
    let (tx, mut rx) = mpsc::channel::<ProgressUpdate>(64);
    let collector = tokio::spawn(async move {
        let mut updates = Vec::new();
        while let Some(u) = rx.recv().await {
            updates.push(u);
        }
        updates
    });

    let runner = CliPhpStanRunner::new();
    let _ = runner
        .analyse(AnalyseRequest {
            php_path: &php,
            phpstan_path: &phpstan,
            working_directory: Some(&fixture),
            level: None,
            memory_limit: Some("512M"),
            config_path: Some(&fixture.join("phpstan.neon")),
            progress_tx: Some(tx),
        })
        .await
        .expect("phpstan invocation should succeed");

    let updates = collector.await.expect("collector task");
    assert!(
        !updates.is_empty(),
        "expected at least one progress update from real PHPStan; got none. \
         This usually means the stderr parser failed to recognise PHPStan's \
         ANSI-escaped progress frames."
    );
    let last = updates.last().expect("non-empty");
    assert!(last.total > 0, "total must be positive: {updates:?}");
    assert!(
        last.done <= last.total,
        "done must not exceed total: {updates:?}"
    );
}
