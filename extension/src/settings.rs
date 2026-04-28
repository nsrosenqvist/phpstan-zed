//! Strongly-typed initialization options exchanged with the bridge.
//!
//! These types serve two purposes:
//!
//! 1. Provide the JSON contract returned from
//!    [`zed::Extension::language_server_initialization_options`], so editors
//!    can see the schema we accept.
//! 2. Carry user-provided overrides read from
//!    [`zed_extension_api::settings::LspSettings`] through the extension and
//!    on to the bridge as CLI flags.

use serde::{Deserialize, Serialize};
use zed_extension_api::{LanguageServerId, Worktree, settings::LspSettings};

/// Wraps every option under a top-level `phpstan` key, matching how editors
/// typically namespace per-language-server settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PhpStanInitializationOptions {
    pub phpstan: PhpStanSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct PhpStanSettings {
    /// Override `phpstan.neon` analysis level (0–9). `None` defers to the
    /// project configuration.
    pub level: Option<u8>,

    /// Memory limit forwarded to PHPStan (e.g. "512M", "1G").
    pub memory_limit: MemoryLimit,

    /// Optional path to a `phpstan.neon` file.
    pub config_path: Option<String>,

    /// When PHPStan should be triggered relative to editor events.
    pub diagnostic_trigger: DiagnosticTrigger,

    /// Pin a specific PHPStan release (e.g. `"1.11.0"`). `None` means the
    /// resolver downloads the latest stable phar.
    pub pinned_version: Option<String>,

    /// Stream PHPStan's progress bar through `$/progress` reports. When
    /// `false`, the editor still shows a spinner but no live counter.
    #[serde(default = "default_show_progress")]
    pub show_progress: bool,
}

fn default_show_progress() -> bool {
    true
}

impl Default for PhpStanSettings {
    fn default() -> Self {
        Self {
            level: None,
            memory_limit: MemoryLimit::default(),
            config_path: None,
            diagnostic_trigger: DiagnosticTrigger::default(),
            pinned_version: None,
            show_progress: default_show_progress(),
        }
    }
}

impl PhpStanSettings {
    /// Read user-configured Zed settings for this language server and merge
    /// them over [`PhpStanSettings::default`].
    ///
    /// Lookup order:
    /// 1. `lsp.<server>.initialization_options.phpstan` — the canonical place
    ///    for users to put PHPStan tweaks.
    /// 2. `lsp.<server>.settings.phpstan` — accepted as a synonym for
    ///    convenience, since some editor tutorials suggest putting things
    ///    under `settings`.
    ///
    /// Any error is treated as "user has not configured anything", which
    /// keeps the extension robust against malformed JSON in user settings.
    pub fn load(language_server_id: &LanguageServerId, worktree: &Worktree) -> Self {
        let Ok(settings) = LspSettings::for_worktree(language_server_id.as_ref(), worktree) else {
            return Self::default();
        };

        if let Some(value) = settings.initialization_options.as_ref()
            && let Some(parsed) = parse_block(value)
        {
            return parsed;
        }
        if let Some(value) = settings.settings.as_ref()
            && let Some(parsed) = parse_block(value)
        {
            return parsed;
        }
        Self::default()
    }
}

fn parse_block(value: &serde_json::Value) -> Option<PhpStanSettings> {
    let inner = value
        .get("phpstan")
        .cloned()
        .unwrap_or_else(|| value.clone());
    serde_json::from_value(inner).ok()
}

/// PHPStan's `--memory-limit` value. Modelled as a newtype to discourage
/// accidental string concatenation elsewhere in the codebase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryLimit(String);

impl MemoryLimit {
    #[allow(dead_code)] // Public constructor used by tests and downstream callers.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[allow(dead_code)] // Symmetric counterpart to `new`; kept for API completeness.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl Default for MemoryLimit {
    fn default() -> Self {
        Self("512M".to_string())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticTrigger {
    #[default]
    OnSave,
    OnChange,
}

impl DiagnosticTrigger {
    /// Lower-cased CLI representation forwarded to the bridge.
    pub fn as_cli(self) -> &'static str {
        match self {
            Self::OnSave => "on-save",
            Self::OnChange => "on-change",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_serialization_matches_contract() {
        let json = serde_json::to_value(PhpStanInitializationOptions::default()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "phpstan": {
                    "level": null,
                    "memoryLimit": "512M",
                    "configPath": null,
                    "diagnosticTrigger": "onSave",
                    "pinnedVersion": null,
                    "showProgress": true
                }
            })
        );
    }

    #[test]
    fn parses_block_under_phpstan_key() {
        let v = serde_json::json!({
            "phpstan": {
                "level": 7,
                "memoryLimit": "1G",
                "configPath": "/p/phpstan.neon",
                "diagnosticTrigger": "onChange",
                "pinnedVersion": "1.11.0"
            }
        });
        let s = parse_block(&v).expect("parse");
        assert_eq!(s.level, Some(7));
        assert_eq!(s.memory_limit.as_str(), "1G");
        assert_eq!(s.config_path.as_deref(), Some("/p/phpstan.neon"));
        assert_eq!(s.diagnostic_trigger, DiagnosticTrigger::OnChange);
        assert_eq!(s.pinned_version.as_deref(), Some("1.11.0"));
    }

    #[test]
    fn parses_top_level_block_without_namespace() {
        let v = serde_json::json!({
            "level": 4,
            "memoryLimit": "256M"
        });
        let s = parse_block(&v).expect("parse");
        assert_eq!(s.level, Some(4));
        assert_eq!(s.memory_limit.as_str(), "256M");
    }

    #[test]
    fn diagnostic_trigger_serializes_camel_case() {
        assert_eq!(
            serde_json::to_value(DiagnosticTrigger::OnChange).unwrap(),
            serde_json::json!("onChange")
        );
    }

    #[test]
    fn diagnostic_trigger_cli_strings() {
        assert_eq!(DiagnosticTrigger::OnSave.as_cli(), "on-save");
        assert_eq!(DiagnosticTrigger::OnChange.as_cli(), "on-change");
    }

    #[test]
    fn memory_limit_is_transparent_string() {
        let m = MemoryLimit::new("1G");
        assert_eq!(serde_json::to_value(&m).unwrap(), serde_json::json!("1G"));
        assert_eq!(m.as_str(), "1G");
        assert_eq!(m.clone().into_string(), "1G");
    }

    #[test]
    fn malformed_settings_fall_back_to_default() {
        // A non-object value cannot deserialize into PhpStanSettings.
        let v = serde_json::json!("nope");
        assert!(parse_block(&v).is_none());
    }
}
