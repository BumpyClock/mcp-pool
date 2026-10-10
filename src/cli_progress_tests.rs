use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::sync::{Notify, oneshot};

use super::*;

#[derive(Default)]
struct Recording {
    bytes: Mutex<Vec<u8>>,
    written: Notify,
}

struct Recorder(Arc<Recording>);

impl Write for Recorder {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.bytes.lock().extend_from_slice(bytes);
        self.0.written.notify_one();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn recording_progress() -> Result<(Progress<Recorder>, Arc<Recording>)> {
    let recording = Arc::new(Recording::default());
    let progress = Progress {
        line: Some(Line {
            writer: Recorder(recording.clone()),
            started: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .context("test clock cannot subtract one second")?,
            frame: 0,
            width: 0,
        }),
    };
    Ok((progress, recording))
}

async fn wait_for_text(recording: &Recording, text: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if String::from_utf8_lossy(&recording.bytes.lock()).contains(text) {
                break;
            }
            recording.written.notified().await;
        }
    })
    .await?;
    Ok(())
}

#[test]
fn interactive_policy_keeps_machine_and_noninteractive_output_clean() {
    for (human, stdout, stderr, terminal, ci, disabled, reference, expected) in [
        (true, true, true, None, None, None, None, true),
        (false, true, true, None, None, None, None, false),
        (true, false, true, None, None, None, None, false),
        (true, true, false, None, None, None, None, false),
        (true, true, true, Some("dumb"), None, None, None, false),
        (true, true, true, None, Some("true"), None, None, false),
        (true, true, true, None, Some("0"), None, None, true),
        (true, true, true, None, Some("FALSE"), None, None, true),
        (true, true, true, None, Some(""), None, None, true),
        (true, true, true, None, None, Some("1"), None, false),
        (true, true, true, None, None, None, Some("1"), false),
    ] {
        assert_eq!(
            enabled(human, stdout, stderr, terminal, ci, disabled, reference),
            expected
        );
    }
}

#[test]
fn shorter_phases_and_cleanup_erase_the_entire_previous_line() -> Result<()> {
    let mut line = Line {
        writer: Vec::new(),
        started: Instant::now(),
        frame: 0,
        width: 0,
    };
    line.draw("Connecting to MCP server")?;
    let width = line.width;
    line.draw("Calling MCP tool")?;
    assert_eq!(line.width, width);
    assert!(String::from_utf8_lossy(&line.writer).contains("\r/ Calling MCP tool (0s)        "));
    line.clear()?;
    assert!(
        line.writer
            .ends_with(format!("\r{}\r", " ".repeat(width)).as_bytes())
    );
    Ok(())
}

#[tokio::test]
async fn completed_operations_do_not_flash_a_progress_line() -> Result<()> {
    let (mut progress, recording) = recording_progress()?;
    assert_eq!(progress.wait("Calling MCP tool", async { 17 }).await, 17);
    progress.finish();
    assert!(recording.bytes.lock().is_empty());
    Ok(())
}

#[tokio::test]
async fn progress_is_visible_before_success_or_failure_and_cleared_afterward() -> Result<()> {
    for outcome in [Ok(17), Err("synthetic failure")] {
        let (mut progress, recording) = recording_progress()?;
        let (complete, completion) = oneshot::channel();
        let operation = tokio::spawn(async move {
            let result = progress.wait("Calling MCP tool", completion).await;
            progress.finish();
            result
        });
        wait_for_text(&recording, "Calling MCP tool").await?;
        assert!(!operation.is_finished());
        complete
            .send(outcome)
            .map_err(|_| anyhow::anyhow!("operation ended early"))?;
        assert_eq!(operation.await??, outcome);
        let captured = recording.bytes.lock();
        assert!(captured.ends_with(b"\r"));
        assert!(!captured.ends_with(b"(1s)"));
    }
    Ok(())
}

#[tokio::test]
async fn cancelling_the_owner_clears_progress_without_a_background_renderer() -> Result<()> {
    let (mut progress, recording) = recording_progress()?;
    let operation = tokio::spawn(async move {
        progress
            .wait("Calling MCP tool", std::future::pending::<()>())
            .await;
    });
    wait_for_text(&recording, "Calling MCP tool").await?;
    operation.abort();
    let cancelled = operation.await.err().context("owner did not cancel")?;
    assert!(cancelled.is_cancelled());
    assert!(recording.bytes.lock().ends_with(b"\r"));
    Ok(())
}

#[tokio::test]
async fn progress_write_failure_does_not_cancel_the_operation() -> Result<()> {
    struct Broken(Arc<Notify>);
    impl Write for Broken {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            self.0.notify_one();
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "synthetic closed terminal",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let attempted = Arc::new(Notify::new());
    let mut progress = Progress {
        line: Some(Line {
            writer: Broken(attempted.clone()),
            started: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .context("test clock cannot subtract one second")?,
            frame: 0,
            width: 0,
        }),
    };
    let (complete, completion) = oneshot::channel();
    let operation =
        tokio::spawn(async move { progress.wait("Calling MCP tool", completion).await });
    tokio::time::timeout(Duration::from_secs(5), attempted.notified()).await?;
    assert!(!operation.is_finished());
    complete
        .send(29)
        .map_err(|_| anyhow::anyhow!("operation was cancelled"))?;
    assert_eq!(operation.await??, 29);
    Ok(())
}
