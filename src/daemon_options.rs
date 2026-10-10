use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;

use anyhow::{Result, bail};

#[derive(Clone, Debug, Default)]
pub(crate) struct Options {
    pub foreground: bool,
    pub json: bool,
    pub log: bool,
    pub log_file: Option<PathBuf>,
    pub servers: BTreeSet<String>,
}

pub(super) fn parse(command: &str, arguments: VecDeque<String>) -> Result<Options> {
    parse_with_environment(command, arguments, |name| std::env::var(name).ok())
}

fn parse_with_environment(
    command: &str,
    mut arguments: VecDeque<String>,
    environment: impl Fn(&str) -> Option<String>,
) -> Result<Options> {
    let mut options = Options {
        foreground: environment("MCPORTER_DAEMON_CHILD").as_deref() == Some("1"),
        log: environment("MCPORTER_DAEMON_LOG").as_deref() == Some("1"),
        log_file: environment("MCPORTER_DAEMON_LOG_PATH")
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from),
        ..Options::default()
    };
    let mut servers = environment("MCPORTER_DAEMON_LOG_SERVERS");
    while let Some(argument) = arguments.pop_front() {
        match argument.as_str() {
            "--json" => options.json = true,
            "--foreground" if matches!(command, "start" | "restart") => options.foreground = true,
            "--log" if matches!(command, "start" | "restart") => options.log = true,
            "--log-file" if matches!(command, "start" | "restart") => {
                options.log_file = Some(PathBuf::from(crate::tool_arguments::value(
                    &mut arguments,
                    &argument,
                )?))
            }
            "--log-servers" if matches!(command, "start" | "restart") => {
                servers = Some(crate::tool_arguments::value(&mut arguments, &argument)?)
            }
            _ => bail!("Unknown daemon {command} flag '{argument}'"),
        }
    }
    options.servers = servers
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect();
    options.log |= options.log_file.is_some() || !options.servers.is_empty();
    if options.log && matches!(command, "start" | "restart") {
        let selected = options
            .log_file
            .take()
            .unwrap_or(crate::config::state_dir()?.join("logs").join("daemon.log"));
        let text = selected.to_string_lossy();
        let selected = if text == "~" || text.starts_with("~/") || text.starts_with("~\\") {
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Home directory unavailable for daemon log"))?;
            home.join(text.get(2..).unwrap_or(""))
        } else {
            selected
        };
        options.log_file = Some(if selected.is_absolute() {
            selected
        } else {
            std::env::current_dir()?.join(selected)
        });
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn logging_flags_and_environment_have_explicit_precedence() -> Result<()> {
        let arguments = [
            "--foreground",
            "--log-file",
            "fixture.log",
            "--log-servers",
            "one,two, one",
            "--json",
        ]
        .map(str::to_owned)
        .into();
        let options = parse_with_environment("start", arguments, |name| {
            (name == "MCPORTER_DAEMON_LOG_PATH").then(|| "ignored.log".to_owned())
        })?;
        assert!(options.foreground && options.log && options.json);
        assert_eq!(options.servers.len(), 2);
        assert!(
            options
                .log_file
                .is_some_and(|path| path.ends_with("fixture.log"))
        );
        assert!(
            parse_with_environment("stop", ["--foreground".to_owned()].into(), |_| None).is_err()
        );
        Ok(())
    }
}
