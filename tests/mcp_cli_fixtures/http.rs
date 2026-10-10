use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

pub struct HttpFixture {
    pub url: String,
    pub initializations: Arc<AtomicUsize>,
    pub headers: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}

impl HttpFixture {
    pub async fn new() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let initializations = Arc::new(AtomicUsize::new(0));
        let headers = Arc::new(Mutex::new(Vec::new()));
        let count = initializations.clone();
        let requests = headers.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await?;
                respond(stream, &count, &requests).await?;
            }
        });
        Ok(Self {
            url,
            initializations,
            headers,
            task,
        })
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn respond(
    stream: TcpStream,
    count: &AtomicUsize,
    requests: &Mutex<Vec<BTreeMap<String, String>>>,
) -> io::Result<()> {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line).await?;
    if line.starts_with("DELETE ") {
        return stream
            .get_mut()
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
    }
    let mut headers = BTreeMap::<String, String>::new();
    loop {
        line.clear();
        if stream.read_line(&mut line).await? == 0 {
            return Err(io::Error::other("fixture request ended before headers"));
        }
        if line == "\r\n" {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("fixture received malformed header"))?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    let length = headers
        .get("content-length")
        .ok_or_else(|| io::Error::other("fixture request omitted length"))?
        .parse::<usize>()
        .map_err(io::Error::other)?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    requests.lock().await.push(headers.clone());
    let request: Value = serde_json::from_slice(&body).map_err(io::Error::other)?;
    let Some(id) = request.get("id") else {
        return stream
            .get_mut()
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
    };
    let result = match request.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            count.fetch_add(1, Ordering::SeqCst);
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},
                "serverInfo":{"name":"http-fixture","version":"1"}})
        }
        Some("tools/list") => json!({"tools":[{
            "name":"headers","inputSchema":{"type":"object","properties":{}}
        }]}),
        Some("tools/call") => json!({
            "content":[{"type":"text","text":"mock HTTP response"}],
            "structuredContent":{"headers":headers}
        }),
        _ => json!({}),
    };
    let body = json!({"jsonrpc":"2.0","id":id,"result":result}).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: mock-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.get_mut().write_all(response.as_bytes()).await
}
