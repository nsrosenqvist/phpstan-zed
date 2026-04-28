//! Binary entry point for the PHPStan LSP bridge.
//!
//! Responsibilities:
//! 1. Parse CLI arguments using `clap`.
//! 2. Initialize structured logging on stderr (stdout is reserved for LSP).
//! 3. Construct the [`PhpStanLspServer`] and run it on stdin/stdout.

use clap::Parser;
use phpstan_lsp_bridge::config::{BridgeConfig, DiagnosticTrigger};
use phpstan_lsp_bridge::phpstan::CliPhpStanRunner;
use phpstan_lsp_bridge::server::PhpStanLspServer;
use std::path::PathBuf;
use std::sync::Arc;
use tower_lsp::{LspService, Server};
use tracing_subscriber::EnvFilter;

/// Command-line arguments accepted by the bridge.
#[derive(Debug, Parser)]
#[command(
    name = "phpstan-lsp-bridge",
    version,
    about = "LSP bridge that wraps the PHPStan CLI and publishes diagnostics."
)]
struct Args {
    /// Absolute path to the PHPStan binary or phar.
    #[arg(long, value_name = "PATH")]
    phpstan_path: PathBuf,

    /// Absolute path to the PHP interpreter.
    #[arg(long, value_name = "PATH")]
    php_path: PathBuf,

    /// Optional working directory for the PHPStan invocation.
    #[arg(long, value_name = "PATH")]
    working_directory: Option<PathBuf>,

    /// Override `phpstan.neon` analysis level (0–9).
    #[arg(long, value_name = "N")]
    level: Option<u8>,

    /// PHP `--memory-limit` value forwarded to PHPStan (e.g. "512M", "1G").
    #[arg(long, value_name = "LIMIT")]
    memory_limit: Option<String>,

    /// Optional path to a `phpstan.neon`/`phpstan.neon.dist` file.
    #[arg(long, value_name = "PATH")]
    config_path: Option<PathBuf>,

    /// When PHPStan should be invoked (`on-save` or `on-change`).
    #[arg(
        long,
        value_name = "MODE",
        default_value = "on-save",
        value_parser = DiagnosticTrigger::parse_cli,
    )]
    trigger: DiagnosticTrigger,

    /// Stream PHPStan's progress bar through `$/progress` reports. When
    /// `false`, the editor still shows a spinner but no live counter.
    #[arg(long, value_name = "BOOL", default_value_t = true, action = clap::ArgAction::Set)]
    show_progress: bool,
}

#[tokio::main]
async fn main() {
    // Logs go to stderr because stdout is the LSP transport.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let args = Args::parse();
    let config = BridgeConfig::new(args.phpstan_path, args.php_path, args.working_directory)
        .with_trigger(args.trigger)
        .with_level(args.level)
        .with_memory_limit(args.memory_limit)
        .with_config_path(args.config_path)
        .with_show_progress(args.show_progress);

    tracing::info!(?config, "starting phpstan-lsp-bridge");

    let runner = Arc::new(CliPhpStanRunner::new());
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| PhpStanLspServer::new(client, config, runner));
    Server::new(stdin, stdout, socket).serve(service).await;
}
