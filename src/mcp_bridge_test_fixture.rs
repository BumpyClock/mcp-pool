use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{Mutex, Notify, Semaphore, broadcast};

use super::{BridgeState, ReconnectFactory, ServerClient, filter};
use crate::mcp_client::McpClient;

#[derive(Clone)]
pub(super) struct Fixture {
    name: String,
    tools: Vec<Value>,
    calls: Arc<Mutex<Vec<Value>>>,
    connections: Arc<AtomicUsize>,
    remote_eof: Arc<AtomicUsize>,
    disconnected: Arc<AtomicUsize>,
    changed: Arc<Notify>,
    events: broadcast::Sender<Event>,
    release: Arc<Semaphore>,
    activity: Option<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
    client_deadline: Duration,
}

#[derive(Clone, Debug)]
enum Event {
    Notification(Value),
    DisconnectListeners,
}

impl Fixture {
    pub(super) fn new(name: &str, tools: Vec<Value>) -> Self {
        let (events, _) = broadcast::channel(16);
        Self {
            name: name.to_owned(),
            tools,
            calls: Arc::new(Mutex::new(Vec::new())),
            connections: Arc::new(AtomicUsize::new(0)),
            remote_eof: Arc::new(AtomicUsize::new(0)),
            disconnected: Arc::new(AtomicUsize::new(0)),
            changed: Arc::new(Notify::new()),
            events,
            release: Arc::new(Semaphore::new(0)),
            activity: None,
            client_deadline: Duration::from_secs(5),
        }
    }

    pub(super) fn with_client_deadline(mut self, deadline: Duration) -> Self {
        self.client_deadline = deadline;
        self
    }

    pub(super) fn with_activity(
        mut self,
        active: Arc<AtomicUsize>,
        maximum: Arc<AtomicUsize>,
    ) -> Self {
        self.activity = Some((active, maximum));
        self
    }

    pub(super) async fn connect(&self) -> Result<McpClient> {
        let (stream, remote) = tokio::io::duplex(64 * 1024);
        let mut remote = BufReader::new(remote);
        let (client, initialized) = tokio::join!(
            McpClient::initialize(Box::new(stream), self.client_deadline),
            initialize(&mut remote)
        );
        initialized?;
        let client = client?;
        let connection = self.connections.fetch_add(1, Ordering::SeqCst);
        let events = self.events.subscribe();
        let fixture = self.clone();
        self.changed.notify_one();
        tokio::spawn(async move {
            if let Err(error) = fixture.serve(remote, events, connection).await {
                eprintln!("Bridge fixture failed: {error:#}");
            }
        });
        Ok(client)
    }

    fn reconnect(&self) -> ReconnectFactory {
        let fixture = self.clone();
        Arc::new(move || {
            let fixture = fixture.clone();
            Box::pin(async move { fixture.connect().await })
        })
    }

    pub(super) async fn calls(&self) -> Vec<Value> {
        self.calls.lock().await.clone()
    }

    pub(super) fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub(super) fn remote_eof(&self) -> usize {
        self.remote_eof.load(Ordering::SeqCst)
    }

    pub(super) fn release_call(&self) {
        self.release.add_permits(1);
    }

    pub(super) fn notify(&self, notification: Value) -> Result<()> {
        self.events.send(Event::Notification(notification))?;
        Ok(())
    }

    pub(super) fn disconnect_listeners(&self) -> Result<()> {
        self.events.send(Event::DisconnectListeners)?;
        Ok(())
    }

    pub(super) async fn wait_for_disconnect(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let changed = self.changed.notified();
                if self.disconnected.load(Ordering::SeqCst) > 0 {
                    return;
                }
                changed.await;
            }
        })
        .await
        .context("bridge fixture listener did not disconnect")?;
        Ok(())
    }

    pub(super) async fn wait_for(
        &self,
        connections: usize,
        remote_eof: usize,
        calls: usize,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let changed = self.changed.notified();
                if self.connections() >= connections
                    && self.remote_eof() >= remote_eof
                    && self.calls.lock().await.len() >= calls
                {
                    return;
                }
                changed.await;
            }
        })
        .await
        .context("bridge fixture did not observe expected connections, EOFs, and calls")?;
        Ok(())
    }

    async fn serve(
        &self,
        mut remote: BufReader<DuplexStream>,
        mut events: broadcast::Receiver<Event>,
        connection: usize,
    ) -> Result<()> {
        let mut frame = Vec::new();
        loop {
            tokio::select! {
                result = remote.read_until(b'\n', &mut frame) => {
                    if result? == 0 {
                        self.remote_eof.fetch_add(1, Ordering::SeqCst);
                        self.changed.notify_one();
                        return Ok(());
                    }
                    let request: Value = serde_json::from_slice(&frame)?;
                    frame.clear();
                    let method = request.get("method").and_then(Value::as_str).context("fixture request omitted method")?;
                    let result = match method {
                        "tools/list" => json!({"tools":self.tools}),
                        "tools/call" => {
                            let params = request.get("params").context("fixture call omitted params")?.clone();
                            self.calls.lock().await.push(json!({"server":self.name, "method":method, "params":params}));
                            self.changed.notify_one();
                            if matches!(params.pointer("/arguments/label").and_then(Value::as_str), Some("timeout" | "hold")) {
                                let mut next = String::new();
                                tokio::select! {
                                    permit = self.release.acquire() => {
                                        permit?.forget();
                                    }
                                    result = remote.read_line(&mut next) => {
                                        if result? == 0 {
                                            self.remote_eof.fetch_add(1, Ordering::SeqCst);
                                            self.changed.notify_one();
                                            return Ok(());
                                        }
                                        bail!("unexpected request while call was held");
                                    }
                                }
                            }
                            if let Some((active, maximum)) = &self.activity {
                                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                                maximum.fetch_max(count, Ordering::SeqCst);
                                tokio::time::sleep(Duration::from_millis(40)).await;
                                active.fetch_sub(1, Ordering::SeqCst);
                            }
                            json!({"content":[{"type":"text","text":self.name}],"echo":params})
                        }
                        _ => bail!("unexpected bridge fixture request: {method}"),
                    };
                    reply(&mut remote, &request, result).await?;
                }
                event = events.recv() => {
                    match event? {
                        Event::Notification(message) => send(&mut remote, message).await?,
                        Event::DisconnectListeners if connection > 0 => {
                            self.disconnected.fetch_add(1, Ordering::SeqCst);
                            self.changed.notify_one();
                            return Ok(());
                        }
                        Event::DisconnectListeners => {}
                    }
                }
            }
        }
    }
}

pub(super) async fn state(
    fixtures: Vec<(Fixture, filter::ToolFilter, Duration)>,
) -> Result<BridgeState> {
    let mut server_order = Vec::with_capacity(fixtures.len());
    let mut clients = std::collections::BTreeMap::new();
    for (fixture, filter, timeout) in fixtures {
        let name = fixture.name.clone();
        let server = ServerClient {
            client: Arc::new(Mutex::new(fixture.connect().await?)),
            reconnect: fixture.reconnect(),
            filter,
            timeout,
        };
        if clients.insert(name.clone(), server).is_some() {
            bail!("Bridge fixture server name must be unique.");
        }
        server_order.push(name);
    }
    Ok(BridgeState {
        server_order,
        clients,
    })
}

pub(super) async fn default_state(fixtures: Vec<Fixture>) -> Result<BridgeState> {
    state(
        fixtures
            .into_iter()
            .map(|fixture| {
                (
                    fixture,
                    filter::ToolFilter::default(),
                    Duration::from_secs(5),
                )
            })
            .collect(),
    )
    .await
}

async fn initialize(remote: &mut BufReader<DuplexStream>) -> Result<()> {
    let mut frame = String::new();
    anyhow::ensure!(
        remote.read_line(&mut frame).await? > 0,
        "missing initialize request"
    );
    let request: Value = serde_json::from_str(&frame)?;
    anyhow::ensure!(
        request.get("method") == Some(&json!("initialize")),
        "unexpected initialize request"
    );
    anyhow::ensure!(
        request.pointer("/params/protocolVersion") == Some(&json!("2025-11-25"))
            && request.pointer("/params/clientInfo/name") == Some(&json!("mcp-pool")),
        "unexpected initialize metadata"
    );
    reply(
        remote,
        &request,
        json!({
            "protocolVersion":"2025-11-25",
            "capabilities":{"tools":{}},
            "serverInfo":{"name":"fixture","version":"1"}
        }),
    )
    .await?;
    frame.clear();
    anyhow::ensure!(
        remote.read_line(&mut frame).await? > 0,
        "missing initialized notification"
    );
    let notification: Value = serde_json::from_str(&frame)?;
    anyhow::ensure!(
        notification.get("method") == Some(&json!("notifications/initialized")),
        "unexpected initialized notification"
    );
    Ok(())
}

async fn reply(remote: &mut BufReader<DuplexStream>, request: &Value, result: Value) -> Result<()> {
    let id = request.get("id").context("fixture request omitted id")?;
    send(remote, json!({"jsonrpc":"2.0","id":id,"result":result})).await
}

async fn send(remote: &mut BufReader<DuplexStream>, message: Value) -> Result<()> {
    remote
        .get_mut()
        .write_all(format!("{message}\n").as_bytes())
        .await?;
    Ok(())
}
