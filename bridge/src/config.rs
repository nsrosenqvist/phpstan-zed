//! Strongly-typed configuration values used by the bridge.

use std::path::{Path, PathBuf};

/// When PHPStan should be invoked relative to editor events.
///
/// Currently only [`DiagnosticTrigger::OnSave`] changes runtime behaviour;
/// [`DiagnosticTrigger::OnChange`] is wired through the CLI for forward
/// compatibility but the server still only acts on `didSave`/`didOpen`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticTrigger {
    /// Run PHPStan on `textDocument/didSave` and `textDocument/didOpen`.
    OnSave,
    /// Run PHPStan on `textDocument/didChange`, debounced.
    OnChange,
}

impl Default for DiagnosticTrigger {
    fn default() -> Self {
        Self::OnSave
    }
}

impl DiagnosticTrigger {
    /// Lower-cased CLI representation accepted by the bridge `--trigger` flag.
    pub fn as_cli(self) -> &'static str {
        match self {
            Self::OnSave => "on-save",
            Self::OnChange => "on-change",
        }
    }

    /// Parse a CLI string back into a trigger. Returns an `Err` with the
    /// offending value so `clap` can surface it to the user.
    pub fn parse_cli(s: &str) -> Result<Self, String> {
        match s {
            "on-save" => Ok(Self::OnSave),
            "on-change" => Ok(Self::OnChange),
            other => Err(format!(
                "invalid trigger '{other}'; expected 'on-save' or 'on-change'"
            )),
        }
    }
}

/// Immutable runtime configuration assembled from CLI arguments.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    phpstan_path: PathBuf,
    php_path: PathBuf,
    working_directory: Option<PathBuf>,
    trigger: DiagnosticTrigger,
    level: Option<u8>,
    memory_limit: Option<String>,
    config_path: Option<PathBuf>,
    show_progress: bool,
}

impl BridgeConfig {
    /// Construct a new configuration. Paths are not validated at construction
    /// time — the LSP layer surfaces validation errors as diagnostics so the
    /// editor can render them.
    pub fn new(
        phpstan_path: PathBuf,
        php_path: PathBuf,
        working_directory: Option<PathBuf>,
    ) -> Self {
        Self {
            phpstan_path,
            php_path,
            working_directory,
            trigger: DiagnosticTrigger::default(),
            level: None,
            memory_limit: None,
            config_path: None,
            show_progress: true,
        }
    }

    pub fn with_trigger(mut self, trigger: DiagnosticTrigger) -> Self {
        self.trigger = trigger;
        self
    }

    pub fn with_level(mut self, level: Option<u8>) -> Self {
        self.level = level;
        self
    }

    pub fn with_memory_limit(mut self, memory_limit: Option<String>) -> Self {
        self.memory_limit = memory_limit.filter(|s| !s.is_empty());
        self
    }

    pub fn with_config_path(mut self, config_path: Option<PathBuf>) -> Self {
        self.config_path = config_path;
        self
    }

    /// Toggle live PHPStan progress reporting. When `false`, the server
    /// still emits a spinner via `$/progress` but does not stream PHPStan's
    /// own progress bar through it.
    pub fn with_show_progress(mut self, show_progress: bool) -> Self {
        self.show_progress = show_progress;
        self
    }

    pub fn phpstan_path(&self) -> &Path {
        &self.phpstan_path
    }

    pub fn php_path(&self) -> &Path {
        &self.php_path
    }

    pub fn working_directory(&self) -> Option<&Path> {
        self.working_directory.as_deref()
    }

    pub fn trigger(&self) -> DiagnosticTrigger {
        self.trigger
    }

    pub fn level(&self) -> Option<u8> {
        self.level
    }

    pub fn memory_limit(&self) -> Option<&str> {
        self.memory_limit.as_deref()
    }

    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    pub fn show_progress(&self) -> bool {
        self.show_progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_trigger_is_on_save() {
        assert_eq!(DiagnosticTrigger::default(), DiagnosticTrigger::OnSave);
    }

    #[test]
    fn config_roundtrips_paths() {
        let cfg = BridgeConfig::new(
            PathBuf::from("/p/phpstan"),
            PathBuf::from("/p/php"),
            Some(PathBuf::from("/p")),
        );
        assert_eq!(cfg.phpstan_path(), Path::new("/p/phpstan"));
        assert_eq!(cfg.php_path(), Path::new("/p/php"));
        assert_eq!(cfg.working_directory(), Some(Path::new("/p")));
        assert_eq!(cfg.trigger(), DiagnosticTrigger::OnSave);
        assert!(cfg.level().is_none());
        assert!(cfg.memory_limit().is_none());
        assert!(cfg.config_path().is_none());
    }

    #[test]
    fn builder_methods_set_fields() {
        let cfg = BridgeConfig::new(PathBuf::from("a"), PathBuf::from("b"), None)
            .with_trigger(DiagnosticTrigger::OnChange)
            .with_level(Some(7))
            .with_memory_limit(Some("1G".into()))
            .with_config_path(Some(PathBuf::from("/p/phpstan.neon")));
        assert_eq!(cfg.trigger(), DiagnosticTrigger::OnChange);
        assert_eq!(cfg.level(), Some(7));
        assert_eq!(cfg.memory_limit(), Some("1G"));
        assert_eq!(cfg.config_path(), Some(Path::new("/p/phpstan.neon")));
    }

    #[test]
    fn empty_memory_limit_is_treated_as_unset() {
        let cfg = BridgeConfig::new(PathBuf::from("a"), PathBuf::from("b"), None)
            .with_memory_limit(Some(String::new()));
        assert!(cfg.memory_limit().is_none());
    }

    #[test]
    fn diagnostic_trigger_serializes_camel_case() {
        assert_eq!(
            serde_json::to_string(&DiagnosticTrigger::OnSave).unwrap(),
            "\"onSave\""
        );
        assert_eq!(
            serde_json::to_string(&DiagnosticTrigger::OnChange).unwrap(),
            "\"onChange\""
        );
    }

    #[test]
    fn diagnostic_trigger_cli_roundtrip() {
        for v in [DiagnosticTrigger::OnSave, DiagnosticTrigger::OnChange] {
            assert_eq!(DiagnosticTrigger::parse_cli(v.as_cli()).unwrap(), v);
        }
        assert!(DiagnosticTrigger::parse_cli("nope").is_err());
    }

    #[test]
    fn show_progress_defaults_to_true_and_toggles() {
        let cfg = BridgeConfig::new(PathBuf::from("a"), PathBuf::from("b"), None);
        assert!(cfg.show_progress());
        let cfg = cfg.with_show_progress(false);
        assert!(!cfg.show_progress());
    }
}
