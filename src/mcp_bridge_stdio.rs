use std::io;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::{BridgeState, rpc};

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_IN_FLIGHT: usize = 64;

enum InputFrame {
    Line(Vec<u8>),
    TooLarge,
    End,
    ReadError,
}

pub(super) async fn serve(state: Arc<BridgeState>) -> Result<()> {
    let (sender, receiver) = mpsc::channel(16);
    let reader = tokio::spawn(read_input(sender));
    let outcome = serve_loop(state, receiver).await;
    reader.abort();
    match reader.await {
        Ok(()) => {}
        Err(error) if error.is_cancelled() => {}
        Err(_) => return Err(anyhow::anyhow!("MCP bridge input task failed.")),
    }
    outcome
}

async fn serve_loop(state: Arc<BridgeState>, mut input: mpsc::Receiver<InputFrame>) -> Result<()> {
    let mut output = tokio::io::stdout();
    let mut pending = JoinSet::new();
    let mut input_open = true;
    let mut input_failed = false;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    loop {
        if !input_open && pending.is_empty() {
            break;
        }
        tokio::select! {
            signal_result = &mut shutdown, if input_open => {
                signal_result?;
                input_open = false;
            }
            frame = input.recv(), if input_open && pending.len() < MAX_IN_FLIGHT => {
                match frame {
                    Some(InputFrame::Line(bytes)) => {
                        match serde_json::from_slice(&bytes) {
                            Ok(message) => {
                                let state = Arc::clone(&state);
                                pending.spawn(async move {
                                    rpc::dispatch(&state, message, None).await
                                });
                            }
                            Err(_) => write_response(
                                &mut output,
                                rpc::malformed_frame_response(false),
                            ).await?,
                        }
                    }
                    Some(InputFrame::TooLarge) => {
                        write_response(
                            &mut output,
                            rpc::malformed_frame_response(true),
                        ).await?;
                    }
                    Some(InputFrame::End) => input_open = false,
                    Some(InputFrame::ReadError) | None => {
                        input_open = false;
                        input_failed = true;
                    }
                }
            }
            completed = pending.join_next(), if !pending.is_empty() => {
                match completed {
                    Some(Ok(Some(response))) => write_response(&mut output, response).await?,
                    Some(Ok(None)) => {}
                    Some(Err(_)) => eprintln!("mcp-pool bridge request task failed."),
                    None => {}
                }
            }
        }
    }

    if input_failed {
        bail!("MCP bridge input could not be read.");
    }
    output.flush().await?;
    Ok(())
}

async fn read_input(sender: mpsc::Sender<InputFrame>) {
    let mut input = BufReader::new(tokio::io::stdin());
    loop {
        match read_limited_line(&mut input).await {
            Ok(Some(frame)) => {
                if sender.send(frame).await.is_err() {
                    return;
                }
            }
            Ok(None) => {
                if sender.send(InputFrame::End).await.is_err() {
                    return;
                }
                return;
            }
            Err(_) => {
                if sender.send(InputFrame::ReadError).await.is_err() {
                    return;
                }
                return;
            }
        }
    }
}

async fn read_limited_line<R>(input: &mut R) -> io::Result<Option<InputFrame>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut oversized = false;
    let mut received_any = false;
    loop {
        let available = input.fill_buf().await?;
        if available.is_empty() {
            if !received_any {
                return Ok(None);
            }
            return Ok(Some(if oversized {
                InputFrame::TooLarge
            } else {
                InputFrame::Line(bytes)
            }));
        }

        received_any = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        if !oversized {
            if bytes.len().saturating_add(consumed) > MAX_FRAME_BYTES {
                oversized = true;
            } else {
                bytes.extend_from_slice(&available[..consumed]);
            }
        }
        input.consume(consumed);
        if newline.is_some() {
            return Ok(Some(if oversized {
                InputFrame::TooLarge
            } else {
                InputFrame::Line(bytes)
            }));
        }
    }
}

async fn write_response(output: &mut tokio::io::Stdout, response: serde_json::Value) -> Result<()> {
    output.write_all(response.to_string().as_bytes()).await?;
    output.write_all(b"\n").await?;
    output.flush().await?;
    Ok(())
}
