//! Mapping from PHPStan's JSON-formatted findings to LSP [`Diagnostic`]s.
//!
//! PHPStan does not emit column information, so a diagnostic spans the full
//! line. PHPStan reports lines 1-indexed; LSP requires 0-indexed.

use crate::phpstan::{PhpStanMessage, PhpStanOutput};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Url};

/// Identifier reported as the `source` of every diagnostic we publish.
pub const DIAGNOSTIC_SOURCE: &str = "PHPStan";

/// Convert a project-wide PHPStan output into a map of file URI →
/// diagnostics. Paths that PHPStan reports as relative are resolved against
/// `working_directory`. Paths that fail to convert into a `file://` URI are
/// silently skipped — they correspond to entries we cannot publish anyway.
pub fn map_all_diagnostics(
    output: &PhpStanOutput,
    working_directory: Option<&Path>,
) -> HashMap<Url, Vec<Diagnostic>> {
    let mut out = HashMap::with_capacity(output.files.len());
    for (path, file) in &output.files {
        let Some(uri) = path_to_uri(path, working_directory) else {
            continue;
        };
        let diagnostics = file.messages.iter().map(message_to_diagnostic).collect();
        out.insert(uri, diagnostics);
    }
    out
}

fn message_to_diagnostic(msg: &PhpStanMessage) -> Diagnostic {
    // PHPStan uses 1-based line numbers; LSP is 0-based. Treat a missing line
    // as line 1, then subtract one. Also clamp to avoid underflow if PHPStan
    // ever reports `0`.
    let raw_line = msg.line.unwrap_or(1);
    let line = raw_line.saturating_sub(1);

    let mut full_message = msg.message.clone();
    if let Some(tip) = msg.tip.as_ref().filter(|t| !t.is_empty()) {
        full_message.push_str("\n\nTip: ");
        full_message.push_str(tip);
    }

    Diagnostic {
        range: Range {
            start: Position { line, character: 0 },
            end: Position {
                line,
                character: u32::MAX,
            },
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: msg.identifier.clone().map(NumberOrString::String),
        code_description: None,
        source: Some(DIAGNOSTIC_SOURCE.to_string()),
        message: full_message,
        related_information: None,
        tags: None,
        data: None,
    }
}

fn path_to_uri(path_str: &str, working_directory: Option<&Path>) -> Option<Url> {
    let path = Path::new(path_str);
    let absolute: PathBuf = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(cwd) = working_directory {
        cwd.join(path)
    } else {
        return None;
    };
    Url::from_file_path(absolute).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phpstan::PhpStanOutput;
    use pretty_assertions::assert_eq;

    fn sample_output() -> PhpStanOutput {
        let json = r#"{
            "totals": {"errors":0,"file_errors":2},
            "files": {
                "/p/src/Foo.php": {
                    "errors": 1,
                    "messages": [
                        {
                            "message": "Undefined variable",
                            "line": 5,
                            "ignorable": true,
                            "identifier": "variable.undefined",
                            "tip": "Define it"
                        }
                    ]
                },
                "/p/src/Bar.php": {
                    "errors": 1,
                    "messages": [
                        {"message":"oops","line":1,"ignorable":true}
                    ]
                }
            },
            "errors": []
        }"#;
        PhpStanOutput::from_json(json).unwrap()
    }

    #[test]
    fn maps_every_file_in_output() {
        let map = map_all_diagnostics(&sample_output(), None);
        let foo = Url::from_file_path("/p/src/Foo.php").unwrap();
        let bar = Url::from_file_path("/p/src/Bar.php").unwrap();
        assert_eq!(map.len(), 2);
        let foo_diags = &map[&foo];
        assert_eq!(foo_diags.len(), 1);
        assert_eq!(foo_diags[0].range.start.line, 4);
        assert_eq!(foo_diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(foo_diags[0].source.as_deref(), Some(DIAGNOSTIC_SOURCE));
        assert_eq!(
            foo_diags[0].code,
            Some(NumberOrString::String("variable.undefined".to_string()))
        );
        assert!(foo_diags[0].message.contains("Tip: Define it"));
        let bar_diags = &map[&bar];
        assert_eq!(bar_diags.len(), 1);
        assert!(bar_diags[0].code.is_none());
    }

    #[test]
    fn relative_paths_are_resolved_against_working_directory() {
        let json = r#"{
            "totals":{"errors":0,"file_errors":1},
            "files":{
                "src/Rel.php":{"errors":1,"messages":[
                    {"message":"r","line":2,"ignorable":true}
                ]}
            },
            "errors":[]
        }"#;
        let out = PhpStanOutput::from_json(json).unwrap();
        let map = map_all_diagnostics(&out, Some(Path::new("/proj")));
        let expected = Url::from_file_path("/proj/src/Rel.php").unwrap();
        assert!(map.contains_key(&expected), "got keys: {:?}", map.keys());
    }

    #[test]
    fn relative_paths_without_working_directory_are_skipped() {
        let json = r#"{
            "totals":{"errors":0,"file_errors":1},
            "files":{
                "src/Rel.php":{"errors":1,"messages":[
                    {"message":"r","ignorable":true}
                ]}
            },
            "errors":[]
        }"#;
        let out = PhpStanOutput::from_json(json).unwrap();
        let map = map_all_diagnostics(&out, None);
        assert!(map.is_empty());
    }

    #[test]
    fn missing_line_falls_back_to_zero() {
        let json = r#"{
            "totals":{"errors":0,"file_errors":1},
            "files":{
                "/p/x.php":{"errors":1,"messages":[
                    {"message":"oops","ignorable":true}
                ]}
            },
            "errors":[]
        }"#;
        let out = PhpStanOutput::from_json(json).unwrap();
        let map = map_all_diagnostics(&out, None);
        let uri = Url::from_file_path("/p/x.php").unwrap();
        assert_eq!(map[&uri][0].range.start.line, 0);
    }

    #[test]
    fn line_zero_does_not_underflow() {
        let json = r#"{
            "totals":{"errors":0,"file_errors":1},
            "files":{
                "/p/x.php":{"errors":1,"messages":[
                    {"message":"oops","line":0,"ignorable":true}
                ]}
            },
            "errors":[]
        }"#;
        let out = PhpStanOutput::from_json(json).unwrap();
        let map = map_all_diagnostics(&out, None);
        let uri = Url::from_file_path("/p/x.php").unwrap();
        assert_eq!(map[&uri][0].range.start.line, 0);
    }
}
