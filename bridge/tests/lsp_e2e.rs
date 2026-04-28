//! End-to-end integration test that drives [`PhpStanLspServer`] via
//! `tower-lsp`'s in-memory transport. A fake [`PhpStanRunner`] returns
//! canned JSON so the test never spawns a real PHPStan process.
//!
//! These tests exercise the project-wide analysis flow:
//! * `initialized` triggers an initial run that publishes diagnostics for
//!   every file PHPStan reports;
//! * a subsequent `didSave` whose run no longer reports a previously-failing
//!   file produces an empty `publishDiagnostics` for that file (clearing
//!   stale findings).

use async_trait::async_trait;
use phpstan_lsp_bridge::config::BridgeConfig;
use phpstan_lsp_bridge::error::BridgeResult;
use phpstan_lsp_bridge::phpstan::{AnalyseRequest, PhpStanOutput, PhpStanRunner, ProgressUpdate};
use phpstan_lsp_bridge::server::PhpStanLspServer;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower_lsp::{LspService, Server};

/// Runner that returns the JSON at the front of the queue on each call. If
/// the queue has only one entry left, that entry sticks — tests don't need
/// to predict the exact number of invocations.
#[derive(Clone)]
struct ScriptedRunner {
    script: Arc<Mutex<Vec<String>>>,
}

impl ScriptedRunner {
    fn new(scripts: Vec<&str>) -> Self {
        Self {
            script: Arc::new(Mutex::new(
                scripts.into_iter().rev().map(String::from).collect(),
            )),
        }
    }
}

#[async_trait]
impl PhpStanRunner for ScriptedRunner {
    async fn analyse(&self, _req: AnalyseRequest<'_>) -> BridgeResult<PhpStanOutput> {
        let json = {
            let mut q = self.script.lock().unwrap();
            if q.len() > 1 {
                q.pop().unwrap()
            } else {
                q.last().cloned().expect("scripted runner had no responses")
            }
        };
        PhpStanOutput::from_json(&json)
    }
}

fn frame(payload: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(payload).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut out = header.into_bytes();
    out.extend_from_slice(&body);
    out
}

async fn read_frame(read: &mut (impl tokio::io::AsyncRead + Unpin)) -> Value {
    let mut header = Vec::new();
    let mut buf = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        let n = read.read(&mut buf).await.expect("read");
        if n == 0 {
            panic!("eof before header end");
        }
        header.push(buf[0]);
    }
    let header = String::from_utf8(header).unwrap();
    let len: usize = header
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .expect("Content-Length")
        .trim()
        .parse()
        .unwrap();
    let mut body = vec![0u8; len];
    read.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// Drain frames until `predicate` matches, automatically replying to any
/// `client/registerCapability` or `window/workDoneProgress/create` request
/// along the way (the server emits these from `initialized` and would
/// deadlock if we never answered).
async fn read_until<R, W, F>(read: &mut R, write: &mut W, mut predicate: F) -> Value
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
    F: FnMut(&Value) -> bool,
{
    let timeout = tokio::time::Duration::from_secs(5);
    let fut = async {
        loop {
            let v = read_frame(read).await;
            let method = v.get("method");
            let is_register = method == Some(&json!("client/registerCapability"));
            let is_progress_create = method == Some(&json!("window/workDoneProgress/create"));
            if (is_register || is_progress_create)
                && let Some(id) = v.get("id")
            {
                let reply = json!({"jsonrpc":"2.0","id":id,"result":null});
                write.write_all(&frame(&reply)).await.unwrap();
                continue;
            }
            if predicate(&v) {
                return v;
            }
        }
    };
    tokio::time::timeout(timeout, fut)
        .await
        .expect("timed out waiting for matching LSP frame")
}

fn spawn_server(
    runner: Arc<dyn PhpStanRunner>,
) -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
    let config = BridgeConfig::new(
        PathBuf::from("/fake/phpstan"),
        PathBuf::from("/fake/php"),
        None,
    );
    let (client_to_server, server_in) = tokio::io::duplex(8192);
    let (server_out, server_to_client) = tokio::io::duplex(8192);
    let runner_for_factory = runner.clone();
    let config_for_factory = config.clone();
    let (service, socket) = LspService::new(move |client| {
        PhpStanLspServer::new(
            client,
            config_for_factory.clone(),
            runner_for_factory.clone(),
        )
    });
    tokio::spawn(async move {
        Server::new(server_in, server_out, socket)
            .serve(service)
            .await;
    });
    (client_to_server, server_to_client)
}

async fn handshake(
    client_to_server: &mut tokio::io::DuplexStream,
    server_to_client: &mut tokio::io::DuplexStream,
) {
    handshake_with_capabilities(
        client_to_server,
        server_to_client,
        json!({"window":{"workDoneProgress":true}}),
    )
    .await;
}

async fn handshake_with_capabilities(
    client_to_server: &mut tokio::io::DuplexStream,
    server_to_client: &mut tokio::io::DuplexStream,
    capabilities: Value,
) {
    client_to_server
        .write_all(&frame(&json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"capabilities":capabilities}
        })))
        .await
        .unwrap();
    let init_resp = read_until(server_to_client, client_to_server, |v| {
        v.get("id") == Some(&json!(1))
    })
    .await;
    assert!(init_resp["result"]["capabilities"]["textDocumentSync"].is_object());
    client_to_server
        .write_all(&frame(&json!({
            "jsonrpc":"2.0","method":"initialized","params":{}
        })))
        .await
        .unwrap();
}

#[tokio::test]
async fn initialized_publishes_project_wide_diagnostics() {
    let json = r#"{
        "totals":{"errors":0,"file_errors":2},
        "files":{
            "/proj/src/Foo.php":{"errors":1,"messages":[
                {"message":"Undefined variable","line":3,"ignorable":true,"identifier":"variable.undefined"}
            ]},
            "/proj/src/Bar.php":{"errors":1,"messages":[
                {"message":"missing return","line":7,"ignorable":true}
            ]}
        },
        "errors":[]
    }"#;
    let runner = Arc::new(ScriptedRunner::new(vec![json]));
    let (mut c2s, mut s2c) = spawn_server(runner);

    handshake(&mut c2s, &mut s2c).await;

    // After `initialized`, the server should publish diagnostics for both
    // files. They may arrive in either order — collect until we have both.
    let mut seen = std::collections::HashMap::<String, Value>::new();
    while seen.len() < 2 {
        let v = read_until(&mut s2c, &mut c2s, |v| {
            v.get("method") == Some(&json!("textDocument/publishDiagnostics"))
        })
        .await;
        let uri = v["params"]["uri"].as_str().unwrap().to_string();
        seen.insert(uri, v);
    }
    let foo = &seen["file:///proj/src/Foo.php"];
    let bar = &seen["file:///proj/src/Bar.php"];
    let foo_diags = foo["params"]["diagnostics"].as_array().unwrap();
    let bar_diags = bar["params"]["diagnostics"].as_array().unwrap();
    assert_eq!(foo_diags.len(), 1);
    assert_eq!(foo_diags[0]["range"]["start"]["line"], json!(2));
    assert_eq!(foo_diags[0]["source"], json!("PHPStan"));
    assert_eq!(foo_diags[0]["code"], json!("variable.undefined"));
    assert_eq!(bar_diags.len(), 1);
    assert_eq!(bar_diags[0]["range"]["start"]["line"], json!(6));
}

#[tokio::test]
async fn subsequent_run_clears_stale_diagnostics() {
    let first = r#"{
        "totals":{"errors":0,"file_errors":1},
        "files":{
            "/proj/src/Foo.php":{"errors":1,"messages":[
                {"message":"oops","line":1,"ignorable":true}
            ]}
        },
        "errors":[]
    }"#;
    let second = r#"{"totals":{"errors":0,"file_errors":0},"files":{},"errors":[]}"#;

    let runner = Arc::new(ScriptedRunner::new(vec![first, second]));
    let (mut c2s, mut s2c) = spawn_server(runner);

    handshake(&mut c2s, &mut s2c).await;

    // Initial run: expect non-empty diagnostics for Foo.
    let initial = read_until(&mut s2c, &mut c2s, |v| {
        v.get("method") == Some(&json!("textDocument/publishDiagnostics"))
            && v["params"]["uri"] == json!("file:///proj/src/Foo.php")
    })
    .await;
    assert_eq!(
        initial["params"]["diagnostics"].as_array().unwrap().len(),
        1
    );

    // Trigger a save → second scripted response is empty → server must
    // publish an *empty* diagnostics array for Foo to clear it.
    c2s.write_all(&frame(&json!({
        "jsonrpc":"2.0","method":"textDocument/didSave",
        "params":{"textDocument":{"uri":"file:///proj/src/Foo.php"}}
    })))
    .await
    .unwrap();

    let cleared = read_until(&mut s2c, &mut c2s, |v| {
        v.get("method") == Some(&json!("textDocument/publishDiagnostics"))
            && v["params"]["uri"] == json!("file:///proj/src/Foo.php")
            && v["params"]["diagnostics"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(false)
    })
    .await;
    assert_eq!(
        cleared["params"]["diagnostics"].as_array().unwrap().len(),
        0
    );
}

#[tokio::test]
async fn emits_work_done_progress_when_client_supports_it() {
    let json = r#"{"totals":{"errors":0,"file_errors":0},"files":{},"errors":[]}"#;
    let runner = Arc::new(ScriptedRunner::new(vec![json]));
    let (mut c2s, mut s2c) = spawn_server(runner);

    // Initialize advertising work-done progress support, then drive the
    // protocol manually so we can assert on the create/begin/end frames
    // emitted around the analysis run.
    c2s.write_all(&frame(&json!({
        "jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"capabilities":{"window":{"workDoneProgress":true}}}
    })))
    .await
    .unwrap();
    // Drain until we get the initialize response. Auto-ack registerCapability
    // along the way (initialized happens after this, so no creates yet).
    loop {
        let v = read_frame(&mut s2c).await;
        if v.get("id") == Some(&json!(1)) {
            break;
        }
        if v.get("method") == Some(&json!("client/registerCapability"))
            && let Some(id) = v.get("id")
        {
            c2s.write_all(&frame(&json!({"jsonrpc":"2.0","id":id,"result":null})))
                .await
                .unwrap();
        }
    }
    c2s.write_all(&frame(&json!({
        "jsonrpc":"2.0","method":"initialized","params":{}
    })))
    .await
    .unwrap();

    // Collect the progress lifecycle without auto-acking the create request.
    let mut create_id: Option<Value> = None;
    let mut saw_begin = false;
    let mut saw_end = false;
    let timeout = tokio::time::Duration::from_secs(5);
    tokio::time::timeout(timeout, async {
        while !saw_end {
            let v = read_frame(&mut s2c).await;
            let method = v.get("method").cloned();
            if method == Some(json!("client/registerCapability"))
                && let Some(id) = v.get("id")
            {
                c2s.write_all(&frame(&json!({"jsonrpc":"2.0","id":id,"result":null})))
                    .await
                    .unwrap();
                continue;
            }
            if method == Some(json!("window/workDoneProgress/create")) {
                let id = v.get("id").cloned().expect("create has id");
                let token = v["params"]["token"].clone();
                assert!(
                    token.as_str().unwrap().starts_with("phpstan-bridge/"),
                    "token must use the documented prefix, got {token}"
                );
                create_id = Some(id.clone());
                c2s.write_all(&frame(&json!({"jsonrpc":"2.0","id":id,"result":null})))
                    .await
                    .unwrap();
                continue;
            }
            if method == Some(json!("$/progress")) {
                let value = &v["params"]["value"];
                match value["kind"].as_str() {
                    Some("begin") => {
                        assert_eq!(value["title"], json!("PHPStan"));
                        assert!(value["message"].is_string());
                        saw_begin = true;
                    }
                    Some("end") => {
                        assert_eq!(value["message"], json!("No issues found"));
                        saw_end = true;
                    }
                    other => panic!("unexpected progress kind: {other:?}"),
                }
            }
        }
    })
    .await
    .expect("timed out waiting for progress lifecycle");
    assert!(create_id.is_some(), "server must request progress create");
    assert!(saw_begin, "server must emit a begin notification");
    assert!(saw_end, "server must emit an end notification");
}

#[tokio::test]
async fn omits_progress_when_client_lacks_capability() {
    // When the client does not advertise workDoneProgress, the server must
    // not send a create request — doing so would be a spec violation. We
    // assert this by asking for a publishDiagnostics frame with a tight
    // timeout: if a create slipped through, the auto-acker would have
    // recorded it (we substitute a custom reader that fails on it).
    let json = r#"{
        "totals":{"errors":0,"file_errors":1},
        "files":{
            "/proj/src/Foo.php":{"errors":1,"messages":[
                {"message":"x","line":1,"ignorable":true}
            ]}
        },
        "errors":[]
    }"#;
    let runner = Arc::new(ScriptedRunner::new(vec![json]));
    let (mut c2s, mut s2c) = spawn_server(runner);

    handshake_with_capabilities(&mut c2s, &mut s2c, json!({})).await;

    let timeout = tokio::time::Duration::from_secs(5);
    tokio::time::timeout(timeout, async {
        loop {
            let v = read_frame(&mut s2c).await;
            let method = v.get("method").cloned();
            if method == Some(json!("client/registerCapability"))
                && let Some(id) = v.get("id")
            {
                c2s.write_all(&frame(&json!({"jsonrpc":"2.0","id":id,"result":null})))
                    .await
                    .unwrap();
                continue;
            }
            assert_ne!(
                method,
                Some(json!("window/workDoneProgress/create")),
                "must not request progress create when client lacks support"
            );
            assert_ne!(
                method,
                Some(json!("$/progress")),
                "must not emit $/progress when client lacks support"
            );
            if method == Some(json!("textDocument/publishDiagnostics")) {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for diagnostics frame");
}

/// Runner that emits a sequence of [`ProgressUpdate`]s through the channel
/// the server attaches, then returns the canned JSON output. Used to verify
/// that progress frames flow end-to-end.
struct StreamingRunner {
    updates: Vec<ProgressUpdate>,
    output: String,
}

#[async_trait]
impl PhpStanRunner for StreamingRunner {
    async fn analyse(&self, req: AnalyseRequest<'_>) -> BridgeResult<PhpStanOutput> {
        let tx = req
            .progress_tx
            .as_ref()
            .expect("server must attach progress_tx when client supports progress");
        for u in &self.updates {
            tx.send(*u).await.expect("send progress");
        }
        PhpStanOutput::from_json(&self.output)
    }
}

#[tokio::test]
async fn streams_progress_reports_with_done_total_and_percentage() {
    let runner = Arc::new(StreamingRunner {
        updates: vec![
            ProgressUpdate {
                done: 10,
                total: 100,
                percentage: 10,
            },
            ProgressUpdate {
                done: 75,
                total: 100,
                percentage: 75,
            },
        ],
        output: r#"{"totals":{"errors":0,"file_errors":0},"files":{},"errors":[]}"#.to_string(),
    });
    let (mut c2s, mut s2c) = spawn_server(runner);

    handshake(&mut c2s, &mut s2c).await;

    // Walk the progress lifecycle, collecting Report frames. We need to
    // auto-ack registerCapability and the workDoneProgress/create request,
    // then assert the Report frames carry the formatted message.
    let mut reports: Vec<Value> = Vec::new();
    let timeout = tokio::time::Duration::from_secs(5);
    tokio::time::timeout(timeout, async {
        loop {
            let v = read_frame(&mut s2c).await;
            let method = v.get("method").cloned();
            if method == Some(json!("client/registerCapability"))
                || method == Some(json!("window/workDoneProgress/create"))
            {
                if let Some(id) = v.get("id") {
                    c2s.write_all(&frame(&json!({"jsonrpc":"2.0","id":id,"result":null})))
                        .await
                        .unwrap();
                }
                continue;
            }
            if method == Some(json!("$/progress")) {
                let kind = v["params"]["value"]["kind"].as_str().unwrap_or("");
                if kind == "report" {
                    reports.push(v.clone());
                }
                if kind == "end" {
                    return;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for progress lifecycle");

    assert_eq!(reports.len(), 2, "expected one report per ProgressUpdate");
    let first = &reports[0]["params"]["value"];
    assert_eq!(first["percentage"], json!(10));
    assert_eq!(first["message"], json!("10 / 100 (10%)"));
    assert_eq!(first["cancellable"], json!(false));
    let second = &reports[1]["params"]["value"];
    assert_eq!(second["percentage"], json!(75));
    assert_eq!(second["message"], json!("75 / 100 (75%)"));
}
