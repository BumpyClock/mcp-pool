use std::future::Future;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

pub(crate) struct Progress<Writer: Write = io::Stderr> {
    line: Option<Line<Writer>>,
}

struct Line<Writer: Write> {
    writer: Writer,
    started: Instant,
    frame: usize,
    width: usize,
}

impl Progress {
    pub(crate) fn new(human_output: bool) -> Self {
        let enabled = enabled(
            human_output,
            io::stdout().is_terminal(),
            io::stderr().is_terminal(),
            std::env::var("TERM").ok().as_deref(),
            std::env::var("CI").ok().as_deref(),
            std::env::var("MCP_POOL_NO_PROGRESS").ok().as_deref(),
            std::env::var("MCPORTER_NO_SPINNER").ok().as_deref(),
        );
        Self {
            line: enabled.then(|| Line {
                writer: io::stderr(),
                started: Instant::now(),
                frame: 0,
                width: 0,
            }),
        }
    }
}

impl<Writer: Write> Progress<Writer> {
    pub(crate) async fn wait<Operation: Future>(
        &mut self,
        message: &str,
        operation: Operation,
    ) -> Operation::Output {
        if self.line.is_none() {
            return operation.await;
        }
        let mut operation = std::pin::pin!(operation);
        let mut ticks = tokio::time::interval(Duration::from_millis(100));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                result = &mut operation => return result,
                _ = ticks.tick() => {
                    if let Some(line) = &mut self.line
                        && line.started.elapsed() >= Duration::from_millis(150)
                        && let Err(error) = line.draw(message)
                    {
                        crate::diagnostics::log(format!("CLI progress output failed: {error}"));
                        self.finish();
                        return operation.await;
                    }
                }
            }
        }
    }

    pub(crate) fn finish(&mut self) {
        if let Some(mut line) = self.line.take()
            && let Err(error) = line.clear()
        {
            crate::diagnostics::log(format!("CLI progress cleanup failed: {error}"));
        }
    }
}

impl<Writer: Write> Drop for Progress<Writer> {
    fn drop(&mut self) {
        self.finish();
    }
}

impl<Writer: Write> Line<Writer> {
    fn draw(&mut self, message: &str) -> io::Result<()> {
        let frame = match self.frame {
            0 => '|',
            1 => '/',
            2 => '-',
            _ => '\\',
        };
        self.frame = (self.frame + 1) % 4;
        let text = format!("{frame} {message} ({}s)", self.started.elapsed().as_secs());
        self.width = self.width.max(text.len());
        write!(self.writer, "\r{text:width$}", width = self.width)?;
        self.writer.flush()
    }

    fn clear(&mut self) -> io::Result<()> {
        if self.width != 0 {
            write!(self.writer, "\r{:width$}\r", "", width = self.width)?;
            self.writer.flush()?;
        }
        Ok(())
    }
}

fn enabled(
    human_output: bool,
    stdout_terminal: bool,
    stderr_terminal: bool,
    terminal: Option<&str>,
    continuous_integration: Option<&str>,
    disabled: Option<&str>,
    reference_disabled: Option<&str>,
) -> bool {
    human_output
        && stdout_terminal
        && stderr_terminal
        && terminal != Some("dumb")
        && !continuous_integration.is_some_and(|value| {
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        })
        && disabled != Some("1")
        && reference_disabled != Some("1")
}

#[cfg(test)]
#[path = "cli_progress_tests.rs"]
mod tests;
