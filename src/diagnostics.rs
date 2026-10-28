use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config;

#[path = "daemon_logging.rs"]
pub(crate) mod logging;

static ENABLED: AtomicBool = AtomicBool::new(false);

const MAX_LOG_LINE_LEN: usize = 2000;

static STDERR: AtomicBool = AtomicBool::new(false);

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
    if let Ok(value) = std::env::var("MCP_POOL_DEBUG") {
        let truthy = matches!(value.as_str(), "1" | "true" | "TRUE" | "yes");
        set_enabled(truthy);
    }
}

pub fn log_dir() -> Option<PathBuf> {
    config::state_dir().ok().map(|dir| dir.join("logs"))
}

/// Truncates oversized lines at a UTF-8 boundary.
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
    if !is_enabled() {
        return;
    }
    let message = message.as_ref();
    if STDERR.load(Ordering::SeqCst) {
        eprintln!("{message}");
    }
    if let Some(result) = logging::write(message) {
        if let Err(error) = result {
            eprintln!("mcp-pool: daemon log write failed: {error}");
        }
        return;
    }
    let Some(dir) = log_dir() else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("mcp-pool: diagnostic log directory unavailable: {error}");
        return;
    }
    let result = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("mcp-pool.log"))
        .and_then(|mut file| writeln!(file, "{message}"));
    if let Err(error) = result {
        eprintln!("mcp-pool: diagnostic log write failed: {error}");
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
