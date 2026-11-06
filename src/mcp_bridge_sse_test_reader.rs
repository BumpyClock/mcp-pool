use anyhow::{Context, Result, ensure};
use serde_json::Value;
use tokio::time::{Duration, timeout};

#[derive(Default)]
struct Frames {
    bytes: Vec<u8>,
}

impl Frames {
    fn take(&mut self) -> Option<Vec<u8>> {
        let end = self.bytes.windows(2).position(|pair| pair == b"\n\n")? + 2;
        Some(self.bytes.drain(..end).collect())
    }
}

pub(super) struct SseReader {
    response: reqwest::Response,
    frames: Frames,
}

impl SseReader {
    pub(super) fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            frames: Frames::default(),
        }
    }

    pub(super) async fn frame(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(frame) = self.frames.take() {
                return Ok(Some(frame));
            }
            let chunk = timeout(Duration::from_secs(2), self.response.chunk())
                .await
                .context("SSE frame deadline exceeded")??;
            let Some(chunk) = chunk else {
                ensure!(
                    self.frames.bytes.is_empty(),
                    "SSE stream ended within a frame"
                );
                return Ok(None);
            };
            self.frames.bytes.extend_from_slice(&chunk);
        }
    }

    pub(super) async fn message(&mut self) -> Result<Value> {
        loop {
            let frame = self
                .frame()
                .await?
                .context("SSE stream ended before its next message")?;
            let frame = std::str::from_utf8(&frame).context("SSE frame had invalid UTF-8")?;
            let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
                continue;
            };
            return serde_json::from_str(data).context("SSE frame had invalid JSON");
        }
    }
}

#[test]
fn buffered_frames_retain_trailing_events_and_split_utf8_bytes() -> Result<()> {
    let mut frames = Frames::default();
    frames.bytes.extend_from_slice(b"data: {\"text\":\"r\xc3");
    assert!(frames.take().is_none());
    frames
        .bytes
        .extend_from_slice(b"\xa9sum\xc3\xa9\"}\n\ndata: {\"next\":true}\n\n");
    let first = frames.take().context("first frame")?;
    assert_eq!(
        std::str::from_utf8(&first)?,
        "data: {\"text\":\"résumé\"}\n\n"
    );
    assert_eq!(
        frames.take().context("trailing frame")?,
        b"data: {\"next\":true}\n\n"
    );
    assert!(frames.take().is_none());
    Ok(())
}
