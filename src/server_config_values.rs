use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

pub(super) fn caller_environment() -> Result<std::collections::BTreeMap<String, String>> {
    environment_from_os(std::env::vars_os())
}

pub(super) fn environment_from_os(
    entries: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Result<std::collections::BTreeMap<String, String>> {
    entries
        .into_iter()
        .map(|(name, value)| {
            let name = name
                .into_string()
                .map_err(|_| anyhow!("caller environment contains a non-UTF-8 variable name"))?;
            let value = value
                .into_string()
                .map_err(|_| anyhow!("caller environment contains a non-UTF-8 variable value"))?;
            Ok((name, value))
        })
        .collect()
}

pub(super) fn windows_absolute(value: &str) -> bool {
    let mut characters = value.chars();
    matches!(
        (characters.next(), characters.next(), characters.next()),
        (Some(drive), Some(':'), Some('\\' | '/')) if drive.is_ascii_alphabetic()
    ) || value.starts_with(r"\\")
}

pub(super) fn looks_like_path(value: &str) -> bool {
    windows_absolute(value) || value.starts_with(['/', '~']) || value.contains(['/', '\\'])
}

pub(super) fn expand_home(path: &Path) -> Result<PathBuf> {
    let value = path.to_string_lossy();
    let suffix = if value == "~" {
        Some("")
    } else {
        value
            .strip_prefix("~/")
            .or_else(|| value.strip_prefix("~\\"))
    };
    match suffix {
        Some(suffix) => Ok(dirs::home_dir()
            .ok_or_else(|| anyhow!("home directory not found"))?
            .join(suffix)),
        None => Ok(path.to_path_buf()),
    }
}

pub(super) fn resolve_path(path: &Path, directory: &Path) -> Result<PathBuf> {
    let path = expand_home(path)?;
    if path.is_absolute() || windows_absolute(&path.to_string_lossy()) {
        Ok(path)
    } else {
        Ok(directory.join(path))
    }
}

pub(super) fn existing_command_path(command: &str, directory: &Path) -> Result<bool> {
    if !command.contains(' ') || !looks_like_path(command) || command.starts_with(['"', '\'']) {
        return Ok(false);
    }
    let path = resolve_path(Path::new(command), directory)?;
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("could not inspect command path"),
    }
}

pub(super) fn command_tokens(command: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    let mut characters = command.trim().chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\'' | '"' if quote == Some(character) => quote = None,
            '\'' | '"' if quote.is_none() => {
                quote = Some(character);
                started = true;
            }
            '\\' if !windows_absolute(&current) => {
                match characters.peek().copied() {
                    Some(next) if next == '"' || next == '\'' || next.is_whitespace() => {
                        current.push(next);
                        characters.next();
                    }
                    _ => current.push(character),
                }
                started = true;
            }
            character if character.is_whitespace() && quote.is_none() => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            character => {
                current.push(character);
                started = true;
            }
        }
    }
    if quote.is_some() {
        bail!("unclosed quote in command string");
    }
    if started {
        tokens.push(current);
    }
    Ok(tokens)
}

fn valid_environment_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(character) if character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

pub(super) fn expand_environment(
    value: &str,
    environment: &impl Fn(&str) -> Result<Option<String>>,
) -> Result<String> {
    if value.contains("${env:") {
        bail!("unsupported environment placeholder; use ${{VAR}}, ${{VAR:-fallback}}, or $env:VAR");
    }
    if let Some(name) = value.strip_prefix("$env:") {
        if !valid_environment_name(name) {
            bail!("invalid whole-value environment placeholder");
        }
        return environment(name)?
            .ok_or_else(|| anyhow!("required environment variable '{name}' is missing"));
    }
    let mut result = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(position) = remaining.find("${") {
        let prefix = remaining
            .get(..position)
            .ok_or_else(|| anyhow!("invalid environment placeholder"))?;
        let after = remaining
            .get(position + 2..)
            .ok_or_else(|| anyhow!("invalid environment placeholder"))?;
        let Some(end) = after.find('}') else {
            result.push_str(remaining);
            return Ok(result);
        };
        let placeholder = after
            .get(..end)
            .ok_or_else(|| anyhow!("invalid environment placeholder"))?;
        let (name, fallback) = match placeholder.split_once(":-") {
            Some((name, fallback)) => (name, Some(fallback)),
            None => (placeholder, None),
        };
        if !valid_environment_name(name) {
            result.push_str(prefix);
            result.push_str("${");
            remaining = after;
            continue;
        }
        result.push_str(prefix.strip_suffix('\\').unwrap_or(prefix));
        let existing = environment(name)?;
        match (existing, fallback) {
            (Some(existing), _) if !existing.is_empty() => result.push_str(&existing),
            (_, Some(fallback)) => result.push_str(fallback),
            (Some(_), None) => {}
            (None, None) => bail!("required environment variable '{name}' is missing"),
        }
        remaining = after
            .get(end + 1..)
            .ok_or_else(|| anyhow!("invalid environment placeholder"))?;
    }
    result.push_str(remaining);
    Ok(result)
}
