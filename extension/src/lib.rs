//! PHPStan Zed extension entry point.
//!
//! The extension's only job is to declare a language server, locate (or
//! download) the PHPStan LSP bridge binary, locate (or download) PHPStan
//! itself, and hand Zed a command line.

mod cache;
mod phpstan_resolver;
mod phpstan_server;
mod settings;

use phpstan_server::PhpStanServer;
use settings::{PhpStanInitializationOptions, PhpStanSettings};
use zed_extension_api::{self as zed, LanguageServerId, Result};

struct PhpStanExtension {
    server: Option<PhpStanServer>,
}

impl zed::Extension for PhpStanExtension {
    fn new() -> Self {
        Self { server: None }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        if language_server_id.as_ref() != PhpStanServer::LANGUAGE_SERVER_ID {
            return Err(format!(
                "phpstan-zed received an unexpected language server id: {}",
                language_server_id.as_ref()
            ));
        }

        let server = self.server.get_or_insert_with(PhpStanServer::new);
        server.command(language_server_id, worktree)
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        let opts = PhpStanInitializationOptions {
            phpstan: PhpStanSettings::load(language_server_id, worktree),
        };
        let value = serde_json::to_value(&opts)
            .map_err(|e| format!("failed to serialize PHPStan init options: {e}"))?;
        Ok(Some(value))
    }

    fn language_server_workspace_configuration(
        &mut self,
        _language_server_id: &LanguageServerId,
        _worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }
}

zed::register_extension!(PhpStanExtension);
