use std::io::{self, IsTerminal, Write};

use crate::config::ServerDef;

pub(super) fn build_server_def(
    url: Option<String>,
    transport: Option<String>,
    command: Option<String>,
    trailing: Vec<String>,
) -> anyhow::Result<ServerDef> {
    let (stdio_command, args) = match (command, trailing.split_first()) {
        (Some(command), None) => (command, Vec::new()),
        (Some(_), Some(_)) => {
            return Err(anyhow::anyhow!(
                "--command cannot be combined with trailing command args"
            ));
        }
        (None, None) => (String::new(), Vec::new()),
        (None, Some((first, rest))) => {
            if first.is_empty() {
                return Err(anyhow::anyhow!("stdio command must not be empty"));
            }
            (first.clone(), rest.to_vec())
        }
    };
    let has_stdio = !stdio_command.is_empty();

    match (url, has_stdio) {
        (Some(_), true) | (None, false) => Err(anyhow::anyhow!(
            "specify exactly one of: --url <URL>  OR  a stdio command (-- COMMAND...)"
        )),
        (Some(url), false) => {
            let transport = transport
                .map(|value| normalize_transport(&value))
                .transpose()?
                .unwrap_or_else(|| "http".to_string());
            Ok(ServerDef {
                url,
                transport,
                ..Default::default()
            })
        }
        (None, true) => Ok(ServerDef {
            command: stdio_command,
            args,
            ..Default::default()
        }),
    }
}

pub(super) fn normalize_transport(value: &str) -> anyhow::Result<String> {
    let normalized = value.to_ascii_lowercase();
    match normalized.as_str() {
        "http" | "sse" => Ok(normalized),
        other => Err(anyhow::anyhow!(
            "invalid --transport '{other}': expected 'http' or 'sse'"
        )),
    }
}

pub(super) async fn add_server(
    name: &str,
    server_def: ServerDef,
    dry_run: bool,
    path: Option<std::path::PathBuf>,
    root: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let mut arguments = vec!["add".to_owned(), name.to_owned()];
    if server_def.is_remote() {
        arguments.extend([
            "--url".into(),
            server_def.url,
            "--transport".into(),
            server_def.transport,
        ]);
    } else {
        arguments.extend(["--command".into(), server_def.command]);
        for argument in server_def.args {
            arguments.extend(["--arg".into(), argument]);
        }
    }
    if dry_run {
        arguments.push("--dry-run".into());
    }
    crate::config_commands::run_with_timeout(path, root, arguments, None).await
}

pub(super) async fn remove_server(
    name: &str,
    yes: bool,
    path: Option<std::path::PathBuf>,
    root: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    if !yes && io::stdin().is_terminal() && !confirm(&format!("remove server '{name}'?")) {
        println!("aborted");
        return Ok(());
    }

    crate::config_commands::run_with_timeout(path, root, vec!["remove".into(), name.into()], None)
        .await
}

fn confirm(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    if io::stdout().flush().is_err() {
        return false;
    }
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
