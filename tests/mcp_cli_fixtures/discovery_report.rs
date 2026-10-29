use std::io;
use std::process::Output;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::stdio::{LIST_GATE, LIST_SIGNAL};
use super::support::{Fixture, parse_json};

struct Placeholders {
    replacements: Vec<(String, &'static str)>,
}

impl Placeholders {
    fn apply(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for (actual, placeholder) in &self.replacements {
            text = text.replace(actual.as_str(), placeholder);
        }
        durations(&text)
    }

    fn json(&self, output: &Output) -> io::Result<String> {
        let parsed = parse_json(output)?;
        let pretty = serde_json::to_string_pretty(&parsed).map_err(io::Error::other)?;
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{pretty}\n")
        );
        let mut normalized = parsed;
        self.value(&mut normalized);
        serde_json::to_string_pretty(&normalized).map_err(io::Error::other)
    }

    fn value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.apply(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.value(item)),
            Value::Object(object) => {
                if object.get("kind") == Some(&json!("local")) && object.contains_key("path") {
                    object.insert("path".to_owned(), json!("<config>"));
                }
                for (key, item) in object.iter_mut() {
                    if key == "durationMs" {
                        *item = json!(0);
                    } else {
                        self.value(item);
                    }
                }
            }
            _ => {}
        }
    }

    fn text(&self, output: &Output) -> String {
        self.apply(&String::from_utf8_lossy(&output.stdout))
    }
}

fn durations(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut digits = String::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        if !digits.is_empty() {
            if character == 'm' && characters.peek() == Some(&'s') {
                normalized.push('N');
            } else {
                normalized.push_str(&digits);
            }
            digits.clear();
        }
        normalized.push(character);
    }
    normalized.push_str(&digits);
    normalized
}

async fn unauthorized_endpoint() -> io::Result<(String, tokio::task::JoinHandle<io::Result<()>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut buffer = [0_u8; 8192];
            let _read = stream.read(&mut buffer).await?;
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
        }
    });
    Ok((url, task))
}

async fn configure(
    fixture: &Fixture,
    servers: impl FnOnce(&Map<String, Value>) -> Map<String, Value>,
) -> io::Result<Placeholders> {
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    let template = configuration
        .pointer("/mcpServers/fixture")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| io::Error::other("missing fixture entry"))?;
    let command = template
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other("missing fixture command"))?
        .to_owned();
    if let Some(object) = configuration.as_object_mut() {
        object.insert("mcpServers".to_owned(), Value::Object(servers(&template)));
    }
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    Ok(Placeholders {
        replacements: vec![(command, "<fixture>")],
    })
}

fn with_environment(template: &Map<String, Value>, name: &str, value: &str) -> Value {
    let mut server = template.clone();
    if let Some(environment) = server.get_mut("env").and_then(Value::as_object_mut) {
        environment.insert(name.to_owned(), json!(value));
    }
    Value::Object(server)
}

const LIST_JSON: &str = r#"{
  "mode": "list",
  "counts": {
    "ok": 1,
    "auth": 1,
    "offline": 0,
    "http": 0,
    "error": 1
  },
  "servers": [
    {
      "name": "alpha",
      "status": "ok",
      "durationMs": 0,
      "transport": "STDIO <fixture>",
      "source": {
        "kind": "local",
        "path": "<config>"
      },
      "protocolVersion": "2025-06-18",
      "era": "legacy",
      "tools": [
        {
          "name": "delayed",
          "description": "Fixture delayed"
        },
        {
          "name": "echo",
          "description": "Fixture echo"
        },
        {
          "name": "fail_rpc",
          "description": "Fixture fail_rpc"
        },
        {
          "name": "fail_tool",
          "description": "Fixture fail_tool"
        }
      ]
    },
    {
      "name": "beta-auth",
      "status": "auth",
      "durationMs": 0,
      "transport": "HTTP <unauthorized>",
      "source": {
        "kind": "local",
        "path": "<config>"
      },
      "error": "MCP protocol discovery failed; no upstream or operation was replayed. For a legacy server that cannot handle discovery, set protocolVersion to legacy: JSON-RPC error -32000: HTTP upstream returned status 401; request was not replayed",
      "issue": {
        "kind": "auth",
        "rawMessage": "MCP protocol discovery failed; no upstream or operation was replayed. For a legacy server that cannot handle discovery, set protocolVersion to legacy: JSON-RPC error -32000: HTTP upstream returned status 401; request was not replayed",
        "statusCode": 401
      },
      "authCommand": "mcp-pool auth beta-auth"
    },
    {
      "name": "gamma-error",
      "status": "error",
      "durationMs": 0,
      "transport": "STDIO <fixture>",
      "source": {
        "kind": "local",
        "path": "<config>"
      },
      "error": "Server 'gamma-error' has invalid tool filters: A server cannot configure both allowedTools and blockedTools.",
      "issue": {
        "kind": "other",
        "rawMessage": "Server 'gamma-error' has invalid tool filters: A server cannot configure both allowedTools and blockedTools."
      }
    }
  ]
}"#;

const LIST_TEXT: &str = "alpha (4 tools; Nms)
beta-auth — MCP protocol discovery failed; no upstream or operation was replayed. For a legacy server that cannot handle discovery, set protocolVersion to legacy: JSON-RPC error -32000: HTTP upstream returned status 401; request was not replayed
gamma-error — Server 'gamma-error' has invalid tool filters: A server cannot configure both allowedTools and blockedTools.
";

const SERVER_JSON: &str = r#"{
  "name": "alpha",
  "status": "ok",
  "durationMs": 0,
  "transport": "STDIO <fixture>",
  "source": {
    "kind": "local",
    "path": "<config>"
  },
  "protocolVersion": "2025-06-18",
  "era": "legacy",
  "tools": [
    {
      "name": "echo",
      "description": "Fixture echo",
      "inputSchema": {
        "type": "object",
        "properties": {
          "label": {
            "type": "string",
            "description": "Label echoed in the result."
          },
          "count": {
            "type": "integer",
            "description": "Number of requested items.",
            "default": 1
          },
          "enabled": {
            "type": "boolean",
            "description": "Enable the fixture option.",
            "default": false
          },
          "delay_ms": {
            "type": "integer",
            "description": "Delay before responding in milliseconds."
          }
        },
        "required": [
          "label"
        ]
      },
      "options": [
        {
          "property": "label",
          "cliName": "label",
          "required": true,
          "type": "string",
          "placeholder": "<label>",
          "description": "Label echoed in the result."
        },
        {
          "property": "count",
          "cliName": "count",
          "required": false,
          "type": "number",
          "placeholder": "<count:number>",
          "description": "Number of requested items.",
          "defaultValue": 1,
          "exampleValue": "1"
        },
        {
          "property": "enabled",
          "cliName": "enabled",
          "required": false,
          "type": "boolean",
          "placeholder": "<enabled:true|false>",
          "description": "Enable the fixture option.",
          "defaultValue": false,
          "exampleValue": "false"
        },
        {
          "property": "delay_ms",
          "cliName": "delay-ms",
          "required": false,
          "type": "number",
          "placeholder": "<delay-ms:number>",
          "description": "Delay before responding in milliseconds.",
          "exampleValue": "1"
        }
      ]
    }
  ],
  "mode": "server"
}"#;

const SERVER_TEXT: &str = r#"alpha

  /**
   * Fixture echo
   *
   * @param label Label echoed in the result.
   * @param count? Number of requested items. Default: 1.
   * @param enabled? Enable the fixture option. Default: false.
   * @param delay_ms? Delay before responding in milliseconds.
   */
  function echo(label: string, count?: number, enabled?: boolean, delay_ms?: number);

  Examples:
    mcp-pool call alpha.echo --args '{"label":"value"}'

  1 tools · Nms · STDIO <fixture>
"#;

const TOOL_SELECTION_JSON: &str = r#"{
  "name": "alpha",
  "status": "error",
  "durationMs": 0,
  "transport": "STDIO <fixture>",
  "source": {
    "kind": "local",
    "path": "<config>"
  },
  "protocolVersion": "2025-06-18",
  "era": "legacy",
  "error": "Unknown tool 'missing' on 'alpha'",
  "issue": {
    "kind": "other",
    "rawMessage": "Unknown tool 'missing' on 'alpha'"
  },
  "tools": [],
  "sources": [
    {
      "kind": "local",
      "path": "<config>"
    }
  ],
  "mode": "server"
}"#;

const TOOL_SELECTION_TEXT: &str = "alpha

  0 tools · Nms · STDIO <fixture>
alpha — Unknown tool 'missing' on 'alpha'
";

const TOOL_SELECTION_STATUS_TEXT: &str = "alpha (0 tools; Nms)
alpha — Unknown tool 'missing' on 'alpha'
";

const DISCOVERY_FAILURE_JSON: &str = r#"{
  "name": "gamma-error",
  "status": "error",
  "durationMs": 0,
  "transport": "STDIO <fixture>",
  "source": {
    "kind": "local",
    "path": "<config>"
  },
  "error": "Server 'gamma-error' has invalid tool filters: A server cannot configure both allowedTools and blockedTools.",
  "issue": {
    "kind": "other",
    "rawMessage": "Server 'gamma-error' has invalid tool filters: A server cannot configure both allowedTools and blockedTools."
  },
  "mode": "server"
}"#;

const DISCOVERY_FAILURE_TEXT: &str = "gamma-error — Server 'gamma-error' has invalid tool filters: A server cannot configure both allowedTools and blockedTools.
";

fn exit_code(output: &Output, expected: i32) {
    assert_eq!(output.status.code(), Some(expected), "{output:?}");
}

#[tokio::test]
async fn discovery_reports_preserve_json_text_and_exit_contracts() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let (url, endpoint) = unauthorized_endpoint().await?;
    let mut placeholders = configure(&fixture, |template| {
        let mut conflicting = template.clone();
        conflicting.insert("allowedTools".to_owned(), json!(["echo"]));
        conflicting.insert("blockedTools".to_owned(), json!(["delayed"]));
        let mut servers = Map::new();
        servers.insert("alpha".to_owned(), Value::Object(template.clone()));
        servers.insert("beta-auth".to_owned(), json!({"url": url.clone()}));
        servers.insert("gamma-error".to_owned(), Value::Object(conflicting));
        servers
    })
    .await?;
    placeholders
        .replacements
        .push((url.clone(), "<unauthorized>"));
    fixture.warm("alpha").await?;

    let list = fixture.command(&["list", "--json", "--no-oauth"]).await?;
    exit_code(&list, 0);
    assert_eq!(placeholders.json(&list)?, LIST_JSON);
    let text = fixture.command(&["list", "--no-oauth"]).await?;
    exit_code(&text, 0);
    assert_eq!(placeholders.text(&text), LIST_TEXT);
    let health = fixture
        .command(&["list", "--no-oauth", "--exit-code"])
        .await?;
    exit_code(&health, 1);
    assert_eq!(placeholders.text(&health), LIST_TEXT);

    let server = fixture.command(&["list", "alpha.echo", "--json"]).await?;
    exit_code(&server, 0);
    assert_eq!(placeholders.json(&server)?, SERVER_JSON);
    let server = fixture.command(&["list", "alpha.echo"]).await?;
    exit_code(&server, 0);
    assert_eq!(placeholders.text(&server), SERVER_TEXT);

    let selection = fixture
        .command(&["list", "alpha.missing", "--json", "--sources"])
        .await?;
    exit_code(&selection, 1);
    assert_eq!(placeholders.json(&selection)?, TOOL_SELECTION_JSON);
    let selection = fixture.command(&["list", "alpha.missing"]).await?;
    exit_code(&selection, 1);
    assert_eq!(placeholders.text(&selection), TOOL_SELECTION_TEXT);
    let selection = fixture
        .command(&["list", "alpha.missing", "--status"])
        .await?;
    exit_code(&selection, 1);
    assert_eq!(placeholders.text(&selection), TOOL_SELECTION_STATUS_TEXT);

    let failure = fixture.command(&["list", "gamma-error", "--json"]).await?;
    exit_code(&failure, 0);
    assert_eq!(placeholders.json(&failure)?, DISCOVERY_FAILURE_JSON);
    let failure = fixture.command(&["list", "gamma-error"]).await?;
    exit_code(&failure, 0);
    assert_eq!(placeholders.text(&failure), DISCOVERY_FAILURE_TEXT);
    let failure = fixture
        .command(&["list", "gamma-error", "--exit-code"])
        .await?;
    exit_code(&failure, 1);
    endpoint.abort();
    fixture.finish().await
}

#[tokio::test]
async fn discovery_output_follows_name_order_when_completion_order_differs() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let gate = fixture.home.join("beta-listed");
    let gate_path = gate.display().to_string();
    configure(&fixture, |template| {
        let mut servers = Map::new();
        servers.insert(
            "alpha".to_owned(),
            with_environment(template, LIST_GATE, &gate_path),
        );
        servers.insert(
            "beta".to_owned(),
            with_environment(template, LIST_SIGNAL, &gate_path),
        );
        servers
    })
    .await?;
    fixture.warm("alpha").await?;
    fixture.warm("beta").await?;
    let listed = parse_json(&fixture.success(&["list", "--json"]).await?)?;
    assert!(gate.try_exists()?, "beta never completed discovery");
    let servers = listed
        .get("servers")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("list omitted servers"))?;
    let order: Vec<_> = servers
        .iter()
        .map(|server| (server.get("name"), server.get("status")))
        .collect();
    assert_eq!(
        order,
        [
            (Some(&json!("alpha")), Some(&json!("ok"))),
            (Some(&json!("beta")), Some(&json!("ok")))
        ]
    );
    tokio::fs::remove_file(&gate).await?;
    let text = fixture.success(&["list"]).await?;
    assert!(gate.try_exists()?, "beta never completed discovery");
    let lines: Vec<_> = String::from_utf8_lossy(&text.stdout)
        .lines()
        .map(|line| line.split(' ').next().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(lines, ["alpha", "beta"]);
    fixture.finish().await
}
