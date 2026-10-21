use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub(crate) fn home_directory() -> Result<PathBuf> {
    match std::env::var_os("MCP_POOL_HOME") {
        Some(home) => Ok(PathBuf::from(home).join("config")),
        None => Ok(dirs::home_dir()
            .context("Home directory unavailable")?
            .join(".mcp")),
    }
}

pub(crate) fn home_config() -> Result<PathBuf> {
    Ok(home_directory()?.join("config.json"))
}

pub(crate) fn project_directory(root: Option<&Path>) -> Result<PathBuf> {
    let current = std::env::current_dir()?;
    let selected = root.unwrap_or(&current);
    let selected = absolute(selected)?;
    anyhow::ensure!(
        selected.is_dir(),
        "Project root must be an existing directory"
    );
    if root.is_some() {
        return Ok(selected);
    }
    let home = dirs::home_dir().map(|path| absolute(&path)).transpose()?;
    for ancestor in selected.ancestors() {
        if ancestor.join(".git").exists()
            || (Some(ancestor) != home.as_deref() && ancestor.join(".mcp").is_dir())
        {
            return Ok(ancestor.to_path_buf());
        }
    }
    Ok(selected)
}

pub(crate) fn project_config(root: Option<&Path>) -> Result<PathBuf> {
    Ok(project_directory(root)?.join(".mcp").join("config.json"))
}

pub(crate) fn explicit(path: Option<PathBuf>) -> Result<Option<PathBuf>> {
    path.or_else(|| std::env::var_os("MCP_POOL_CONFIG").map(PathBuf::from))
        .map(|path| absolute(&path))
        .transpose()
}

pub(crate) fn destination(path: Option<PathBuf>, root: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit(path)? {
        return Ok(path);
    }
    let project = project_config(root)?;
    match std::fs::metadata(&project) {
        Ok(metadata) if metadata.is_file() => Ok(project),
        Ok(_) => anyhow::bail!("Project config is not a file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => home_config(),
        Err(error) => Err(error).context("Inspecting project config"),
    }
}

pub(crate) fn absolute(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    let expanded = if text == "~" {
        dirs::home_dir().context("Home directory unavailable")?
    } else if let Some(relative) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        dirs::home_dir()
            .context("Home directory unavailable")?
            .join(relative)
    } else {
        path.to_path_buf()
    };
    let absolute = std::path::absolute(expanded)?;
    let mut existing = absolute.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(
            existing
                .file_name()
                .context("Config path has no filename")?
                .to_owned(),
        );
        existing = existing.parent().context("Config path has no parent")?;
    }
    let mut resolved = std::fs::canonicalize(existing).context("Resolving configuration path")?;
    #[cfg(windows)]
    {
        let text = resolved.to_string_lossy();
        if let Some(portable) = text.strip_prefix(r"\\?\UNC\") {
            resolved = PathBuf::from(format!(r"\\{portable}"));
        } else if let Some(portable) = text.strip_prefix(r"\\?\") {
            resolved = PathBuf::from(portable);
        }
    }
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}
