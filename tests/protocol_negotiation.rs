use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::{Json, Router};
use serde_json::{Value, json};

#[path = "mcp_cli_fixtures/stdio.rs"]
mod stdio;
#[path = "mcp_cli_fixtures/support.rs"]
#[allow(dead_code)]
mod support;

use support::{Fixture, RpcProcess, initialize, parse_json};

const MARKER: &str = "MCP_POOL_PROTOCOL_FIXTURE";
const MODE: &str = "MCP_POOL_PROTOCOL_MODE";
const COUNTER: &str = "MCP_POOL_PROTOCOL_COUNTER";

async fn configure(fixture: &Fixture, mode: &str, preference: &str) -> io::Result<()> {
    let configuration = json!({"mcpServers":{"protocol":{
        "command":std::env::current_exe()?,
        "args":["--exact","protocol_upstream_fixture","--quiet","--nocapture"],
        "env":{MARKER:"1",MODE:mode,COUNTER:fixture.counter},
        "protocolVersion":preference,
        "lifecycle":"keep-alive"
    }}});
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    // Drain libtest's stdout preamble without negotiating either protocol.
    let mut proxy = RpcProcess::fixture_proxy(fixture.spawn(&["proxy", "protocol"])?)?;
    let pong = proxy
        .exchange(json!({
            "jsonrpc":"2.0","id":0,"method":"ping","params":{}
        }))
        .await?;
    assert!(pong.get("result").is_some(), "{pong}");
    proxy.finish().await
}

fn record(event: &str) -> io::Result<()> {
    let path = std::env::var_os(COUNTER)
        .ok_or_else(|| io::Error::other("protocol fixture counter missing"))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{event}")
}

#[test]
fn protocol_upstream_fixture() -> io::Result<()> {
    if std::env::var_os(MARKER).is_none() {
        return Ok(());
    }
    record("spawn")?;
    let modern = std::env::var(MODE).map_err(io::Error::other)? == "modern";
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?).map_err(io::Error::other)?;
        let Some(identifier) = request.get("id") else {
            continue;
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        record(method)?;
        let result = match method {
            "ping" => Some(json!({})),
            "server/discover" if modern => Some(json!({
                "supportedVersions":["2026-07-28"],
                "capabilities":{"tools":{}},
                "instructions":"Synthetic modern protocol fixture."
            })),
            "initialize" if !modern => Some(json!({
                "protocolVersion":"2025-11-25","capabilities":{"tools":{}},
                "serverInfo":{"name":"protocol-fixture","version":"1"}
            })),
            "tools/list" => Some(json!({
                "resultType":"complete","ttlMs":0,
                "tools":[{"name":"echo","inputSchema":{
                    "type":"object","properties":{"label":{"type":"string"}},"required":["label"]
                }}]
            })),
            "tools/call" => {
                if modern
                    && request.pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
                        != Some(&json!("2026-07-28"))
                {
                    return Err(io::Error::other("modern call omitted version metadata"));
                }
                Some(json!({
                    "resultType":"complete","content":[],
                    "structuredContent":request.pointer("/params/arguments")
                }))
            }
            _ => None,
        };
        let response = match result {
            Some(result) => json!({"jsonrpc":"2.0","id":identifier,"result":result}),
            None => json!({"jsonrpc":"2.0","id":identifier,
                "error":{"code":-32601,"message":"Fixture method not supported"}}),
        };
        writeln!(io::stdout().lock(), "{response}")?;
        io::stdout().flush()?;
    }
    Ok(())
}

#[tokio::test]
async fn modern_cli_calls_and_bridge_share_one_upstream_without_initialize() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    configure(&fixture, "modern", "auto").await?;
    let listing = parse_json(&fixture.success(&["list", "protocol", "--json"]).await?)?;
    assert_eq!(listing.get("protocolVersion"), Some(&json!("2026-07-28")));
    assert_eq!(listing.get("era"), Some(&json!("modern")));
    assert_eq!(
        listing.get("instructions"),
        Some(&json!("Synthetic modern protocol fixture."))
    );
    let (first, second) = tokio::try_join!(
        fixture.success(&["call", "protocol.echo", "label=first", "--output", "json"]),
        fixture.success(&["call", "protocol.echo", "label=second", "--output", "json"])
    )?;
    assert_eq!(parse_json(&first)?, json!({"label":"first"}));
    assert_eq!(parse_json(&second)?, json!({"label":"second"}));

    let mut bridge =
        RpcProcess::new(fixture.spawn(&["serve", "--stdio", "--servers", "protocol"])?)?;
    assert!(
        bridge
            .exchange(initialize(1))
            .await?
            .get("result")
            .is_some()
    );
    let bridged = bridge
        .exchange(json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"protocol__echo","arguments":{"label":"bridge"}}
        }))
        .await?;
    assert_eq!(
        bridged.pointer("/result/structuredContent"),
        Some(&json!({"label":"bridge"}))
    );
    bridge.finish().await?;
    assert_eq!(fixture.event_count("spawn").await?, 1);
    assert_eq!(fixture.event_count("server/discover").await?, 1);
    assert_eq!(fixture.event_count("initialize").await?, 0);
    assert_eq!(fixture.event_count("tools/call").await?, 3);
    fixture.finish().await
}

#[tokio::test]
async fn legacy_modes_negotiate_once_and_pins_never_fall_back() -> io::Result<()> {
    for preference in ["auto", "legacy", "2026-07-28"] {
        let fixture = Fixture::new().await?;
        configure(&fixture, "legacy", preference).await?;
        let listed = fixture
            .command(&["list", "protocol", "--json", "--exit-code"])
            .await?;
        if preference == "2026-07-28" {
            assert!(!listed.status.success(), "{listed:?}");
            assert_eq!(fixture.event_count("initialize").await?, 0);
            assert_eq!(fixture.event_count("tools/call").await?, 0);
        } else {
            assert!(listed.status.success(), "{listed:?}");
            assert_eq!(
                parse_json(&listed)?.get("protocolVersion"),
                Some(&json!("2025-11-25"))
            );
            assert_eq!(
                parse_json(
                    &fixture
                        .success(&["call", "protocol.echo", "label=legacy", "--output", "json",])
                        .await?
                )?,
                json!({"label":"legacy"})
            );
            assert_eq!(fixture.event_count("initialize").await?, 1);
        }
        assert_eq!(fixture.event_count("spawn").await?, 1);
        assert_eq!(
            fixture.event_count("server/discover").await?,
            usize::from(preference != "legacy")
        );
        fixture.finish().await?;
    }
    Ok(())
}

#[derive(Default)]
struct HttpFixture {
    discoveries: AtomicUsize,
    initializations: AtomicUsize,
    calls: AtomicUsize,
}

async fn modern_http(
    State(state): State<Arc<HttpFixture>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Json<Value> {
    let identifier = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let name = request.pointer("/params/name").and_then(Value::as_str);
    let valid_headers = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
        == Some("2026-07-28")
        && headers
            .get("mcp-method")
            .and_then(|value| value.to_str().ok())
            == Some(method)
        && headers
            .get("mcp-name")
            .and_then(|value| value.to_str().ok())
            == name
        && !headers.contains_key("mcp-session-id")
        && request.pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
            == Some(&json!("2026-07-28"));
    if !valid_headers {
        return Json(json!({"jsonrpc":"2.0","id":identifier,
            "error":{"code":-32600,"message":"Invalid modern fixture transport headers"}}));
    }
    let result = match method {
        "server/discover" => {
            state.discoveries.fetch_add(1, Ordering::SeqCst);
            json!({"supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}})
        }
        "initialize" => {
            state.initializations.fetch_add(1, Ordering::SeqCst);
            return Json(json!({"jsonrpc":"2.0","id":identifier,
                "error":{"code":-32601,"message":"Modern server does not initialize"}}));
        }
        "tools/list" => json!({"tools":[{
            "name":"echo","inputSchema":{"type":"object","properties":{"label":{"type":"string"}}}
        }],"resultType":"complete","ttlMs":0}),
        "tools/call" => {
            state.calls.fetch_add(1, Ordering::SeqCst);
            json!({"content":[],"resultType":"complete",
                "structuredContent":request.pointer("/params/arguments")})
        }
        _ => json!({}),
    };
    Json(json!({"jsonrpc":"2.0","id":identifier,"result":result}))
}

#[tokio::test]
async fn modern_http_cli_sets_transport_headers_and_shares_negotiation() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let state = Arc::new(HttpFixture::default());
    let application = Router::new()
        .route("/mcp", axum::routing::post(modern_http))
        .with_state(state.clone());
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, application)
            .with_graceful_shutdown(async {
                if let Err(error) = stopped.await {
                    eprintln!("HTTP fixture shutdown channel failed: {error}");
                }
            })
            .await
    });
    tokio::fs::write(
        &fixture.config,
        json!({"mcpServers":{"protocol":{
            "url":url,"protocolVersion":"2026-07-28",
            "headers":{"MCP-Method":"must-not-be-forwarded","MCP-Name":"must-not-be-forwarded"}
        }}})
        .to_string(),
    )
    .await?;
    let listed = parse_json(&fixture.success(&["list", "protocol", "--json"]).await?)?;
    assert_eq!(listed.get("era"), Some(&json!("modern")));
    for label in ["first", "second"] {
        let argument = format!("label={label}");
        assert_eq!(
            parse_json(
                &fixture
                    .success(&["call", "protocol.echo", &argument, "--output", "json",])
                    .await?
            )?,
            json!({"label":label})
        );
    }
    assert_eq!(state.discoveries.load(Ordering::SeqCst), 1);
    assert_eq!(state.initializations.load(Ordering::SeqCst), 0);
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
    fixture.finish().await?;
    shutdown
        .send(())
        .map_err(|_| io::Error::other("HTTP fixture already stopped"))?;
    server.await.map_err(io::Error::other)?
}
