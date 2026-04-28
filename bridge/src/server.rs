//! `tower-lsp` server implementation that drives the PHPStan runner and
//! publishes diagnostics back to the editor.
//!
//! ## Analysis model
//!
//! PHPStan is invoked **project-wide**, not per file. Running it on a single
//! file would defeat its result cache and hide cross-file findings (e.g. an
//! undefined function reference resolved in a sibling file). The `paths`
//! key in `phpstan.neon` defines the analysis surface; PHPStan's own cache
//! handles incremental speed.
//!
//! Re-runs are triggered by:
//! * `initialized` — once, on project open;
//! * `textDocument/didSave` — whenever the user saves any PHP file;
//! * `workspace/didChangeWatchedFiles` — whenever a `phpstan.neon[.*]` file
//!   is created, modified, or deleted (registered dynamically in
//!   `initialized`).
//!
//! Concurrent triggers are coalesced: if a request arrives while another run
//! is in flight, it is merged into a single follow-up run rather than
//! queueing every event.

use crate::config::BridgeConfig;
use crate::diagnostics::map_all_diagnostics;
use crate::phpstan::{AnalyseRequest, PhpStanRunner, ProgressUpdate};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tower_lsp::Client;
use tower_lsp::LanguageServer;
use tower_lsp::jsonrpc::Result as JsonRpcResult;
use tower_lsp::lsp_types::notification::Progress as ProgressNotification;
use tower_lsp::lsp_types::request::WorkDoneProgressCreate;
use tower_lsp::lsp_types::{
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, FileSystemWatcher, GlobPattern,
    InitializeParams, InitializeResult, InitializedParams, MessageType, ProgressParams,
    ProgressParamsValue, ProgressToken, Registration, SaveOptions, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Url, WatchKind, WorkDoneProgress, WorkDoneProgressBegin,
    WorkDoneProgressCreateParams, WorkDoneProgressEnd, WorkDoneProgressReport,
};

/// Identifier used when registering the dynamic watched-files capability.
/// Stable so re-registration is idempotent if the editor re-uses the slot.
const NEON_WATCH_REGISTRATION_ID: &str = "phpstan-neon-watch";

/// LSP method whose dynamic capability we register on `initialized`.
const DID_CHANGE_WATCHED_FILES_METHOD: &str = "workspace/didChangeWatchedFiles";

/// Glob patterns covering the canonical PHPStan configuration filenames.
const NEON_GLOB_PATTERNS: &[&str] = &[
    "**/phpstan.neon",
    "**/phpstan.neon.dist",
    "**/phpstan.dist.neon",
];

/// Title shown alongside the `$/progress` notifications during analysis.
/// Surfaces in the editor's status area (e.g. Zed's bottom bar).
const PROGRESS_TITLE: &str = "PHPStan";

/// Secondary message displayed while a run is in flight.
const PROGRESS_BEGIN_MESSAGE: &str = "Analysing project\u{2026}";

/// Prefix for server-generated work-done progress tokens. Combined with a
/// monotonic counter to keep tokens unique across runs.
const PROGRESS_TOKEN_PREFIX: &str = "phpstan-bridge/";

/// Message used in `WorkDoneProgressEnd` when the run failed. Successful
/// runs build their message with [`progress_end_message`].
const PROGRESS_END_FAILED: &str = "Analysis failed";

/// Bound on buffered progress updates between the runner and the LSP
/// forwarder task. Updates are dropped (via `try_send`) rather than
/// blocking PHPStan when the editor cannot keep up.
const PROGRESS_CHANNEL_CAPACITY: usize = 32;

/// Format the trailing `End` message based on how many diagnostics the run
/// surfaced. Kept as a free function so it can be unit-tested without a
/// full server fixture.
fn progress_end_message(total: usize) -> String {
    match total {
        0 => "No issues found".to_string(),
        1 => "1 issue".to_string(),
        n => format!("{n} issues"),
    }
}

/// LSP server backing the bridge.
pub struct PhpStanLspServer {
    client: Client,
    config: BridgeConfig,
    runner: Arc<dyn PhpStanRunner>,
    /// URIs we have most recently published non-empty diagnostics for. Used
    /// to clear stale findings when a subsequent run no longer reports them.
    published_uris: StdMutex<HashSet<Url>>,
    /// Held for the duration of an analysis run. We use `try_lock` to detect
    /// in-flight runs without blocking the caller.
    run_lock: AsyncMutex<()>,
    /// Set when a request arrives during an in-flight run, so the active
    /// runner knows to drain one more pass before releasing the lock.
    pending: AtomicBool,
    /// Whether the client advertised `window.workDoneProgress` support
    /// during initialize. Server-initiated progress is suppressed when
    /// false to remain spec-compliant with minimal clients.
    client_supports_progress: AtomicBool,
    /// Monotonic counter feeding [`PROGRESS_TOKEN_PREFIX`] to produce unique
    /// progress tokens per run.
    progress_counter: AtomicU64,
}

impl PhpStanLspServer {
    pub fn new(client: Client, config: BridgeConfig, runner: Arc<dyn PhpStanRunner>) -> Self {
        Self {
            client,
            config,
            runner,
            published_uris: StdMutex::new(HashSet::new()),
            run_lock: AsyncMutex::new(()),
            pending: AtomicBool::new(false),
            client_supports_progress: AtomicBool::new(false),
            progress_counter: AtomicU64::new(0),
        }
    }

    pub fn capabilities() -> ServerCapabilities {
        ServerCapabilities {
            text_document_sync: Some(TextDocumentSyncCapability::Options(
                TextDocumentSyncOptions {
                    // open_close is advertised so the editor still routes
                    // notifications; the server itself ignores them.
                    open_close: Some(true),
                    change: Some(TextDocumentSyncKind::NONE),
                    save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                        include_text: Some(false),
                    })),
                    will_save: Some(false),
                    will_save_wait_until: Some(false),
                },
            )),
            workspace: None,
            diagnostic_provider: None,
            // We never advertise a code-action provider; PHPStan only emits
            // text findings. Keep the surface minimal.
            ..Default::default()
        }
    }

    /// Schedule a project-wide PHPStan run, coalescing with any in-flight
    /// run so we never queue more than one follow-up.
    async fn request_analysis(&self) {
        // Try to become the active runner. If we can't, signal the active
        // one that one more pass is needed and return.
        let _guard = match self.run_lock.try_lock() {
            Ok(g) => g,
            Err(_) => {
                self.pending.store(true, Ordering::SeqCst);
                tracing::debug!("analysis already running; coalesced into pending");
                return;
            }
        };

        // Drain pending requests in a tight loop. Each iteration clears the
        // pending flag *before* running so requests that arrive while the
        // run is in progress trigger another pass.
        loop {
            self.pending.store(false, Ordering::SeqCst);
            self.run_analysis_once().await;
            if !self.pending.load(Ordering::SeqCst) {
                return;
            }
        }
    }

    /// Run PHPStan once across the whole project and publish the resulting
    /// diagnostics. Bridge-level failures surface as a single fatal
    /// diagnostic anchored at the project root so the user always sees
    /// feedback in the editor.
    async fn run_analysis_once(&self) {
        let progress = self.begin_progress().await;

        // Build a streaming channel for live `$/progress` reports. We only
        // attach the sender (and thus only ask PHPStan to emit its progress
        // bar) when the client actually supports `$/progress` *and* the
        // user has not opted out via `phpstan.showProgress`.
        let (progress_tx, forwarder) = match progress.clone() {
            Some(token) if self.config.show_progress() => {
                let (tx, mut rx) = mpsc::channel::<ProgressUpdate>(PROGRESS_CHANNEL_CAPACITY);
                let client = self.client.clone();
                let handle = tokio::spawn(async move {
                    while let Some(update) = rx.recv().await {
                        client
                            .send_notification::<ProgressNotification>(ProgressParams {
                                token: token.clone(),
                                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                                    WorkDoneProgressReport {
                                        cancellable: Some(false),
                                        message: Some(format!(
                                            "{} / {} ({}%)",
                                            update.done, update.total, update.percentage
                                        )),
                                        percentage: Some(u32::from(update.percentage)),
                                    },
                                )),
                            })
                            .await;
                    }
                });
                (Some(tx), Some(handle))
            }
            _ => (None, None),
        };

        let req = AnalyseRequest {
            php_path: self.config.php_path(),
            phpstan_path: self.config.phpstan_path(),
            working_directory: self.config.working_directory(),
            level: self.config.level(),
            memory_limit: self.config.memory_limit(),
            config_path: self.config.config_path(),
            progress_tx,
        };

        let end_message = match self.runner.analyse(req).await {
            Ok(output) => {
                let diags = map_all_diagnostics(&output, self.config.working_directory());
                let total: usize = diags.values().map(|d| d.len()).sum();
                let new_uris: HashSet<Url> = diags.keys().cloned().collect();

                // Publish fresh diagnostics first.
                for (uri, ds) in diags {
                    tracing::debug!(%uri, count = ds.len(), "publishing diagnostics");
                    self.client.publish_diagnostics(uri, ds, None).await;
                }

                // Clear any URIs that had diagnostics last time but no
                // longer do, otherwise stale findings remain in the editor.
                let stale: Vec<Url> = {
                    let mut tracked = self
                        .published_uris
                        .lock()
                        .expect("published_uris mutex poisoned");
                    let stale = tracked.difference(&new_uris).cloned().collect();
                    *tracked = new_uris;
                    stale
                };
                for uri in stale {
                    tracing::debug!(%uri, "clearing stale diagnostics");
                    self.client.publish_diagnostics(uri, Vec::new(), None).await;
                }

                progress_end_message(total)
            }
            Err(err) => {
                let message = err.to_diagnostic_message();
                tracing::error!(error = %message, "PHPStan invocation failed");
                // Surface the failure through both channels:
                //   * `log_message` lands in the LSP log pane;
                //   * `show_message` produces a Zed toast so the user
                //     actually notices that something is wrong.
                // We deliberately do NOT publish a synthetic diagnostic on
                // the project-root URI: editors (Zed included) try to read
                // that URI as a file when rendering inline diagnostics,
                // which fails for a directory and silently drops the
                // notification.
                self.client.log_message(MessageType::ERROR, &message).await;
                self.client
                    .show_message(MessageType::ERROR, format!("PHPStan: {message}"))
                    .await;
                PROGRESS_END_FAILED.to_string()
            }
        };

        // Closing the sender lets the forwarder task exit naturally.
        if let Some(handle) = forwarder {
            // The sender is held by `req`, which has already been consumed
            // by `runner.analyse`. Awaiting drains any pending updates.
            let _ = handle.await;
        }

        self.end_progress(progress, end_message).await;
    }

    /// Allocate a fresh progress token and ask the client to create the
    /// `$/progress` slot, then send the `Begin` notification.
    ///
    /// Returns `None` if the client did not advertise progress support, or
    /// if the create request failed. Either case is non-fatal — analysis
    /// continues without a status indicator.
    async fn begin_progress(&self) -> Option<ProgressToken> {
        if !self.client_supports_progress.load(Ordering::SeqCst) {
            return None;
        }
        let n = self.progress_counter.fetch_add(1, Ordering::SeqCst);
        let token = ProgressToken::String(format!("{PROGRESS_TOKEN_PREFIX}{n}"));
        if let Err(e) = self
            .client
            .send_request::<WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await
        {
            tracing::debug!(error = %e, "client refused workDoneProgress/create");
            return None;
        }
        // Fire-and-forget: tower-lsp's progress builder owns the begin
        // notification. We don't keep the OngoingProgress handle because
        // it isn't `Send` across our await points cleanly; instead we send
        // the End notification ourselves in `end_progress`.
        self.client
            .send_notification::<ProgressNotification>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: PROGRESS_TITLE.to_string(),
                        cancellable: Some(false),
                        message: Some(PROGRESS_BEGIN_MESSAGE.to_string()),
                        percentage: None,
                    },
                )),
            })
            .await;
        Some(token)
    }

    /// Send the matching `End` notification for a previously-issued
    /// progress token. No-op when `token` is `None`.
    async fn end_progress(&self, token: Option<ProgressToken>, message: String) {
        let Some(token) = token else { return };
        self.client
            .send_notification::<ProgressNotification>(ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: Some(message),
                })),
            })
            .await;
    }

    /// Register the dynamic file-watch capability for `phpstan.neon*` files.
    /// Failures are logged but non-fatal — clients without dynamic
    /// registration support still get save-triggered analysis.
    async fn register_neon_watcher(&self) {
        let watchers: Vec<FileSystemWatcher> = NEON_GLOB_PATTERNS
            .iter()
            .map(|pat| FileSystemWatcher {
                glob_pattern: GlobPattern::String((*pat).to_string()),
                kind: Some(WatchKind::all()),
            })
            .collect();

        let options = DidChangeWatchedFilesRegistrationOptions { watchers };
        let register_options = match serde_json::to_value(options) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize watch options");
                return;
            }
        };

        let registration = Registration {
            id: NEON_WATCH_REGISTRATION_ID.to_string(),
            method: DID_CHANGE_WATCHED_FILES_METHOD.to_string(),
            register_options: Some(register_options),
        };

        if let Err(e) = self.client.register_capability(vec![registration]).await {
            tracing::warn!(error = %e, "failed to register neon watcher");
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for PhpStanLspServer {
    async fn initialize(&self, params: InitializeParams) -> JsonRpcResult<InitializeResult> {
        let supports_progress = params
            .capabilities
            .window
            .as_ref()
            .and_then(|w| w.work_done_progress)
            .unwrap_or(false);
        self.client_supports_progress
            .store(supports_progress, Ordering::SeqCst);
        Ok(InitializeResult {
            capabilities: Self::capabilities(),
            server_info: Some(ServerInfo {
                name: "phpstan-lsp-bridge".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "phpstan-lsp-bridge ready")
            .await;
        self.register_neon_watcher().await;
        // Initial project analysis on open.
        self.request_analysis().await;
    }

    async fn shutdown(&self) -> JsonRpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, _params: DidOpenTextDocumentParams) {
        // No-op: project-wide analysis is triggered on `initialized` and on
        // save. Re-running on every open would thrash PHPStan's cache and
        // duplicate work for files already covered by the project run.
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) {
        self.request_analysis().await;
    }

    async fn did_change(&self, _params: DidChangeTextDocumentParams) {
        // Intentionally a no-op: PHPStan is too expensive to run on every
        // keystroke. The OnChange trigger remains for forward compatibility.
    }

    async fn did_close(&self, _params: DidCloseTextDocumentParams) {
        // No-op: project-wide diagnostics persist regardless of which files
        // are open in the editor. Clearing on close would hide real findings
        // until the next save.
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        tracing::info!(
            count = params.changes.len(),
            "phpstan.neon change detected; re-running analysis"
        );
        self.request_analysis().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_advertise_open_close_and_save() {
        let caps = PhpStanLspServer::capabilities();
        let sync = caps.text_document_sync.expect("text_document_sync");
        let opts = match sync {
            TextDocumentSyncCapability::Options(o) => o,
            _ => panic!("expected options"),
        };
        assert_eq!(opts.open_close, Some(true));
        assert_eq!(opts.change, Some(TextDocumentSyncKind::NONE));
        match opts.save.expect("save") {
            TextDocumentSyncSaveOptions::SaveOptions(s) => {
                assert_eq!(s.include_text, Some(false))
            }
            _ => panic!("expected SaveOptions variant"),
        }
    }

    #[test]
    fn neon_glob_patterns_cover_canonical_filenames() {
        // The user-facing contract is that every PHPStan configuration
        // filename triggers a re-run. Lock the list down to catch typos.
        assert!(NEON_GLOB_PATTERNS.contains(&"**/phpstan.neon"));
        assert!(NEON_GLOB_PATTERNS.contains(&"**/phpstan.neon.dist"));
        assert!(NEON_GLOB_PATTERNS.contains(&"**/phpstan.dist.neon"));
    }

    #[test]
    fn progress_end_message_pluralises() {
        assert_eq!(progress_end_message(0), "No issues found");
        assert_eq!(progress_end_message(1), "1 issue");
        assert_eq!(progress_end_message(7), "7 issues");
    }
}
