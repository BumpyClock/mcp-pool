use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config;

#[path = "daemon_logging.rs"]
pub(crate) mod logging;

#[derive(Clone, Default)]
pub(crate) struct ServerLog {
    server: Option<std::sync::Arc<str>>,
    redactions: std::sync::Arc<[String]>,
}

impl ServerLog {
    pub(crate) fn new(server: &str) -> Self {
        Self {
            server: Some(server.into()),
            ..Self::default()
        }
    }

    pub(crate) fn for_runtime(runtime: &str) -> Self {
        Self::new(&logging::logical_name(runtime))
    }

    pub(crate) fn with_redactions(mut self, values: impl IntoIterator<Item = String>) -> Self {
        let mut redactions: Vec<_> = self
            .redactions
            .iter()
            .cloned()
            .chain(values)
            .filter(|value| !value.is_empty())
            .collect();
        redactions.sort_by_key(|value| std::cmp::Reverse(value.len()));
        redactions.dedup();
        self.redactions = redactions.into();
        self
    }

    pub(crate) fn redact(&self, message: &str) -> String {
        let mut message = message.to_owned();
        for value in self.redactions.iter() {
            message = message.replace(value, "[redacted]");
        }
        message
    }

    pub(crate) fn log(&self, message: impl AsRef<str>) {
        if !is_enabled() {
            return;
        }
        write_record(self.server.as_deref(), &self.redact(message.as_ref()));
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);

const MAX_LOG_LINE_LEN: usize = 2000;

pub(super) static STDERR: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

pub fn set_stderr_mirror(enabled: bool) {
    STDERR.store(enabled, Ordering::SeqCst);
}

pub fn init_from_env() {
    if let Some(value) = crate::environment::string("MCP_CLI_DEBUG") {
        let truthy = matches!(value.as_str(), "1" | "true" | "TRUE" | "yes");
        set_enabled(truthy);
    }
}

pub fn log_dir() -> std::io::Result<PathBuf> {
    Ok(config::state_dir()?.join("logs"))
}

pub fn summarize_log_line(line: &str) -> String {
    let len = line.len();
    if len <= MAX_LOG_LINE_LEN {
        return line.to_string();
    }
    let mut end = MAX_LOG_LINE_LEN;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = line.get(..end).unwrap_or("");
    format!("{prefix} truncated=true original_len={len}")
}

/// Writes diagnostics to the log file and optional stderr, never stdout.
pub fn log(message: impl AsRef<str>) {
    write_record(None, message.as_ref());
}

fn write_record(server: Option<&str>, message: &str) {
    if !is_enabled() {
        return;
    }
    if let Some(result) = logging::write(server, message) {
        if let Err(error) = result {
            eprintln!("mcp-cli: daemon log write failed: {error}");
        }
        return;
    }
    if STDERR.load(Ordering::SeqCst) {
        eprintln!("{message}");
    }
    let dir = match log_dir() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("mcp-cli: diagnostic state directory unavailable: {error}");
            return;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("mcp-cli: diagnostic log directory unavailable: {error}");
        return;
    }
    let result = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("mcp-cli.log"))
        .and_then(|mut file| writeln!(file, "{message}"));
    if let Err(error) = result {
        eprintln!("mcp-cli: diagnostic log write failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_within_the_limit_are_unchanged() {
        for line in [
            "upstream_stderr ready on port 1234".to_owned(),
            "a".repeat(MAX_LOG_LINE_LEN),
        ] {
            assert_eq!(summarize_log_line(&line), line);
        }
    }

    #[test]
    fn long_lines_keep_the_longest_utf8_prefix_and_original_length() {
        let ascii = "x".repeat(10_000);
        assert_eq!(
            summarize_log_line(&ascii),
            format!(
                "{} truncated=true original_len=10000",
                "x".repeat(MAX_LOG_LINE_LEN)
            )
        );
        let limit_inside_two_byte_character = format!("a{}", "é".repeat(3000));
        assert_eq!(
            summarize_log_line(&limit_inside_two_byte_character),
            format!(
                "a{} truncated=true original_len=6001",
                "é".repeat((MAX_LOG_LINE_LEN - 1) / 2)
            )
        );
    }
}
