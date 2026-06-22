use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Maximum byte length of a single log line preserved verbatim. Lines longer
/// than this (e.g. multi-hundred-KB upstream JSON payloads) are truncated to a
/// prefix plus a marker so logs stay readable and debug mode stays fast.
const MAX_LOG_LINE_LEN: usize = 2000;

/// When set, diagnostic lines are also echoed to stderr. The foreground `serve`
/// daemon enables this so `mcp-pool serve --debug` shows logs in the terminal.
/// The proxy path never enables it: there, stdout is the raw MCP byte stream and
/// stderr is reserved for user-facing errors.
static STDERR: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// Mirror enabled diagnostics to stderr (in addition to the log file).
pub fn set_stderr_mirror(enabled: bool) {
    STDERR.store(enabled, Ordering::SeqCst);
}

/// Enable logging automatically when MCP_POOL_DEBUG is set to a truthy value.
pub fn init_from_env() {
    if let Ok(value) = std::env::var("MCP_POOL_DEBUG") {
        let truthy = matches!(value.as_str(), "1" | "true" | "TRUE" | "yes");
        set_enabled(truthy);
    }
}

pub fn log_dir() -> Option<PathBuf> {
    config::state_dir().ok().map(|dir| dir.join("logs"))
}

/// Summarize a potentially huge log line. Short lines (<= `MAX_LOG_LINE_LEN`
/// bytes) pass through unchanged; longer lines are truncated to the first
/// `MAX_LOG_LINE_LEN` bytes (snapped down to a UTF-8 char boundary) with a
/// `truncated=true original_len=<bytes>` marker appended. Never panics: the
/// boundary search avoids slicing through a multi-byte character.
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

/// Append a diagnostic line to the log file when enabled, and mirror it to stderr
/// when the stderr mirror is on (foreground `serve --debug`). Never writes to
/// stdout: on the proxy path stdout carries the MCP byte stream.
pub fn log(message: impl AsRef<str>) {
    if !is_enabled() {
        return;
    }
    let message = message.as_ref();
    if STDERR.load(Ordering::SeqCst) {
        eprintln!("{message}");
    }
    let Some(dir) = log_dir() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("mcp-pool.log"))
    {
        let _ = writeln!(file, "{}", message);
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
        assert!(out.contains("original_len=10000"), "original length recorded");
        assert!(out.len() < body.len(), "output shorter than input");
        // The full payload tail must never appear in the summarized line.
        assert!(!out.contains(&"x".repeat(10_000)));
        assert!(out.starts_with(&"x".repeat(MAX_LOG_LINE_LEN)));
    }

    #[test]
    fn summarize_does_not_split_multibyte_char() {
        // 'é' is two bytes; a string of them around the boundary must not panic
        // and must remain valid UTF-8 after truncation.
        let body = "é".repeat(3000);
        let out = summarize_log_line(&body);
        assert!(out.contains("truncated=true"));
    }
}
