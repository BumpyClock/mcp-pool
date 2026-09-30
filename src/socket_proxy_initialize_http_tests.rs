use super::initialize_tests::{connect, follower_queued, read, send};
use super::*;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, oneshot};

async fn serve_connection(
    stream: TcpStream,
    initialize_count: Arc<AtomicU32>,
    release_initialize: Arc<Semaphore>,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let method = line.split_whitespace().next().unwrap_or("").to_string();
    let mut content_length = 0;
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().map_err(io::Error::other)?;
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).await?;
    let (status, body, session_header) = if method == "GET" {
        ("405 Method Not Allowed", String::new(), "")
    } else if method == "DELETE" {
        ("200 OK", String::new(), "")
    } else {
        let request: Value = serde_json::from_slice(&body)?;
        if request.get("method").and_then(Value::as_str) == Some("held") {
            initialize_count.fetch_add(1, Ordering::SeqCst);
            release_initialize
                .acquire()
                .await
                .map_err(io::Error::other)?
                .forget();
            (
                "200 OK",
                json!({"jsonrpc":"2.0","id":request.get("id"),"result":{"owner":"A"}}).to_string(),
                "",
            )
        } else if request.get("method").and_then(Value::as_str) == Some("empty-error") {
            initialize_count.fetch_add(1, Ordering::SeqCst);
            ("200 OK", json!({"jsonrpc":"2.0","id":"","error":{"code":-32603,"message":"uncorrelated failure"}}).to_string(), "")
        } else if request.get("method").and_then(Value::as_str) == Some("initialize") {
            initialize_count.fetch_add(1, Ordering::SeqCst);
            release_initialize
                .acquire()
                .await
                .map_err(io::Error::other)?
                .forget();
            (
                "200 OK",
                json!({"jsonrpc":"2.0","id":request.get("id"),"result":{
                    "protocolVersion":"2025-03-26",
                    "capabilities":{},
                    "serverInfo":{"name":"shared-initialize-fixture","version":"1"}
                }})
                .to_string(),
                "Mcp-Session-Id: shared-initialize\r\n",
            )
        } else if request.get("id").is_some() {
            (
                "200 OK",
                json!({"jsonrpc":"2.0","id":request.get("id"),"result":{}}).to_string(),
                "",
            )
        } else {
            ("202 Accepted", String::new(), "")
        }
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{session_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    reader.get_mut().write_all(response.as_bytes()).await?;
    reader.get_mut().shutdown().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_http_empty_id_error_fails_only_its_post_not_held_client() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let count = Arc::new(AtomicU32::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (shutdown, mut shutdown_rx) = oneshot::channel();
    let server_count = count.clone();
    let server_release = release.clone();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    connections.spawn(serve_connection(stream, server_count.clone(), server_release.clone()));
                }
                result = connections.join_next(), if !connections.is_empty() => {
                    if let Some(result) = result {
                        result.map_err(io::Error::other)??;
                    }
                }
            }
        }
        connections.abort_all();
        while let Some(result) = connections.join_next().await {
            match result {
                Ok(result) => result?,
                Err(error) if error.is_cancelled() => {}
                Err(error) => return Err(io::Error::other(error)),
            }
        }
        Ok::<(), io::Error>(())
    });
    let identity = super::lifecycle_tests::proxy();
    let proxy = Arc::new(SocketProxy::new(
        "http-correlation".to_string(),
        identity.socket_path(),
        UpstreamSpec::Http {
            url: format!("http://{address}/mcp"),
            sse: false,
        },
        true,
    ));
    proxy.start().await?;
    let mut first = BufReader::new(connect(&proxy).await?);
    let mut second = BufReader::new(connect(&proxy).await?);
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"A","method":"held"}),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while count.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":"B","method":"empty-error"}),
    )
    .await?;
    let failure = read(&mut second).await?;
    assert_eq!(failure.get("id"), Some(&json!("B")));
    assert!(failure.get("error").is_some());
    let generation = proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    assert_eq!(generation.request_map.lock().len(), 1);
    assert!(
        generation
            .request_map
            .lock()
            .values()
            .any(|pending| pending.original_id == json!("A"))
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), read(&mut first))
            .await
            .is_err()
    );
    release.add_permits(1);
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":"A","result":{"owner":"A"}})
    );
    assert!(generation.request_map.lock().is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), read(&mut second))
            .await
            .is_err()
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "neither request is replayed"
    );
    proxy.stop().await?;
    shutdown
        .send(())
        .map_err(|_| io::Error::other("fixture server disappeared"))?;
    server.await.map_err(io::Error::other)??;
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_http_backend_receives_one_initialize_from_two_concurrent_socket_clients()
-> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let initialize_count = Arc::new(AtomicU32::new(0));
    let release_initialize = Arc::new(Semaphore::new(0));
    let (shutdown_server, mut shutdown_rx) = oneshot::channel();
    let server_count = initialize_count.clone();
    let server_release = release_initialize.clone();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    connections.spawn(serve_connection(
                        stream, server_count.clone(), server_release.clone(),
                    ));
                }
                result = connections.join_next(), if !connections.is_empty() => {
                    if let Some(result) = result {
                        result.map_err(io::Error::other)??;
                    }
                }
            }
        }
        connections.abort_all();
        while let Some(result) = connections.join_next().await {
            match result {
                Ok(result) => result?,
                Err(error) if error.is_cancelled() => {}
                Err(error) => return Err(io::Error::other(error)),
            }
        }
        Ok::<(), io::Error>(())
    });

    let identity = super::lifecycle_tests::proxy();
    let proxy = Arc::new(SocketProxy::new(
        "real-http-initialize".to_string(),
        identity.socket_path(),
        UpstreamSpec::Http {
            url: format!("http://{address}/mcp"),
            sse: false,
        },
        true,
    ));
    drop(identity);
    proxy.start().await?;
    let mut first = connect(&proxy).await?;
    let mut second = connect(&proxy).await?;
    let (first_write, second_write) = tokio::join!(
        send(
            &mut first,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-03-26","capabilities":{}}})
        ),
        send(
            &mut second,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-03-26","capabilities":{}}})
        ),
    );
    first_write?;
    second_write?;
    follower_queued(&proxy).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while initialize_count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    assert_eq!(initialize_count.load(Ordering::SeqCst), 1);
    assert!(!proxy.readiness().mcp_initialize_result_received);
    release_initialize.add_permits(1);
    let mut first = BufReader::new(first);
    let mut second = BufReader::new(second);
    let (first_response, second_response) = tokio::join!(read(&mut first), read(&mut second));
    let first_response = first_response?;
    let second_response = second_response?;
    assert_eq!(first_response, second_response);
    assert_eq!(first_response.get("id"), Some(&json!(1)));
    assert!(first_response.get("result").is_some());
    assert!(proxy.readiness().mcp_initialize_result_received);
    let mut third = connect(&proxy).await?;
    send(
        &mut third,
        json!({"jsonrpc":"2.0","id":"cached","method":"initialize","params":{}}),
    )
    .await?;
    let cached = read(&mut BufReader::new(third)).await?;
    assert_eq!(cached.get("id"), Some(&json!("cached")));
    assert_eq!(cached.get("result"), first_response.get("result"));
    assert_eq!(initialize_count.load(Ordering::SeqCst), 1);
    proxy.stop().await?;
    shutdown_server
        .send(())
        .map_err(|_| io::Error::other("fixture server disappeared"))?;
    server.await.map_err(io::Error::other)??;
    Ok(())
}
