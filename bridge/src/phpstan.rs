//! Invocation of the PHPStan CLI and parsing of its `--error-format=json`
//! output.
//!
//! The runtime is hidden behind the [`PhpStanRunner`] trait so the LSP server
//! can be unit-tested with a fake implementation that returns canned JSON
//! without spawning a process.

use crate::error::{BridgeError, BridgeResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Top-level structure returned by `phpstan analyse --error-format=json`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PhpStanOutput {
    pub totals: PhpStanTotals,
    #[serde(default)]
    pub files: HashMap<String, PhpStanFileErrors>,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct PhpStanTotals {
    #[serde(default)]
    pub errors: u32,
    #[serde(default)]
    pub file_errors: u32,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PhpStanFileErrors {
    #[serde(default)]
    pub errors: u32,
    #[serde(default)]
    pub messages: Vec<PhpStanMessage>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PhpStanMessage {
    pub message: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub ignorable: bool,
    #[serde(default)]
    pub identifier: Option<String>,
    /// Optional rule tip; not all PHPStan versions emit it.
    #[serde(default)]
    pub tip: Option<String>,
}

impl PhpStanOutput {
    /// Parse JSON emitted by PHPStan, surfacing parse errors with the raw text
    /// preserved so the user (or the diagnostic publisher) can see what went
    /// wrong.
    pub fn from_json(raw: &str) -> BridgeResult<Self> {
        serde_json::from_str(raw).map_err(|source| BridgeError::InvalidJson {
            source,
            raw: raw.to_owned(),
        })
    }
}

/// Abstraction over the PHPStan CLI so the LSP server can be tested without
/// spawning real processes.
#[async_trait]
pub trait PhpStanRunner: Send + Sync {
    async fn analyse(&self, request: AnalyseRequest<'_>) -> BridgeResult<PhpStanOutput>;
}

/// All parameters required for one PHPStan invocation. The analysis is
/// **always project-wide**: PHPStan reads its `paths` from `phpstan.neon`
/// and uses its own result cache for incremental speed. Running on a single
/// file would defeat the cache and hide cross-file errors.
#[derive(Debug, Clone)]
pub struct AnalyseRequest<'a> {
    pub php_path: &'a Path,
    pub phpstan_path: &'a Path,
    pub working_directory: Option<&'a Path>,
    /// Override the `phpstan.neon` analysis level. `None` defers to the
    /// project configuration.
    pub level: Option<u8>,
    /// Optional `--memory-limit` value forwarded to PHPStan.
    pub memory_limit: Option<&'a str>,
    /// Optional `--configuration` path forwarded to PHPStan.
    pub config_path: Option<&'a Path>,
    /// When `Some`, the runner streams PHPStan's stderr progress bar and
    /// forwards parsed updates here. `--no-progress` is automatically
    /// **omitted** in this mode so PHPStan actually emits the bar.
    pub progress_tx: Option<mpsc::Sender<ProgressUpdate>>,
}

impl<'a> AnalyseRequest<'a> {
    /// Build a request without progress streaming. Equivalent to setting
    /// every required field manually with `progress_tx: None`.
    pub fn new(php_path: &'a Path, phpstan_path: &'a Path) -> Self {
        Self {
            php_path,
            phpstan_path,
            working_directory: None,
            level: None,
            memory_limit: None,
            config_path: None,
            progress_tx: None,
        }
    }
}

/// Snapshot of PHPStan's progress at a single point in time, parsed from the
/// `<done>/<total> [...] <percentage>%` line PHPStan writes to stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressUpdate {
    pub done: u32,
    pub total: u32,
    pub percentage: u8,
}

/// Production runner that actually spawns `php <phpstan> analyse ...`.
#[derive(Debug, Default, Clone)]
pub struct CliPhpStanRunner;

impl CliPhpStanRunner {
    pub fn new() -> Self {
        Self
    }

    fn build_command(req: &AnalyseRequest<'_>) -> Command {
        let mut cmd = Command::new(req.php_path);
        cmd.arg(req.phpstan_path)
            .arg("analyse")
            .arg("--error-format=json")
            .arg("--no-interaction");
        // Only suppress the progress bar when nobody asked for it. The
        // bar lands on stderr; in non-TTY mode PHPStan still prints a
        // `<done>/<total> [...] <percentage>%` frame for every file, which
        // is what `parse_progress_chunk` consumes.
        if req.progress_tx.is_none() {
            cmd.arg("--no-progress");
        }
        if let Some(level) = req.level {
            cmd.arg(format!("--level={level}"));
        }
        if let Some(limit) = req.memory_limit {
            cmd.arg(format!("--memory-limit={limit}"));
        }
        if let Some(cfg) = req.config_path {
            cmd.arg("--configuration").arg(cfg);
        }
        // No file argument: PHPStan reads `paths` from `phpstan.neon` and
        // analyses the whole project, leveraging its result cache.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = req.working_directory {
            cmd.current_dir(cwd);
        }
        cmd
    }
}

#[async_trait]
impl PhpStanRunner for CliPhpStanRunner {
    async fn analyse(&self, request: AnalyseRequest<'_>) -> BridgeResult<PhpStanOutput> {
        let php = PathBuf::from(request.php_path);
        let progress_tx = request.progress_tx.clone();
        let mut child = Self::build_command(&request)
            .spawn()
            .map_err(|source| BridgeError::Spawn { php, source })?;

        // Drain stderr concurrently. When a progress sink is attached, parse
        // chunks for `<done>/<total> ... <percentage>%` frames and forward
        // them. Either way we accumulate the full text so a crash can still
        // surface a meaningful error message.
        let stderr = child
            .stderr
            .take()
            .expect("stderr is piped via build_command");
        let stderr_task = tokio::spawn(drain_stderr(stderr, progress_tx));

        // Read stdout fully to a String. PHPStan emits the JSON document in
        // one go after analysis completes, so a simple `read_to_end` is
        // adequate (no streaming JSON parser needed).
        let mut stdout = child
            .stdout
            .take()
            .expect("stdout is piped via build_command");
        let mut stdout_buf = Vec::new();
        stdout
            .read_to_end(&mut stdout_buf)
            .await
            .map_err(|source| BridgeError::Spawn {
                php: PathBuf::from(request.php_path),
                source,
            })?;

        let status = child.wait().await.map_err(|source| BridgeError::Spawn {
            php: PathBuf::from(request.php_path),
            source,
        })?;

        // The stderr task always terminates once the child closes its end of
        // the pipe, so awaiting it here cannot deadlock.
        let stderr_text = stderr_task.await.unwrap_or_default();

        // PHPStan exits with a non-zero status when it finds errors; that's
        // the success path for us. Only treat it as a crash if there is no
        // stdout to parse.
        let stdout_str = String::from_utf8(stdout_buf)?;
        if stdout_str.trim().is_empty() {
            return Err(BridgeError::PhpStanCrashed {
                code: status.code(),
                stderr: stderr_text,
            });
        }

        PhpStanOutput::from_json(&stdout_str)
    }
}

/// Read PHPStan's stderr to completion, forwarding parsed progress frames
/// to `progress_tx` (when attached) and returning the full text for crash
/// reporting.
///
/// We read raw bytes rather than lines because PHPStan's progress bar uses
/// `\r` to overwrite the previous frame in place; with `read_line` each
/// frame would only surface after the *next* one arrived. Reading chunks
/// keeps the UI responsive at ~the same cadence PHPStan refreshes.
async fn drain_stderr<R>(stderr: R, progress_tx: Option<mpsc::Sender<ProgressUpdate>>) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stderr);
    let mut buf = [0u8; 4096];
    let mut accumulated = String::new();
    let mut last_emitted: Option<ProgressUpdate> = None;
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(error = %e, "stderr read failed; aborting drain");
                break;
            }
        };
        let chunk = String::from_utf8_lossy(&buf[..n]);
        if let Some(tx) = progress_tx.as_ref()
            && let Some(update) = parse_progress_chunk(&chunk)
            && Some(update) != last_emitted
        {
            // try_send so a slow consumer never blocks PHPStan; we'd
            // rather drop a frame than back up the pipe.
            let _ = tx.try_send(update);
            last_emitted = Some(update);
        }
        accumulated.push_str(&chunk);
    }
    accumulated
}

/// Parse the most recent `<done>/<total> [...] <percentage>%` frame in a
/// raw stderr chunk. PHPStan separates frames in three different ways
/// depending on the runtime:
///
/// * `\r` carriage returns (legacy TTY mode);
/// * ANSI cursor-control sequences such as `\x1b[1G\x1b[2K` (current
///   non-TTY default — what we hit when spawned from an LSP);
/// * plain `\n` newlines (rare).
///
/// To handle all three uniformly we first replace every ANSI CSI escape
/// with a newline, then split on `\r`/`\n` and return the last candidate
/// that parses cleanly.
///
/// Returns `None` when no frame is present (e.g. the chunk is just a
/// "Note: Using configuration file ..." line).
fn parse_progress_chunk(chunk: &str) -> Option<ProgressUpdate> {
    let normalised = normalise_progress_chunk(chunk);
    normalised
        .split(['\r', '\n'])
        .rev()
        .find_map(parse_progress_frame)
}

/// Replace every ANSI CSI escape (`ESC[<params><final>`) with a newline so
/// downstream code can treat them as frame separators. Non-ANSI bytes are
/// preserved verbatim.
fn normalise_progress_chunk(chunk: &str) -> String {
    let mut out = String::with_capacity(chunk.len());
    let mut iter = chunk.chars().peekable();
    while let Some(c) = iter.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // CSI sequences begin with `ESC[`. Anything else (e.g. a bare ESC)
        // we drop to be safe.
        if iter.peek() != Some(&'[') {
            continue;
        }
        iter.next(); // consume `[`
        // Skip parameter and intermediate bytes, then a single final byte
        // in the range 0x40..=0x7E.
        for cc in iter.by_ref() {
            if ('\x40'..='\x7e').contains(&cc) {
                break;
            }
        }
        out.push('\n');
    }
    out
}

/// Parse a single `<done>/<total> [...] <percentage>%` frame. The bracketed
/// progress-bar segment is intentionally ignored — its contents are
/// cosmetic and vary across PHPStan releases.
fn parse_progress_frame(frame: &str) -> Option<ProgressUpdate> {
    let trimmed = frame.trim();
    let after_pct = trimmed.strip_suffix('%')?;
    // Walk back over the percentage digits.
    let pct_start = after_pct
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    let percentage: u8 = after_pct[pct_start..].parse().ok()?;
    if percentage > 100 {
        return None;
    }
    // Leading `<done>/<total>` is delimited by the first whitespace.
    let head = trimmed.split_whitespace().next()?;
    let (done_str, total_str) = head.split_once('/')?;
    let done: u32 = done_str.parse().ok()?;
    let total: u32 = total_str.parse().ok()?;
    if total == 0 {
        return None;
    }
    Some(ProgressUpdate {
        done,
        total,
        percentage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_JSON: &str = r#"{
        "totals": { "errors": 0, "file_errors": 2 },
        "files": {
            "/proj/src/Foo.php": {
                "errors": 2,
                "messages": [
                    {
                        "message": "Undefined variable: $bar",
                        "line": 7,
                        "ignorable": true,
                        "identifier": "variable.undefined"
                    },
                    {
                        "message": "Method has no return type",
                        "line": 12,
                        "ignorable": true,
                        "identifier": "missingType.return",
                        "tip": "Add ': void'"
                    }
                ]
            }
        },
        "errors": []
    }"#;

    #[test]
    fn parses_phpstan_output() {
        let parsed = PhpStanOutput::from_json(SAMPLE_JSON).expect("parse");
        assert_eq!(parsed.totals.file_errors, 2);
        let file = parsed.files.get("/proj/src/Foo.php").expect("file entry");
        assert_eq!(file.messages.len(), 2);
        assert_eq!(file.messages[0].line, Some(7));
        assert_eq!(
            file.messages[0].identifier.as_deref(),
            Some("variable.undefined")
        );
        assert_eq!(file.messages[1].tip.as_deref(), Some("Add ': void'"));
    }

    #[test]
    fn empty_files_object_is_allowed() {
        let json = r#"{"totals":{"errors":0,"file_errors":0},"files":{},"errors":[]}"#;
        let parsed = PhpStanOutput::from_json(json).unwrap();
        assert!(parsed.files.is_empty());
    }

    #[test]
    fn invalid_json_preserves_raw() {
        let err = PhpStanOutput::from_json("not json").unwrap_err();
        match err {
            BridgeError::InvalidJson { raw, .. } => assert_eq!(raw, "not json"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn build_command_includes_required_flags() {
        let req = AnalyseRequest {
            php_path: Path::new("/usr/bin/php"),
            phpstan_path: Path::new("/p/vendor/bin/phpstan"),
            working_directory: Some(Path::new("/p")),
            level: None,
            memory_limit: None,
            config_path: None,
            progress_tx: None,
        };
        let cmd = CliPhpStanRunner::build_command(&req);
        let std_cmd = cmd.as_std();
        let args: Vec<&std::ffi::OsStr> = std_cmd.get_args().collect();
        assert_eq!(std_cmd.get_program(), "/usr/bin/php");
        assert_eq!(args[0], "/p/vendor/bin/phpstan");
        assert_eq!(args[1], "analyse");
        assert_eq!(args[2], "--error-format=json");
        assert_eq!(args[3], "--no-interaction");
        assert_eq!(args[4], "--no-progress");
        // No file argument: the analysis is project-wide.
        assert_eq!(args.len(), 5);
        assert_eq!(std_cmd.get_current_dir(), Some(Path::new("/p")));
    }

    #[test]
    fn build_command_appends_optional_analysis_flags_in_order() {
        let req = AnalyseRequest {
            php_path: Path::new("/usr/bin/php"),
            phpstan_path: Path::new("/p/vendor/bin/phpstan"),
            working_directory: None,
            level: Some(7),
            memory_limit: Some("1G"),
            config_path: Some(Path::new("/p/phpstan.neon")),
            progress_tx: None,
        };
        let cmd = CliPhpStanRunner::build_command(&req);
        let std_cmd = cmd.as_std();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "/p/vendor/bin/phpstan");
        assert_eq!(args[1], "analyse");
        assert_eq!(args[2], "--error-format=json");
        assert_eq!(args[3], "--no-interaction");
        assert_eq!(args[4], "--no-progress");
        assert_eq!(args[5], "--level=7");
        assert_eq!(args[6], "--memory-limit=1G");
        assert_eq!(args[7], "--configuration");
        assert_eq!(args[8], "/p/phpstan.neon");
        assert_eq!(args.len(), 9);
    }

    #[test]
    fn build_command_omits_unset_optional_flags() {
        let req = AnalyseRequest {
            php_path: Path::new("/usr/bin/php"),
            phpstan_path: Path::new("/p/phpstan"),
            working_directory: None,
            level: None,
            memory_limit: None,
            config_path: None,
            progress_tx: None,
        };
        let args: Vec<String> = CliPhpStanRunner::build_command(&req)
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|a| a.starts_with("--level")));
        assert!(!args.iter().any(|a| a.starts_with("--memory-limit")));
        assert!(!args.iter().any(|a| a == "--configuration"));
    }

    #[test]
    fn build_command_drops_no_progress_when_progress_tx_attached() {
        let (tx, _rx) = mpsc::channel(1);
        let req = AnalyseRequest {
            php_path: Path::new("/usr/bin/php"),
            phpstan_path: Path::new("/p/phpstan"),
            working_directory: None,
            level: None,
            memory_limit: None,
            config_path: None,
            progress_tx: Some(tx),
        };
        let args: Vec<String> = CliPhpStanRunner::build_command(&req)
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args.iter().any(|a| a == "--no-progress"),
            "stderr-streaming mode must not suppress the progress bar; got {args:?}"
        );
    }

    #[test]
    fn parse_progress_frame_extracts_done_total_and_percentage() {
        let u = parse_progress_frame(" 1234/5678 [===>          ]  21% ").unwrap();
        assert_eq!(u.done, 1234);
        assert_eq!(u.total, 5678);
        assert_eq!(u.percentage, 21);
    }

    #[test]
    fn parse_progress_frame_handles_completion() {
        let u = parse_progress_frame("28290/28290 [============================] 100%").unwrap();
        assert_eq!(u.percentage, 100);
        assert_eq!(u.done, 28290);
        assert_eq!(u.total, 28290);
    }

    #[test]
    fn parse_progress_frame_rejects_garbage() {
        assert!(parse_progress_frame("not a frame").is_none());
        assert!(parse_progress_frame("Loaded 12 files").is_none());
        // Total of zero would imply division-by-zero downstream; reject it.
        assert!(parse_progress_frame("0/0 [..] 0%").is_none());
        // 999% is not a valid percentage value.
        assert!(parse_progress_frame("1/1 [..] 999%").is_none());
    }

    #[test]
    fn parse_progress_chunk_returns_latest_frame() {
        // Real PHPStan output uses `\r` to overwrite the previous frame.
        let chunk = "\r 100/500 [==>             ]  20%\r 250/500 [===========>    ]  50%";
        let u = parse_progress_chunk(chunk).expect("frame");
        assert_eq!(u.done, 250);
        assert_eq!(u.percentage, 50);
    }

    #[test]
    fn parse_progress_chunk_skips_non_progress_lines() {
        // First line is bootstrap noise, second is a progress frame.
        let chunk = "Note: Using configuration file phpstan.dist.neon\n 7/14 [=======>    ] 50%";
        let u = parse_progress_chunk(chunk).expect("frame");
        assert_eq!(u.done, 7);
        assert_eq!(u.percentage, 50);
    }

    #[test]
    fn parse_progress_chunk_returns_none_for_pure_text() {
        assert!(parse_progress_chunk("Loading bootstrap file...\n").is_none());
    }

    #[test]
    fn parse_progress_chunk_handles_ansi_cursor_resets() {
        // Verbatim non-TTY output from PHPStan 1.x: each frame is preceded
        // by `ESC[1G` (cursor to column 1) and `ESC[2K` (erase line)
        // instead of a carriage return. The previous regex-style splitter
        // collapsed the whole sequence into one "line" and produced a
        // bogus `0/50, 100%` reading. We must surface the *last* full
        // frame here.
        let chunk =
            "  0/50 [....]   0%\x1b[1G\x1b[2K 20/50 [..]  40%\x1b[1G\x1b[2K 50/50 [..] 100%";
        let u = parse_progress_chunk(chunk).expect("frame");
        assert_eq!(u.done, 50);
        assert_eq!(u.total, 50);
        assert_eq!(u.percentage, 100);
    }

    #[test]
    fn normalise_progress_chunk_replaces_csi_with_newline() {
        let n = normalise_progress_chunk("a\x1b[1Gb\x1b[2Kc");
        assert_eq!(n, "a\nb\nc");
    }

    #[tokio::test]
    async fn drain_stderr_emits_progress_updates_and_returns_text() {
        // Feed a synthetic stderr stream and verify both side-effects: the
        // channel receives parsed frames and the returned String contains
        // every byte we wrote (so crash reports are still complete).
        //
        // We synchronise writes with reads on the channel so the parser
        // sees each frame in its own chunk, mirroring how PHPStan emits
        // progress in production (one flush per redraw). When the OS
        // happens to coalesce two frames into one chunk, we only emit the
        // latest — that is intentional and tested separately by
        // `parse_progress_chunk_returns_latest_frame`.
        let (mut writer, reader) = tokio::io::duplex(256);
        let (tx, mut rx) = mpsc::channel::<ProgressUpdate>(8);

        let drain = tokio::spawn(drain_stderr(reader, Some(tx)));

        use tokio::io::AsyncWriteExt;
        writer.write_all(b" 10/100 [>] 10%\r").await.unwrap();
        writer.flush().await.unwrap();
        let first = rx.recv().await.unwrap();
        writer.write_all(b" 50/100 [====>] 50%\r").await.unwrap();
        writer.flush().await.unwrap();
        let second = rx.recv().await.unwrap();
        writer.write_all(b"100/100 [=========] 100%").await.unwrap();
        drop(writer);

        let text = drain.await.unwrap();
        assert!(text.contains("100/100"));

        let mut updates = vec![first, second];
        while let Some(u) = rx.recv().await {
            updates.push(u);
        }
        // Each unique frame is emitted at most once.
        assert_eq!(
            updates,
            vec![
                ProgressUpdate {
                    done: 10,
                    total: 100,
                    percentage: 10
                },
                ProgressUpdate {
                    done: 50,
                    total: 100,
                    percentage: 50
                },
                ProgressUpdate {
                    done: 100,
                    total: 100,
                    percentage: 100
                },
            ]
        );
    }
}
