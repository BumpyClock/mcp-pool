use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};

pub(crate) use crate::cli_syntax::command_index;
use crate::cli_syntax::{CommandSyntax, GlobalArgument};

static WARNINGS: AtomicBool = AtomicBool::new(true);

pub(super) struct ContextOptions {
    pub path: Option<PathBuf>,
    pub root: Option<PathBuf>,
    pub command: String,
    pub arguments: Vec<String>,
    pub oauth_timeout: Option<u64>,
    pub plain: bool,
    pub help: bool,
}

pub(super) fn parse(syntax: CommandSyntax<'_>) -> Result<ContextOptions> {
    let command_position = syntax.command_position.context("Missing MCP command")?;
    let command = syntax
        .parts
        .iter()
        .find(|part| part.position == command_position)
        .map(|part| part.argument)
        .context("Missing MCP command")?;
    let mut path = None;
    let mut root = None;
    let mut oauth_timeout = None;
    let mut level = None;
    let mut leading_json = false;
    let mut plain = false;
    let mut retained = Vec::new();
    for part in syntax.parts {
        if part.position == command_position {
            continue;
        }
        if let Some(option) = part.global {
            match option.id {
                "config" => path = Some(PathBuf::from(option.value(part.next)?)),
                "root" => root = Some(PathBuf::from(option.value(part.next)?)),
                "log_level" => level = Some(option.value(part.next)?.to_owned()),
                "oauth_timeout" => {
                    oauth_timeout = Some(crate::tool_arguments::positive_milliseconds(
                        option.value(part.next)?,
                    )?)
                }
                "debug" => level = Some("debug".to_owned()),
                "no_color" if part.position < command_position => {
                    if matches!(command, "list" | "describe" | "list-tools") {
                        retained.push(part.argument.to_owned());
                    }
                }
                "plain" => plain = true,
                "json" => leading_json = true,
                _ => retained.push(part.argument.to_owned()),
            }
        } else {
            retained.push(part.argument.to_owned());
        }
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
    resolve_paths(&mut path, &mut root)?;
    if leading_json {
        if command == "call" {
            retained.splice(0..0, ["--output".to_owned(), "json".to_owned()]);
        } else if command != "serve" {
            retained.push("--json".to_owned());
        }
    }
    Ok(ContextOptions {
        path,
        root,
        command: command.to_owned(),
        arguments: retained,
        oauth_timeout,
        plain,
        help: syntax.help,
    })
}

pub(crate) fn global_paths(arguments: &[String]) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    let mut path = None;
    let mut root = None;
    for part in CommandSyntax::parse(arguments).parts {
        if let Some(
            option @ GlobalArgument {
                id: "config" | "root",
                ..
            },
        ) = part.global
        {
            let value = PathBuf::from(option.value(part.next)?);
            if option.id == "config" {
                path = Some(value);
            } else {
                root = Some(value);
            }
        }
    }
    resolve_paths(&mut path, &mut root)?;
    Ok((path, root))
}

fn resolve_paths(path: &mut Option<PathBuf>, root: &mut Option<PathBuf>) -> Result<()> {
    if let Some(directory) = root.as_mut() {
        *directory = crate::config_paths::absolute(directory)?;
        if !directory.is_dir() {
            bail!("--root must name an existing directory");
        }
        if let Some(selected) = path.as_mut()
            && selected.is_relative()
        {
            *selected = directory.join(&*selected);
        }
    }
    Ok(())
}

pub(crate) fn warning(message: &str) {
    if WARNINGS.load(Ordering::SeqCst) {
        eprintln!("[mcp-cli] {message}");
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

    fn parse(arguments: Vec<String>) -> Result<ContextOptions> {
        super::parse(CommandSyntax::parse(&arguments))
    }

    #[test]
    fn leading_globals_do_not_become_commands_or_call_payloads() -> Result<()> {
        let no_color = parse(vec!["--no-color".into(), "list".into(), "fixture".into()])?;
        assert_eq!(no_color.command, "list");
        assert_eq!(no_color.arguments, vec!["--no-color", "fixture"]);
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
    fn explicit_call_option_values_do_not_become_global_intent() -> Result<()> {
        let parsed = parse(vec![
            "call".into(),
            "--server".into(),
            "docs".into(),
            "--tool".into(),
            "--help".into(),
            "--stdio-arg".into(),
            "--plain".into(),
        ])?;
        assert!(!parsed.help);
        assert!(!parsed.plain);
        assert_eq!(
            parsed.arguments,
            [
                "--server",
                "docs",
                "--tool",
                "--help",
                "--stdio-arg",
                "--plain"
            ]
        );
        Ok(())
    }

    #[test]
    fn path_extraction_skips_other_global_values_and_literal_arguments() -> Result<()> {
        let arguments = [
            "--log-level",
            "--config",
            "--config=selected.json",
            "call",
            "docs.echo",
            "--",
            "--config",
            "literal.json",
            "--root=missing",
        ]
        .map(str::to_owned);
        assert_eq!(
            global_paths(&arguments)?,
            (Some(PathBuf::from("selected.json")), None)
        );
        Ok(())
    }

    #[test]
    fn timeout_precedence_and_invalid_environment_match_reference() {
        assert_eq!(timeout(Some(12), Some("20"), Some(30), 60_000), 12);
        assert_eq!(timeout(None, Some("20"), Some(30), 60_000), 20);
        assert_eq!(timeout(None, Some("0"), None, 60_000), 60_000);
        assert_eq!(timeout(None, Some("garbage"), None, 30_000), 30_000);
    }

    #[test]
    fn plain_output_is_explicit_and_literal_payloads_do_not_change_it() -> Result<()> {
        let plain = parse(vec!["--plain".into(), "call".into(), "docs.echo".into()])?;
        assert!(plain.plain);
        let literal = parse(vec![
            "call".into(),
            "docs.echo".into(),
            "--".into(),
            "--plain".into(),
        ])?;
        assert!(!literal.plain);
        assert_eq!(literal.arguments, ["docs.echo", "--", "--plain"]);
        Ok(())
    }
}
