use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};

static WARNINGS: AtomicBool = AtomicBool::new(true);

pub(super) struct ContextOptions {
    pub path: Option<PathBuf>,
    pub command: String,
    pub arguments: Vec<String>,
    pub oauth_timeout: Option<u64>,
}

pub(crate) fn command_index(arguments: &[String]) -> Option<usize> {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        if matches!(
            argument.as_str(),
            "--config" | "--root" | "--log-level" | "--oauth-timeout"
        ) {
            index += 2;
        } else if matches!(argument.as_str(), "--json" | "--debug")
            || ["--config=", "--root=", "--log-level=", "--oauth-timeout="]
                .iter()
                .any(|prefix| argument.starts_with(prefix))
        {
            index += 1;
        } else {
            return Some(index);
        }
    }
    None
}

pub(super) fn parse(arguments: Vec<String>) -> Result<ContextOptions> {
    let command_position = command_index(&arguments).context("Missing MCP command")?;
    let command = arguments
        .get(command_position)
        .cloned()
        .context("Missing MCP command")?;
    let mut path = None;
    let mut root = None;
    let mut oauth_timeout = None;
    let mut level = None;
    let mut leading_json = false;
    let mut retained = Vec::new();
    let mut arguments = arguments.into_iter().enumerate();
    let mut literal = false;
    while let Some((index, argument)) = arguments.next() {
        if index == command_position {
            continue;
        }
        if argument == "--" {
            literal = true;
        }
        if !literal {
            let (flag, inline) = argument
                .split_once('=')
                .map_or((argument.as_str(), None), |(flag, value)| {
                    (flag, Some(value))
                });
            if matches!(
                flag,
                "--config" | "--root" | "--log-level" | "--oauth-timeout"
            ) {
                let value = inline
                    .map(str::to_owned)
                    .or_else(|| arguments.next().map(|(_, value)| value))
                    .with_context(|| format!("Flag '{flag}' requires a value"))?;
                match flag {
                    "--config" => path = Some(PathBuf::from(value)),
                    "--root" => root = Some(PathBuf::from(value)),
                    "--log-level" => level = Some(value),
                    _ => {
                        oauth_timeout = Some(crate::tool_arguments::positive_milliseconds(&value)?)
                    }
                }
                continue;
            }
            if argument == "--debug" {
                level = Some("debug".to_owned());
                continue;
            }
            if argument == "--json" && index < command_position {
                leading_json = true;
                continue;
            }
        }
        retained.push(argument);
    }
    if let Some(level) = level {
        if !matches!(
            level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error" | "silent"
        ) {
            bail!("--log-level must be trace, debug, info, warn, error, or silent");
        }
        crate::diagnostics::set_enabled(matches!(level.as_str(), "trace" | "debug"));
        WARNINGS.store(
            !matches!(level.as_str(), "error" | "silent"),
            Ordering::SeqCst,
        );
    }
    if let Some(root) = root {
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()?.join(root)
        };
        if !root.is_dir() {
            bail!("--root must name an existing directory");
        }
        if let Some(selected) = path.as_mut() {
            if selected.is_relative() {
                *selected = root.join(&*selected);
            }
        } else if let Some(selected) = std::env::var_os("MCPORTER_CONFIG") {
            let selected = PathBuf::from(selected);
            path = Some(if selected.is_relative() {
                root.join(selected)
            } else {
                selected
            });
        } else {
            let selected = root.join("config").join("mcporter.json");
            match std::fs::metadata(&selected) {
                Ok(metadata) if metadata.is_file() => path = Some(selected),
                Ok(_) => bail!("Root mcporter config is not a file"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Inspecting root mcporter config"),
            }
        }
    }
    if leading_json {
        if command == "call" {
            retained.splice(0..0, ["--output".to_owned(), "json".to_owned()]);
        } else if command != "serve" {
            retained.push("--json".to_owned());
        }
    }
    Ok(ContextOptions {
        path,
        command,
        arguments: retained,
        oauth_timeout,
    })
}

pub(crate) fn warning(message: &str) {
    if WARNINGS.load(Ordering::SeqCst) {
        eprintln!("[mcp-pool] {message}");
    }
}

pub(crate) fn timeout(
    override_ms: Option<u64>,
    environment: Option<&str>,
    configured: Option<u64>,
    fallback: u64,
) -> u64 {
    override_ms
        .or_else(|| {
            environment.and_then(|value| {
                if value.is_empty()
                    || value.starts_with('0')
                    || !value.chars().all(|character| character.is_ascii_digit())
                {
                    return None;
                }
                value.parse().ok().filter(|value| *value > 0)
            })
        })
        .or(configured)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_globals_do_not_become_commands_or_call_payloads() -> Result<()> {
        let parsed = parse(vec!["--json".into(), "list".into()])?;
        assert_eq!(parsed.command, "list");
        assert_eq!(parsed.arguments, vec!["--json"]);
        let parsed = parse(vec![
            "--debug".into(),
            "--json".into(),
            "call".into(),
            "docs.echo".into(),
            "--json".into(),
            r#"{"value":3}"#.into(),
        ])?;
        assert_eq!(parsed.command, "call");
        assert_eq!(
            parsed.arguments,
            vec!["--output", "json", "docs.echo", "--json", r#"{"value":3}"#]
        );
        crate::diagnostics::set_enabled(false);
        Ok(())
    }

    #[test]
    fn timeout_precedence_and_invalid_environment_match_reference() {
        assert_eq!(timeout(Some(12), Some("20"), Some(30), 60_000), 12);
        assert_eq!(timeout(None, Some("20"), Some(30), 60_000), 20);
        assert_eq!(timeout(None, Some("0"), None, 60_000), 60_000);
        assert_eq!(timeout(None, Some("garbage"), None, 30_000), 30_000);
    }
}
