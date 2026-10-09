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
    fn summarize_keeps_short_lines_unchanged() {
        let line = "upstream_stderr ready on port 1234";
        assert_eq!(summarize_log_line(line), line);
    }

    #[test]
    fn summarize_keeps_boundary_length_line_unchanged() {
        let line = "a".repeat(MAX_LOG_LINE_LEN);
        assert_eq!(summarize_log_line(&line), line);
    }

    #[test]
    fn summarize_truncates_long_lines_with_marker_and_no_full_tail() {
        let body = "x".repeat(10_000);
        let out = summarize_log_line(&body);
        assert!(out.contains("truncated=true"), "marker present: {out:.40}");
        assert!(
            out.contains("original_len=10000"),
            "original length recorded"
        );
        assert!(out.len() < body.len(), "output shorter than input");
        assert!(!out.contains(&"x".repeat(10_000)));
        assert!(out.starts_with(&"x".repeat(MAX_LOG_LINE_LEN)));
    }

    #[test]
    fn summarize_does_not_split_multibyte_char() {
        let body = "é".repeat(3000);
        let out = summarize_log_line(&body);
        assert!(out.contains("truncated=true"));
    }
}
