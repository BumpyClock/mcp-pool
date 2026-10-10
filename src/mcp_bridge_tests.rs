use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::filter::ToolFilter;
use super::names::{decode_tool_name, encode_tool_name};
use super::options::{ServeMode, parse};
use super::rpc;
use super::{BridgeClient, BridgeState, ClientFuture};
#[path = "mcp_bridge_reconnect_tests.rs"]
mod reconnect_tests;
#[path = "mcp_bridge_selection_tests.rs"]
mod selection_tests;

struct SyntheticClient {
    server: String,
    tools: Vec<Value>,
    calls: Arc<Mutex<Vec<Value>>>,
    closed: Arc<AtomicBool>,
    active: Option<Arc<AtomicUsize>>,
    maximum: Option<Arc<AtomicUsize>>,
}

struct CloseOnCancel {
    closed: Arc<AtomicBool>,
    completed: bool,
}

impl Drop for CloseOnCancel {
    fn drop(&mut self) {
        if !self.completed {
            self.closed.store(true, Ordering::SeqCst);
        }
    }
}

impl BridgeClient for SyntheticClient {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn list_tools(&mut self) -> ClientFuture<'_, Vec<Value>> {
        let tools = self.tools.clone();
        Box::pin(async move { Ok(tools) })
    }

    fn request<'a>(&'a mut self, method: &'a str, params: Value) -> ClientFuture<'a, Value> {
        let server = self.server.clone();
        let calls = Arc::clone(&self.calls);
        let active = self.active.clone();
        let maximum = self.maximum.clone();
        let closed = Arc::clone(&self.closed);
        let method = method.to_owned();
        Box::pin(async move {
            let should_stall =
                params.pointer("/arguments/label").and_then(Value::as_str) == Some("timeout");
            calls
                .lock()
                .await
                .push(json!({"server":server, "method":method, "params":params}));
            if should_stall {
                let mut cancellation = CloseOnCancel {
                    closed,
                    completed: false,
                };
                tokio::time::sleep(Duration::from_secs(1)).await;
                cancellation.completed = true;
            }
            if let (Some(active), Some(maximum)) = (active, maximum) {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(count, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(40)).await;
                active.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(json!({"content":[{"type":"text","text":server}],"echo":params}))
        })
    }
}

fn client(server: &str, tools: Vec<Value>) -> SyntheticClient {
    SyntheticClient {
        server: server.to_owned(),
        tools,
        calls: Arc::new(Mutex::new(Vec::new())),
        closed: Arc::new(AtomicBool::new(false)),
        active: None,
        maximum: None,
    }
}

fn tool(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {"query":{"type":"string"}},
            "required": ["query"]
        },
        "outputSchema": {"type":"object", "properties":{"result":{"type":"string"}}}
    })
}

#[test]
fn qualified_tool_names_escape_boundaries_and_decode_longest_server_prefix() {
    assert_eq!(encode_tool_name("alpha", "ping"), "alpha__ping");
    assert_eq!(
        encode_tool_name("alpha-long", "tool__with_separator"),
        "alpha-long__tool%5F%5Fwith_separator"
    );
    assert_eq!(encode_tool_name("alpha", "_ping"), "alpha__%5Fping");
    assert_eq!(encode_tool_name("alpha_", "ping"), "alpha%5F__ping");
    assert_ne!(
        encode_tool_name("alpha", "_ping"),
        encode_tool_name("alpha_", "ping")
    );
    assert_eq!(
        decode_tool_name(
            "alpha-long__tool%5F%5Fwith_separator",
            &["alpha".to_owned(), "alpha-long".to_owned()]
        ),
        Some(("alpha-long".to_owned(), "tool__with_separator".to_owned()))
    );
    assert_eq!(decode_tool_name("alpha__", &["alpha".to_owned()]), None);
}

#[tokio::test]
async fn initialize_and_notifications_follow_mcp_handshake_contract() -> Result<()> {
    let state = state_with_two_clients()?;
    let initialize = rpc::dispatch(
        &state,
        json!({
            "jsonrpc":"2.0",
            "id":"init-1",
            "method":"initialize",
            "params":{
                "protocolVersion":"2025-06-18",
                "capabilities":{},
                "clientInfo":{"name":"fixture","version":"1"}
            }
        }),
        None,
    )
    .await;
    assert_eq!(
        initialize,
        Some(json!({
            "jsonrpc":"2.0",
            "id":"init-1",
            "result":{
                "protocolVersion":"2025-06-18",
                "capabilities":{"tools":{}},
                "serverInfo":{"name":"mcp-pool","version":env!("CARGO_PKG_VERSION")},
                "instructions":"mcp-pool bridge exposing configured keep-alive servers. Tool names are namespaced as server__tool."
            }
        }))
    );
    assert_eq!(
        rpc::dispatch(
            &state,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            None,
        )
        .await,
        None
    );
    Ok(())
}

#[test]
fn modern_protocol_request_falls_back_to_supported_legacy_revision() {
    let message = json!({
        "jsonrpc":"2.0",
        "id":"init-modern",
        "method":"initialize",
        "params":{"protocolVersion":"2026-07-28"}
    });
    assert!(rpc::supports_protocol_version("2026-07-28"));
    assert_eq!(
        rpc::protocol_version_for_request(&message).as_deref(),
        Some("2025-11-25")
    );
}

#[tokio::test]
async fn aggregate_tool_list_preserves_schemas_and_namespaces_tools() -> Result<()> {
    let state = state_with_two_clients()?;
    let response = rpc::dispatch(
        &state,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
        None,
    )
    .await;
    assert_eq!(
        response,
        Some(json!({
            "jsonrpc":"2.0",
            "id":1,
            "result":{"tools":[
                {
                    "name":"alpha__lookup",
                    "description":"[alpha] alpha lookup",
                    "inputSchema":{
                        "type":"object",
                        "properties":{"query":{"type":"string"}},
                        "required":["query"]
                    },
                    "outputSchema":{"type":"object","properties":{"result":{"type":"string"}}}
                },
                {
                    "name":"beta__ping",
                    "description":"[beta] beta ping",
                    "inputSchema":{
                        "type":"object",
                        "properties":{"query":{"type":"string"}},
                        "required":["query"]
                    },
                    "outputSchema":{"type":"object","properties":{"result":{"type":"string"}}}
                }
            ]}
        }))
    );
    Ok(())
}

#[tokio::test]
async fn exposed_tools_normalize_invalid_schemas_and_preserve_extensions() -> Result<()> {
    let state = BridgeState::with_clients(vec![(
        "alpha".to_owned(),
        Box::new(client(
            "alpha",
            vec![
                json!({"name":"", "inputSchema":{"type":"object"}}),
                json!({"description":"missing name"}),
                json!({
                    "name":"normalize", "description":"", "inputSchema":{"type":"array"},
                    "outputSchema":null, "annotations":{"readOnlyHint":true}
                }),
                json!({
                    "name":"preserve", "description":"kept",
                    "inputSchema":{"type":"object","required":["query"],"additionalProperties":false},
                    "outputSchema":{}, "_meta":{"fixture":"extension"}
                }),
            ],
        )),
    )])?;
    assert_eq!(
        state
            .list_tools(None, false)
            .await
            .map_err(|_| anyhow::anyhow!("listing failed"))?,
        vec![
            json!({
                "name":"alpha__normalize", "description":"Tool from MCP server 'alpha'.",
                "inputSchema":{"type":"object"}, "annotations":{"readOnlyHint":true}
            }),
            json!({
                "name":"alpha__preserve", "description":"[alpha] kept",
                "inputSchema":{"type":"object","required":["query"],"additionalProperties":false},
                "outputSchema":{}, "_meta":{"fixture":"extension"}
            }),
        ]
    );
    let bare = state
        .list_tools(Some("alpha"), true)
        .await
        .map_err(|_| anyhow::anyhow!("listing failed"))?;
    assert_eq!(
        bare.first(),
        Some(&json!({
            "name":"normalize", "description":"", "inputSchema":{"type":"object"},
            "annotations":{"readOnlyHint":true}
        }))
    );
    Ok(())
}

#[tokio::test]
async fn tool_call_routes_original_name_and_arguments_and_preserves_id() -> Result<()> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let state = BridgeState::with_clients(vec![(
        "alpha".to_owned(),
        Box::new(SyntheticClient {
            server: "alpha".to_owned(),
            tools: vec![tool("lookup", "lookup")],
            calls: Arc::clone(&calls),
            closed: Arc::new(AtomicBool::new(false)),
            active: None,
            maximum: None,
        }),
    )])?;
    let response = rpc::dispatch(
        &state,
        json!({
            "jsonrpc":"2.0",
            "id":"downstream-id",
            "method":"tools/call",
            "params":{"name":"alpha__lookup","arguments":{"query":"weather"}}
        }),
        None,
    )
    .await;
    let forwarded = json!({
        "server":"alpha",
        "method":"tools/call",
        "params":{"name":"lookup","arguments":{"query":"weather"}}
    });
    assert_eq!(calls.lock().await.as_slice(), &[forwarded]);
    assert_eq!(
        response,
        Some(json!({
            "jsonrpc":"2.0",
            "id":"downstream-id",
            "result":{
                "content":[{"type":"text","text":"alpha"}],
                "echo":{"name":"lookup","arguments":{"query":"weather"}}
            }
        }))
    );
    Ok(())
}

#[tokio::test]
async fn per_server_mode_uses_bare_tool_names() -> Result<()> {
    let state = state_with_two_clients()?;
    let listed = rpc::dispatch(
        &state,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
        Some("beta"),
    )
    .await;
    assert_eq!(
        listed
            .as_ref()
            .and_then(|message| message.get("result"))
            .and_then(|result| result.get("tools"))
            .and_then(Value::as_array)
            .and_then(|tools| tools.first())
            .and_then(|tool| tool.get("name"))
            .and_then(Value::as_str),
        Some("ping")
    );
    let called = rpc::dispatch(
        &state,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ping"}}),
        Some("beta"),
    )
    .await;
    assert_eq!(
        called
            .as_ref()
            .and_then(|message| message.get("result"))
            .and_then(|result| result.get("content"))
            .and_then(Value::as_array)
            .and_then(|content| content.first())
            .and_then(|entry| entry.get("text"))
            .and_then(Value::as_str),
        Some("beta")
    );
    Ok(())
}

#[tokio::test]
async fn independent_server_calls_run_concurrently() -> Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(BridgeState::with_clients(vec![
        (
            "alpha".to_owned(),
            Box::new(SyntheticClient {
                server: "alpha".to_owned(),
                tools: vec![tool("run", "run")],
                calls: Arc::clone(&calls),
                closed: Arc::new(AtomicBool::new(false)),
                active: Some(Arc::clone(&active)),
                maximum: Some(Arc::clone(&maximum)),
            }),
        ),
        (
            "beta".to_owned(),
            Box::new(SyntheticClient {
                server: "beta".to_owned(),
                tools: vec![tool("run", "run")],
                calls,
                closed: Arc::new(AtomicBool::new(false)),
                active: Some(Arc::clone(&active)),
                maximum: Some(Arc::clone(&maximum)),
            }),
        ),
    ])?);
    let alpha_state = Arc::clone(&state);
    let beta_state = Arc::clone(&state);
    let (alpha, beta) = tokio::join!(
        async move {
            rpc::dispatch(
                &alpha_state,
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"alpha__run"}}),
                None,
            )
            .await
        },
        async move {
            rpc::dispatch(
                &beta_state,
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"beta__run"}}),
                None,
            )
            .await
        }
    );
    assert!(alpha.is_some());
    assert!(beta.is_some());
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn allow_and_block_filters_apply_to_listing_and_calls() -> Result<()> {
    let raw = json!({"allowed_tools":["lookup"]});
    let filter = ToolFilter::from_raw(&raw)?;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let state = BridgeState::with_filtered_clients(vec![(
        "alpha".to_owned(),
        Box::new(SyntheticClient {
            server: "alpha".to_owned(),
            tools: vec![tool("lookup", "lookup"), tool("secret", "secret")],
            calls: Arc::clone(&calls),
            closed: Arc::new(AtomicBool::new(false)),
            active: None,
            maximum: None,
        }),
        filter,
    )])?;
    let listed = rpc::dispatch(
        &state,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        None,
    )
    .await;
    assert_eq!(
        listed
            .as_ref()
            .and_then(|message| message.get("result"))
            .and_then(|result| result.get("tools"))
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1)
    );
    let blocked = rpc::dispatch(
        &state,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"alpha__secret"}}),
        None,
    )
    .await;
    assert_eq!(
        blocked
            .as_ref()
            .and_then(|message| message.get("error"))
            .and_then(|error| error.get("code"))
            .and_then(Value::as_i64),
        Some(-32602)
    );
    assert!(calls.lock().await.is_empty());
    assert!(
        ToolFilter::from_raw(&json!({
            "allowedTools":["lookup"],
            "blockedTools":["secret"]
        }))
        .is_err()
    );
    Ok(())
}

#[test]
fn serve_flags_default_to_stdio_and_parse_http_and_server_selection() -> Result<()> {
    assert_eq!(
        parse(Vec::new())?,
        super::options::ServeOptions {
            mode: ServeMode::Stdio,
            servers: None,
        }
    );
    assert_eq!(
        parse(vec![
            "--http".to_owned(),
            "3210".to_owned(),
            "--host=127.0.0.2".to_owned(),
            "--servers".to_owned(),
            "alpha,beta".to_owned(),
        ])?,
        super::options::ServeOptions {
            mode: ServeMode::Http {
                host: "127.0.0.2".to_owned(),
                port: 3210,
            },
            servers: Some(vec!["alpha".to_owned(), "beta".to_owned()]),
        }
    );
    assert!(
        parse(vec![
            "--stdio".to_owned(),
            "--http".to_owned(),
            "1".to_owned()
        ])
        .is_err()
    );
    assert!(parse(vec!["--http=65536".to_owned()]).is_err());
    assert!(parse(vec!["--host=localhost".to_owned()]).is_err());
    Ok(())
}

#[test]
fn serve_flags_keep_last_values_and_validate_conflicts_after_parsing() -> Result<()> {
    assert_eq!(
        parse(
            [
                "--host=localhost",
                "--http=0",
                "--http",
                "3210",
                "--host=127.0.0.2"
            ]
            .map(str::to_owned)
            .to_vec()
        )?,
        super::options::ServeOptions {
            mode: ServeMode::Http {
                host: "127.0.0.2".to_owned(),
                port: 3210,
            },
            servers: None,
        }
    );
    for (arguments, error) in [
        (
            vec!["--http=1", "--stdio"],
            "Flags '--stdio' and '--http' cannot be used together.",
        ),
        (vec!["--stdio", "--http="], "Flag '--http' requires a port."),
        (vec!["--http"], "Flag '--http' requires a port."),
        (
            vec!["--host=localhost"],
            "Flag '--host' can only be used with '--http'.",
        ),
    ] {
        assert_eq!(
            parse(arguments.into_iter().map(str::to_owned).collect())
                .err()
                .ok_or_else(|| anyhow::anyhow!("invalid flags were accepted"))?
                .to_string(),
            error
        );
    }
    Ok(())
}

fn state_with_two_clients() -> Result<BridgeState> {
    BridgeState::with_clients(vec![
        (
            "alpha".to_owned(),
            Box::new(client("alpha", vec![tool("lookup", "alpha lookup")])),
        ),
        (
            "beta".to_owned(),
            Box::new(client("beta", vec![tool("ping", "beta ping")])),
        ),
    ])
}
