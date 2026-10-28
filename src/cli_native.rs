use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use crate::config_commands::AddOperation;
use serde_json::json;

pub(super) fn build_add_operation(
    name: String,
    url: Option<String>,
    transport: Option<String>,
    command: Option<String>,
    trailing: Vec<String>,
    destination: Option<PathBuf>,
    dry_run: bool,
) -> anyhow::Result<AddOperation> {
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

    let entry = match (url, has_stdio) {
        (Some(_), true) | (None, false) => {
            return Err(anyhow::anyhow!(
                "specify exactly one of: --url <URL>  OR  a stdio command (-- COMMAND...)"
            ));
        }
        (Some(url), false) => {
            let transport = transport
                .map(|value| normalize_transport(&value))
                .transpose()?
                .unwrap_or_else(|| "http".to_string());
            if url.is_empty() {
                // An empty URL previously became an empty stdio command in the native adapter.
                json!({"transport":"stdio","command":"","args":[]})
            } else {
                json!({"transport":transport,"baseUrl":url})
            }
        }
        (None, true) => json!({"transport":"stdio","command":stdio_command,"args":args}),
    };
    AddOperation::native(name, entry, destination, dry_run)
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

    crate::config_commands::remove(path, root, name).await
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
