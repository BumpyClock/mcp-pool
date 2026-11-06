use std::io::{self, BufRead, IsTerminal, Write};
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
            if url.is_empty() {
                anyhow::bail!("--url must not be empty");
            }
            let transport = transport
                .map(|value| normalize_transport(&value))
                .transpose()?
                .unwrap_or_else(|| "http".to_string());
            json!({"transport":transport,"baseUrl":url})
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
    if !yes
        && io::stdin().is_terminal()
        && !confirm(
            &format!("remove server '{name}'?"),
            &mut io::stdin().lock(),
            &mut io::stdout().lock(),
        )?
    {
        println!("aborted");
        return Ok(());
    }

    crate::config_commands::remove(path, root, name).await
}

fn confirm(prompt: &str, input: &mut impl BufRead, output: &mut impl Write) -> io::Result<bool> {
    write!(output, "{prompt} [y/N] ")?;
    output.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_distinguishes_declining_from_io_failures() -> io::Result<()> {
        struct FlushFailure;
        impl Write for FlushFailure {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }

        let mut output = Vec::new();
        assert!(confirm("remove?", &mut &b"YES\n"[..], &mut output)?);
        assert!(!confirm("remove?", &mut &b"no\n"[..], &mut output)?);
        let mut invalid_utf8 = &b"\xff\n"[..];
        assert_eq!(
            confirm("remove?", &mut invalid_utf8, &mut output)
                .err()
                .map(|error| error.kind()),
            Some(io::ErrorKind::InvalidData)
        );
        assert_eq!(
            confirm("remove?", &mut &b"yes\n"[..], &mut &mut [][..])
                .err()
                .map(|error| error.kind()),
            Some(io::ErrorKind::WriteZero)
        );
        assert_eq!(
            confirm("remove?", &mut &b"yes\n"[..], &mut FlushFailure)
                .err()
                .map(|error| error.kind()),
            Some(io::ErrorKind::BrokenPipe)
        );
        Ok(())
    }
}
