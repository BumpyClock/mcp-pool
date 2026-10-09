use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

pub const MARKER: &str = "MCP_POOL_TEST_STDIO";
pub const COUNTER: &str = "MCP_POOL_TEST_COUNTER";

fn record(event: &str) -> io::Result<()> {
    let path = std::env::var_os(COUNTER)
        .ok_or_else(|| io::Error::other("fixture counter path is missing"))?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(format!("{event}\n").as_bytes())
}

fn tool(name: &str) -> Value {
    json!({
        "name": name, "description": format!("Fixture {name}"),
        "inputSchema": {"type":"object", "properties":{
            "label":{"type":"string"}, "count":{"type":"integer"},
            "enabled":{"type":"boolean"}, "delay_ms":{"type":"integer"}
        }, "required":["label"]}
    })
}

pub fn response(request: &Value) -> io::Result<Value> {
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let parameters = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => {
            record("initialize")?;
            json!({"protocolVersion":"2025-06-18",
                "capabilities":{"tools":{}, "resources":{}},
                "serverInfo":{"name":"controlled-fixture", "version":"1"}})
        }
        "tools/list" => {
            record("tools/list")?;
            if parameters.get("cursor") == Some(&json!("tools-page-two")) {
                json!({"tools":[tool("delayed"),tool("fail_rpc"),tool("fail_tool")]})
            } else {
                json!({"tools":[tool("echo")], "nextCursor":"tools-page-two"})
            }
        }
        "tools/call" => {
            record("tools/call")?;
            let name = parameters
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name == "fail_rpc" {
                return Ok(json!({"jsonrpc":"2.0", "id":request.get("id"),
                    "error":{"code":-32042,"message":"fixture RPC failure"}}));
            }
            if name == "fail_tool" {
                json!({"isError":true,
                    "content":[{"type":"text","text":"fixture tool failure"}]})
            } else {
                let arguments = parameters
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let milliseconds = arguments
                    .get("delay_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                std::thread::sleep(Duration::from_millis(milliseconds));
                let payload = json!({"arguments":arguments,
                    "cwd":std::env::current_dir()?.canonicalize()?.to_string_lossy(),
                    "environment":std::env::var("MCP_POOL_TEST_VALUE").unwrap_or_default()});
                json!({"content":[{"type":"text","text":payload.to_string()}],
                    "structuredContent":payload})
            }
        }
        "resources/list" => {
            if parameters.get("cursor") == Some(&json!("resources-page-two")) {
                json!({"resources":[{"uri":"fixture://second", "name":"second"}]})
            } else {
                json!({"resources":[{"uri":"fixture://first", "name":"first"}],
                    "nextCursor":"resources-page-two"})
            }
        }
        "resources/read" => json!({"contents":[{
            "uri":parameters.get("uri"), "mimeType":"text/plain", "text":"fixture resource body"
        }]}),
        "ping" => json!({}),
        _ => {
            return Ok(json!({"jsonrpc":"2.0", "id":request.get("id"),
            "error":{"code":-32601,"message":"fixture method not found"}}));
        }
    };
    Ok(json!({"jsonrpc":"2.0", "id":request.get("id"), "result":result}))
}

#[test]
fn upstream_fixture() -> io::Result<()> {
    if std::env::var_os(MARKER).is_none() {
        return Ok(());
    }
    record("spawn")?;
    record(&format!("pid={}", std::process::id()))?;
    let output = Arc::new(Mutex::new(io::stdout()));
    let mut workers = Vec::new();
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?).map_err(io::Error::other)?;
        if request.get("id").is_none() {
            continue;
        }
        let output = output.clone();
        workers.push(std::thread::spawn(move || -> io::Result<()> {
            let response = response(&request)?;
            let mut output = output
                .lock()
                .map_err(|_| io::Error::other("fixture stdout poisoned"))?;
            writeln!(output, "{response}")?;
            output.flush()
        }));
    }
    for worker in workers {
        worker
            .join()
            .map_err(|_| io::Error::other("fixture worker failed"))??;
    }
    Ok(())
}
