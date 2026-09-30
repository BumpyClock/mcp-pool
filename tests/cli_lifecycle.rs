use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};

const BINARY: &str = env!("CARGO_BIN_EXE_mcp-pool");
static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct Daemon {
    home: PathBuf,
    child: Child,
}

impl Daemon {
    async fn launch(config: &str) -> io::Result<Self> {
        let home = std::env::temp_dir().join(format!(
            "mcp-pool-cli-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        tokio::fs::create_dir(&home).await?;
        tokio::fs::create_dir(home.join("config")).await?;
        tokio::fs::write(home.join("config").join("config.toml"), config).await?;
        let child = Command::new(BINARY)
            .arg("serve")
            .env("MCP_POOL_HOME", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let daemon = Self { home, child };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if daemon.control_is_live().await {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(io::Error::other)?;
        Ok(daemon)
    }

    async fn control_is_live(&self) -> bool {
        #[cfg(unix)]
        {
            tokio::net::UnixStream::connect(self.home.join("state").join("mcp-pool-control.sock"))
                .await
                .is_ok()
        }
        #[cfg(windows)]
        {
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in self.home.to_string_lossy().as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            let pipe = format!(r"\\.\pipe\mcp-pool-{:08x}-control", hash as u32);
            tokio::net::windows::named_pipe::ClientOptions::new()
                .open(&pipe)
                .is_ok()
        }
    }

    async fn command(&self, args: &[&str]) -> io::Result<std::process::Output> {
        tokio::time::timeout(
            Duration::from_secs(15),
            Command::new(BINARY)
                .args(args)
                .env("MCP_POOL_HOME", &self.home)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(io::Error::other)?
    }

    async fn status(&self) -> io::Result<Value> {
        let output = self.command(&["status", "--json"]).await?;
        assert!(output.status.success(), "status failed: {output:?}");
        serde_json::from_slice(&output.stdout).map_err(io::Error::other)
    }

    fn proxy(&self) -> io::Result<Child> {
        Command::new(BINARY)
            .args(["proxy", "echo"])
            .env("MCP_POOL_HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
    }

    async fn finish(mut self) -> io::Result<()> {
        let output = self.command(&["shutdown"]).await?;
        assert!(output.status.success(), "shutdown failed: {output:?}");
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .map_err(io::Error::other)??;
        assert!(status.success());
        tokio::fs::remove_dir_all(&self.home).await
    }
}

async fn fixture() -> io::Result<(String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let initializations = Arc::new(AtomicUsize::new(0));
    let count = initializations.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let count = count.clone();
            tokio::spawn(async move {
                if let Err(error) = respond(stream, count).await {
                    panic!("HTTP fixture failed: {error}");
                }
            });
        }
    });
    Ok((url, initializations, task))
}

async fn respond(stream: tokio::net::TcpStream, count: Arc<AtomicUsize>) -> io::Result<()> {
    let mut stream = BufReader::new(stream);
    let mut headers = HashMap::new();
    let mut line = String::new();
    stream.read_line(&mut line).await?;
    if line.starts_with("DELETE ") {
        stream
            .get_mut()
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
        return Ok(());
    }
    loop {
        line.clear();
        stream.read_line(&mut line).await?;
        if line == "\r\n" {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("invalid fixture request header"))?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    let length = headers
        .get("content-length")
        .ok_or_else(|| io::Error::other("missing content length"))?
        .parse::<usize>()
        .map_err(io::Error::other)?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    let request: Value = serde_json::from_slice(&body).map_err(io::Error::other)?;
    let method = request.get("method").and_then(Value::as_str);
    let Some(id) = request.get("id") else {
        stream
            .get_mut()
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
        return Ok(());
    };
    let result = if method == Some("initialize") {
        count.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "1"}
        })
    } else {
        assert_eq!(
            headers.get("mcp-session-id").map(String::as_str),
            Some("fixture-session")
        );
        assert_eq!(
            headers.get("mcp-protocol-version").map(String::as_str),
            Some("2025-03-26")
        );
        request.get("params").cloned().unwrap_or(Value::Null)
    };
    let response = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
    let message = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: fixture-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
    stream.get_mut().write_all(message.as_bytes()).await
}

async fn exchange(
    input: &mut tokio::process::ChildStdin,
    output: &mut BufReader<tokio::process::ChildStdout>,
    request: Value,
) -> io::Result<Value> {
    input.write_all(format!("{request}\n").as_bytes()).await?;
    input.flush().await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut line))
        .await
        .map_err(io::Error::other)??;
    serde_json::from_str(&line).map_err(io::Error::other)
}

#[tokio::test]
async fn concurrent_proxy_clients_share_one_initialized_session() -> io::Result<()> {
    let (url, initializations, server) = fixture().await?;
    let daemon = Daemon::launch(&format!(
        "[server.echo]\nurl = {url:?}\ntransport = \"http\"\n"
    ))
    .await?;
    let started = daemon.command(&["start", "echo"]).await?;
    assert!(started.status.success(), "start failed: {started:?}");
    let status = daemon.status().await?;
    assert_eq!(
        status.pointer("/servers/0/readiness/upstream_transport_ready"),
        Some(&json!(true))
    );
    assert_eq!(
        status.pointer("/servers/0/readiness/mcp_initialize_result_received"),
        Some(&json!(false))
    );
    let mut first = daemon.proxy()?;
    let mut second = daemon.proxy()?;
    let mut first_input = first
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing stdin"))?;
    let mut second_input = second
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing stdin"))?;
    let mut first_output = BufReader::new(
        first
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?,
    );
    let mut second_output = BufReader::new(
        second
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?,
    );
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": "cli-test", "version": "1"}}
    });
    let (first_response, second_response) = tokio::try_join!(
        exchange(&mut first_input, &mut first_output, initialize.clone()),
        exchange(&mut second_input, &mut second_output, initialize.clone())
    )?;
    assert_eq!(first_response.get("id"), Some(&json!(1)));
    assert_eq!(first_response.get("result"), second_response.get("result"));
    assert!(first_response.get("error").is_none(), "{first_response}");
    assert!(second_response.get("error").is_none(), "{second_response}");
    assert_eq!(initializations.load(Ordering::SeqCst), 1);
    let (first_response, second_response) = tokio::try_join!(
        exchange(
            &mut first_input,
            &mut first_output,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"caller": "first"}
            })
        ),
        exchange(
            &mut second_input,
            &mut second_output,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"caller": "second"}
            })
        )
    )?;
    assert_eq!(
        first_response,
        json!({"jsonrpc": "2.0", "id": 2, "result": {"caller": "first"}})
    );
    assert_eq!(
        second_response,
        json!({"jsonrpc": "2.0", "id": 2, "result": {"caller": "second"}})
    );
    let status = daemon.status().await?;
    let readiness = status
        .pointer("/servers/0/readiness")
        .ok_or_else(|| io::Error::other("missing readiness"))?;
    assert_eq!(
        readiness.get("upstream_transport_ready"),
        Some(&json!(true))
    );
    assert_eq!(
        readiness.get("mcp_initialize_result_received"),
        Some(&json!(true))
    );
    let table = daemon.command(&["status", "--no-color"]).await?;
    assert!(table.status.success());
    let table = String::from_utf8(table.stdout).map_err(io::Error::other)?;
    assert!(table.contains("READINESS"), "{table}");
    assert!(table.contains("mcp"), "{table}");
    let restarted = daemon.command(&["restart", "echo"]).await?;
    assert!(restarted.status.success(), "restart failed: {restarted:?}");
    let status = daemon.status().await?;
    assert_eq!(
        status.pointer("/servers/0/readiness/mcp_initialize_result_received"),
        Some(&json!(false))
    );
    drop(first_input);
    drop(second_input);
    tokio::time::timeout(Duration::from_secs(5), first.wait())
        .await
        .map_err(io::Error::other)??;
    tokio::time::timeout(Duration::from_secs(5), second.wait())
        .await
        .map_err(io::Error::other)??;
    let mut third = daemon.proxy()?;
    let mut third_input = third
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing stdin"))?;
    let mut third_output = BufReader::new(
        third
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?,
    );
    let response = exchange(&mut third_input, &mut third_output, initialize).await?;
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(initializations.load(Ordering::SeqCst), 2);
    let stopped = daemon.command(&["stop", "echo"]).await?;
    assert!(stopped.status.success(), "stop failed: {stopped:?}");
    let status = daemon.status().await?;
    assert_eq!(status.get("servers"), Some(&json!([])));
    drop(third_input);
    tokio::time::timeout(Duration::from_secs(5), third.wait())
        .await
        .map_err(io::Error::other)??;
    daemon.finish().await?;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn start_reports_missing_executable_and_keeps_daemon_usable() -> io::Result<()> {
    let daemon =
        Daemon::launch("[server.broken]\ncommand = \"mcp-pool-definitely-missing-executable\"\n")
            .await?;
    let output = daemon.command(&["start", "broken"]).await?;
    assert!(
        !output.status.success(),
        "missing executable reported success"
    );
    assert!(!output.stderr.is_empty());
    let status = daemon.status().await?;
    assert_eq!(
        status.pointer("/servers/0/readiness/upstream_transport_ready"),
        Some(&json!(false))
    );
    assert!(
        status
            .pointer("/servers/0/readiness/startup_error")
            .is_some_and(Value::is_string)
    );
    daemon.finish().await
}
