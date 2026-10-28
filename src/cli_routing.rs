use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::CommandFactory;

pub(super) fn top_level_only(command: &str) -> bool {
    matches!(
        command,
        "call" | "resource" | "auth" | "vault" | "config" | "daemon"
    )
}

pub(super) fn parser(native: bool) -> clap::Command {
    let mut parser = super::Cli::command();
    if native {
        parser = parser
            .bin_name("mcp-pool pool")
            .about("Manage native MCP pools")
            .after_help(
                "MCP discovery and tool commands are available outside the pool namespace.",
            );
    }
    for subcommand in parser.get_subcommands_mut() {
        let name = subcommand.get_name().to_owned();
        if native && top_level_only(&name) {
            *subcommand = std::mem::take(subcommand).hide(true);
        } else if !native {
            let description = match name.as_str() {
                "list" => Some("Discover tools from configured MCP servers"),
                "serve" => Some("Expose pooled MCP tools over stdio or HTTP"),
                _ => None,
            };
            if let Some(description) = description {
                *subcommand = std::mem::take(subcommand).about(description);
            }
        }
    }
    parser
}

const NATIVE_COMMANDS: &[&str] = &[
    "start", "stop", "restart", "status", "add", "remove", "proxy", "shutdown",
];

pub(crate) fn is_native_command(command: &str) -> bool {
    NATIVE_COMMANDS.contains(&command) || command == "pool"
}

pub(crate) fn rewrite_help(arguments: Vec<String>, command_position: usize) -> Vec<String> {
    if arguments
        .get(command_position)
        .is_none_or(|argument| argument != "help")
    {
        return arguments;
    }
    let mut rewritten = arguments
        .get(..command_position)
        .unwrap_or_default()
        .to_vec();
    let target = arguments
        .get(command_position + 1)
        .filter(|argument| !argument.starts_with('-'));
    if let Some(target) = target {
        rewritten.push(target.clone());
        rewritten.extend(
            arguments
                .get(command_position + 2..)
                .unwrap_or_default()
                .iter()
                .cloned(),
        );
        rewritten.push("--help".to_owned());
    } else {
        rewritten.push("--help".to_owned());
    }
    rewritten
}

pub(crate) fn infer(
    arguments: &[String],
    command_position: usize,
    config_path: Option<PathBuf>,
    root: Option<&Path>,
) -> Result<Vec<String>> {
    let command = arguments
        .get(command_position)
        .context("Missing command or MCP server")?;
    if crate::mcp_cli::is_mcp_command(command)
        || is_native_command(command)
        || command.starts_with('-')
    {
        bail!("Cannot infer routing for '{command}'");
    }

    let configuration = crate::server_config::load_at(config_path, root)
        .with_context(|| format!("could not resolve command or MCP server '{command}'"))?;
    let remaining = arguments.get(command_position + 1..).unwrap_or_default();

    if configuration.servers.contains_key(command) {
        let next_positional = first_positional(remaining);
        if next_positional.is_some_and(|index| {
            remaining
                .get(index)
                .is_some_and(|argument| argument == "list")
        }) {
            let marker = next_positional.context("missing inferred list selector")?;
            let mut routed = arguments
                .get(..command_position)
                .context("missing inferred MCP command prefix")?
                .to_vec();
            routed.push("list".to_owned());
            routed.push(command.to_owned());
            for (index, argument) in remaining.iter().enumerate() {
                if index != marker {
                    routed.push(argument.clone());
                }
            }
            return Ok(routed);
        }
        if let Some(index) = next_positional {
            return route_server_call(arguments, command_position, index, command);
        }
        let mut routed = arguments
            .get(..command_position)
            .context("missing inferred MCP command prefix")?
            .to_vec();
        routed.push("list".to_owned());
        routed.push(command.to_owned());
        routed.extend_from_slice(remaining);
        return Ok(routed);
    }

    let selector = command
        .as_str()
        .split_once('(')
        .map_or(command.as_str(), |(selector, _)| selector);
    let (server, tool) = split_configured_selector(&configuration, selector)?;
    let mut routed = arguments
        .get(..command_position)
        .context("missing inferred MCP command prefix")?
        .to_vec();
    routed.push("call".to_owned());
    routed.push("--server".to_owned());
    routed.push(server);
    if let Some(suffix) = command
        .strip_prefix(selector)
        .filter(|suffix| !suffix.is_empty())
    {
        routed.push(format!("{tool}{suffix}"));
    } else {
        routed.push("--tool".to_owned());
        routed.push(tool);
    }
    routed.extend_from_slice(
        arguments
            .get(command_position + 1..)
            .context("missing inferred MCP selector")?,
    );
    Ok(routed)
}

fn split_configured_selector(
    configuration: &crate::server_config::ServerConfiguration,
    selector: &str,
) -> Result<(String, String)> {
    let mut prefix = String::new();
    let mut server = None;
    for segment in selector.split('.') {
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(segment);
        if prefix != selector && configuration.servers.contains_key(&prefix) {
            server = Some(prefix.clone());
        }
    }
    let server =
        server.ok_or_else(|| anyhow::anyhow!("unknown command or MCP server '{selector}'"))?;
    let tool = selector
        .strip_prefix(&format!("{server}."))
        .filter(|tool| !tool.is_empty())
        .context("Inferred MCP call is missing a tool name")?;
    Ok((server, tool.to_owned()))
}

fn route_server_call(
    arguments: &[String],
    command_position: usize,
    tool_offset: usize,
    server: &str,
) -> Result<Vec<String>> {
    let tool_position = command_position + 1 + tool_offset;
    let mut routed = arguments
        .get(..command_position)
        .context("missing inferred MCP command prefix")?
        .to_vec();
    let mut global = Vec::new();
    let mut between = Vec::new();
    let mut index = command_position + 1;
    while index < tool_position {
        let argument = arguments
            .get(index)
            .context("missing inferred MCP server argument")?;
        let width = global_argument_width(argument, arguments.get(index + 1));
        if width > 0 {
            global.extend_from_slice(
                arguments
                    .get(index..index + width)
                    .context("missing global argument value")?,
            );
            index += width;
        } else {
            between.push(argument.clone());
            index += 1;
        }
    }
    let tool = arguments
        .get(tool_position)
        .context("missing inferred MCP tool")?;
    routed.extend(global);
    routed.push("call".to_owned());
    routed.push("--server".to_owned());
    routed.push(server.to_owned());
    routed.extend(between);
    if tool.contains('(') {
        routed.push(tool.to_owned());
    } else {
        routed.push("--tool".to_owned());
        routed.push(tool.to_owned());
    }
    routed.extend_from_slice(
        arguments
            .get(tool_position + 1..)
            .context("missing inferred MCP tool")?,
    );
    Ok(routed)
}

fn global_argument_width(argument: &str, next: Option<&String>) -> usize {
    if argument.contains('=') {
        return if matches!(
            argument.split_once('=').map(|(flag, _)| flag),
            Some("--config" | "--root" | "--log-level" | "--oauth-timeout")
        ) {
            1
        } else {
            0
        };
    }
    if matches!(argument, "--debug" | "--no-color" | "--plain") {
        return 1;
    }
    if matches!(
        argument,
        "--config" | "--root" | "--log-level" | "--oauth-timeout"
    ) && next.is_some()
    {
        return 2;
    }
    0
}

fn first_positional(arguments: &[String]) -> Option<usize> {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        if argument == "--" {
            return (index + 1 < arguments.len()).then_some(index + 1);
        }
        if !argument.starts_with('-') {
            return Some(index);
        }
        if argument.split_once('=').is_none()
            && (crate::tool_arguments::options::takes_value(argument)
                || global_argument_width(argument, arguments.get(index + 1)) == 2)
        {
            index += 2;
        } else {
            index += 1;
        }
    }
    None
}

pub(crate) fn global_paths(arguments: &[String]) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    let mut config_path = None;
    let mut root = None;
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        if argument == "--" {
            break;
        }
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(flag, value)| {
                (flag, Some(value))
            });
        let value = match flag {
            "--config" => inline
                .map(str::to_owned)
                .or_else(|| arguments.get(index + 1).cloned())
                .context("--config requires a path")?,
            "--root" => inline
                .map(str::to_owned)
                .or_else(|| arguments.get(index + 1).cloned())
                .context("--root requires a path")?,
            _ => {
                index += 1;
                continue;
            }
        };
        if flag == "--config" {
            config_path = Some(PathBuf::from(value));
        } else {
            root = Some(PathBuf::from(value));
        }
        index += usize::from(inline.is_none()) + 1;
    }
    if let Some(directory) = root.as_mut() {
        *directory = crate::config_paths::absolute(directory)?;
        if !directory.is_dir() {
            bail!("--root must name an existing directory");
        }
        if let Some(path) = config_path.as_mut()
            && path.is_relative()
        {
            *path = directory.join(&*path);
        }
    }
    Ok((config_path, root))
}
