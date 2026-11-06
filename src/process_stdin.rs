use std::io::{self, Cursor, Read};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc;

pub(crate) struct Input {
    received: mpsc::Receiver<io::Result<Vec<u8>>>,
    pending: Cursor<Vec<u8>>,
}

pub(crate) fn stdin() -> io::Result<Input> {
    Input::spawn(io::stdin())
}

impl Input {
    fn spawn(mut reader: impl Read + Send + 'static) -> io::Result<Self> {
        let (sender, received) = mpsc::channel(1);
        // OS stdin reads cannot be cancelled. Keep them outside Tokio's blocking
        // pool so runtime shutdown can still wait for credential and file cleanup.
        drop(
            std::thread::Builder::new()
                .name("mcp-cli-stdin".into())
                .spawn(move || {
                    let mut buffer = [0; 8192];
                    loop {
                        let message = match reader.read(&mut buffer) {
                            Ok(0) => return,
                            Ok(count) => Ok(buffer.get(..count).unwrap_or_default().to_vec()),
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            Err(error) => Err(error),
                        };
                        let failed = message.is_err();
                        if sender.blocking_send(message).is_err() || failed {
                            return;
                        }
                    }
                })?,
        );
        Ok(Self {
            received,
            pending: Cursor::new(Vec::new()),
        })
    }
}

impl AsyncRead for Input {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let input = self.get_mut();
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            let count = input.pending.read(output.initialize_unfilled())?;
            if count > 0 {
                output.advance(count);
                return Poll::Ready(Ok(()));
            }
            match input.received.poll_recv(context) {
                Poll::Ready(Some(Ok(bytes))) => input.pending = Cursor::new(bytes),
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc as channel;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn preserves_bytes_through_partial_reads_and_reports_eof() -> io::Result<()> {
        let expected = b"stdin bytes\n".repeat(10_000);
        let mut input = Input::spawn(Cursor::new(expected.clone()))?;
        let mut received = Vec::new();
        let mut buffer = [0; 17];
        loop {
            let count = input.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            received.extend_from_slice(buffer.get(..count).unwrap_or_default());
        }
        assert_eq!(received, expected);
        Ok(())
    }

    struct FailedInput;

    impl Read for FailedInput {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    #[tokio::test]
    async fn preserves_input_errors_instead_of_reporting_eof() -> io::Result<()> {
        let mut input = Input::spawn(FailedInput)?;
        assert_eq!(
            input.read_u8().await.err().map(|error| error.kind()),
            Some(io::ErrorKind::BrokenPipe)
        );
        Ok(())
    }

    struct BlockedInput {
        entered: channel::Sender<()>,
        release: channel::Receiver<()>,
    }

    impl Read for BlockedInput {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            self.entered.send(()).map_err(io::Error::other)?;
            self.release.recv().map_err(io::Error::other)?;
            Ok(0)
        }
    }

    #[test]
    fn cancelled_input_does_not_keep_the_runtime_alive() -> io::Result<()> {
        let (entered, observed) = channel::channel();
        let (release, released) = channel::channel();
        let (finished, completion) = channel::channel();
        let owner = std::thread::Builder::new().spawn(move || -> io::Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread().build()?;
            let input = Input::spawn(BlockedInput {
                entered,
                release: released,
            })?;
            drop(input);
            drop(runtime);
            finished.send(()).map_err(io::Error::other)
        })?;
        let began = observed.recv_timeout(Duration::from_secs(2));
        let exited = completion.recv_timeout(Duration::from_secs(2));
        release.send(()).map_err(io::Error::other)?;
        owner
            .join()
            .map_err(|_| io::Error::other("stdin fixture owner failed"))??;
        began.map_err(io::Error::other)?;
        exited.map_err(io::Error::other)
    }
}
