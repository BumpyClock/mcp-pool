use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

#[path = "config_import_kind.rs"]
mod kind;
use kind::ImportKind;
#[path = "config_import_convert.rs"]
mod convert;

#[cfg(test)]
#[path = "config_import_sources_tests.rs"]
mod tests;

pub(crate) struct ImportedServer {
    pub entry: Value,
    pub source: PathBuf,
}

#[derive(Default)]
pub(crate) struct ImportBatch {
    pub servers: BTreeMap<String, ImportedServer>,
    pub warnings: Vec<String>,
    requested_names: BTreeSet<String>,
}

pub(crate) fn collect(
    source: &str,
    explicit: Option<&Path>,
    project: &Path,
    home: &Path,
    names: &[String],
) -> Result<ImportBatch> {
    let source = ImportKind::parse(source)?;
    let mut batch = ImportBatch {
        requested_names: names.iter().cloned().collect(),
        ..ImportBatch::default()
    };
    if let Some(path) = explicit {
        if source == ImportKind::Project {
            bail!("Use a named import source with --from PATH");
        }
        read_into(
            &mut batch,
            if source == ImportKind::ClaudeDesktop {
                ImportKind::Claude
            } else {
                source
            },
            path,
            project,
            false,
            true,
        )?;
        if source == ImportKind::Claude {
            read_claude_local(&mut batch, path, project)?;
        }
    } else {
        match source {
            ImportKind::Cursor
            | ImportKind::Windsurf
            | ImportKind::OpenCode
            | ImportKind::VsCode => {
                for path in source_paths(source, project, home).into_iter().rev() {
                    read_into(&mut batch, source, &path, project, true, true)?;
                }
            }
            ImportKind::Claude => {
                let custom = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
                let directory = custom.clone().unwrap_or_else(|| home.to_path_buf());
                let user = directory.join(".claude.json");
                let legacy = custom.unwrap_or_else(|| home.join(".claude"));
                for path in [legacy.join("mcp.json"), legacy.join("settings.json")] {
                    read_into(&mut batch, ImportKind::Claude, &path, project, true, true)?;
                }
                read_into(&mut batch, ImportKind::Claude, &user, project, true, true)?;
                read_claude_project(&mut batch, project, true)?;
                read_claude_local(&mut batch, &user, project)?;
            }
            ImportKind::ClaudeDesktop => {
                #[cfg(target_os = "macos")]
                let directory = home
                    .join("Library")
                    .join("Application Support")
                    .join("Claude");
                #[cfg(windows)]
                let directory = std::env::var_os("APPDATA")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join("AppData").join("Roaming"))
                    .join("Claude");
                #[cfg(not(any(windows, target_os = "macos")))]
                let directory = home.join(".config").join("Claude");
                read_into(
                    &mut batch,
                    ImportKind::Claude,
                    &directory.join("claude_desktop_config.json"),
                    project,
                    false,
                    true,
                )?;
            }
            ImportKind::Codex => {
                let directory = std::env::var_os("CODEX_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".codex"));
                read_into(
                    &mut batch,
                    ImportKind::Codex,
                    &directory.join("config.toml"),
                    project,
                    true,
                    true,
                )?;
                read_into(
                    &mut batch,
                    ImportKind::Codex,
                    &project.join(".codex").join("config.toml"),
                    project,
                    true,
                    true,
                )?;
            }
            ImportKind::Mcporter => {
                read_into(
                    &mut batch,
                    source,
                    &home.join(".mcporter").join("mcporter.json"),
                    project,
                    true,
                    true,
                )?;
                read_into(
                    &mut batch,
                    source,
                    &project.join("config").join("mcporter.json"),
                    project,
                    true,
                    true,
                )?;
            }
            ImportKind::Native => {
                let path = dirs::config_dir()
                    .context("Legacy config directory unavailable; use --from PATH")?
                    .join("mcp-pool")
                    .join("config.toml");
                read_into(&mut batch, source, &path, project, false, true)?;
            }
            ImportKind::Project => {
                read_claude_project(&mut batch, project, false)?;
                for (kind, path) in [
                    (
                        ImportKind::Codex,
                        project.join(".codex").join("config.toml"),
                    ),
                    (
                        ImportKind::Mcporter,
                        project.join("config").join("mcporter.json"),
                    ),
                ] {
                    read_into(&mut batch, kind, &path, project, true, false)?;
                }
            }
        }
    }
    if batch.servers.is_empty() {
        bail!("No importable servers found for '{source}'; use --from PATH to select a config");
    }
    Ok(batch)
}

fn source_paths(source: ImportKind, project: &Path, home: &Path) -> Vec<PathBuf> {
    let app_data = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData").join("Roaming"));
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let mut paths = match source {
        ImportKind::Cursor => vec![
            project.join(".cursor").join("mcp.json"),
            home.join(".cursor").join("mcp.json"),
            app_data.join("Cursor").join("User").join("mcp.json"),
            home.join("Library")
                .join("Application Support")
                .join("Cursor")
                .join("User")
                .join("mcp.json"),
            xdg.join("Cursor").join("User").join("mcp.json"),
        ],
        ImportKind::Windsurf => {
            let mut paths = vec![
                home.join(".codeium")
                    .join("windsurf")
                    .join("mcp_config.json"),
                home.join(".codeium")
                    .join("windsurf-next")
                    .join("mcp_config.json"),
                home.join(".windsurf").join("mcp_config.json"),
                home.join(".config")
                    .join(".codeium")
                    .join("windsurf")
                    .join("mcp_config.json"),
            ];
            #[cfg(windows)]
            paths.push(
                app_data
                    .join("Codeium")
                    .join("windsurf")
                    .join("mcp_config.json"),
            );
            paths
        }
        ImportKind::VsCode => {
            #[cfg(target_os = "macos")]
            let directory = home.join("Library").join("Application Support");
            #[cfg(windows)]
            let directory = app_data;
            #[cfg(not(any(windows, target_os = "macos")))]
            let directory = xdg;
            vec![
                project.join(".vscode").join("mcp.json"),
                directory.join("Code").join("User").join("mcp.json"),
                directory
                    .join("Code - Insiders")
                    .join("User")
                    .join("mcp.json"),
            ]
        }
        ImportKind::OpenCode => {
            #[cfg(windows)]
            let user_directory = if std::env::var_os("XDG_CONFIG_HOME").is_some() {
                xdg.join("opencode")
            } else {
                app_data.join("opencode")
            };
            #[cfg(not(windows))]
            let user_directory = xdg.join("opencode");
            let mut paths = Vec::new();
            if let Some(path) = std::env::var_os("OPENCODE_CONFIG") {
                paths.push(PathBuf::from(path));
            }
            paths.extend([
                project.join("opencode.jsonc"),
                project.join("opencode.json"),
            ]);
            if let Some(directory) = std::env::var_os("OPENCODE_CONFIG_DIR") {
                let directory = PathBuf::from(directory);
                paths.extend([
                    directory.join("opencode.jsonc"),
                    directory.join("opencode.json"),
                ]);
            }
            paths.push(project.join(".openai").join("config.json"));
            if let Some(directory) = std::env::var_os("OPENAI_WORKDIR") {
                paths.push(PathBuf::from(directory).join(".openai").join("config.json"));
            }
            paths.push(xdg.join("openai").join("config.json"));
            paths.extend([
                user_directory.join("opencode.jsonc"),
                user_directory.join("opencode.json"),
            ]);
            paths
        }
        _ => Vec::new(),
    };
    let mut seen = BTreeSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    paths
}

fn read_claude_project(batch: &mut ImportBatch, project: &Path, replace: bool) -> Result<()> {
    let mut claude = ImportBatch {
        requested_names: batch.requested_names.clone(),
        ..ImportBatch::default()
    };
    for path in [
        project.join(".claude").join("mcp.json"),
        project.join(".claude").join("settings.json"),
        project.join(".mcp.json"),
        project.join(".claude").join("settings.local.json"),
    ] {
        read_into(&mut claude, ImportKind::Claude, &path, project, true, true)?;
    }
    for (name, server) in claude.servers {
        if !replace && batch.servers.contains_key(&name) {
            bail!("Duplicate project server '{name}'; import each source separately");
        }
        batch.servers.insert(name, server);
    }
    batch.warnings.extend(claude.warnings);
    Ok(())
}

fn read_text(path: &Path, optional: bool) -> Result<Option<String>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Reading import source {}", path.display()));
        }
    };
    if metadata.len() > 16 * 1024 * 1024 {
        bail!("Import source {} exceeds 16 MiB", path.display());
    }
    std::fs::read_to_string(path)
        .with_context(|| format!("Reading import source {}", path.display()))
        .map(Some)
}

fn json_document(path: &Path, contents: &str) -> Result<Value> {
    jsonc_parser::parse_to_serde_value(
        contents.trim_start_matches('\u{feff}'),
        &jsonc_parser::ParseOptions::default(),
    )
    .map_err(|_| anyhow::anyhow!("Invalid JSONC in import source {}", path.display()))?
    .with_context(|| format!("Empty import source {}", path.display()))
}

fn read_into(
    batch: &mut ImportBatch,
    kind: ImportKind,
    path: &Path,
    project: &Path,
    optional: bool,
    replace: bool,
) -> Result<()> {
    let Some(contents) = read_text(path, optional)? else {
        return Ok(());
    };
    let path = crate::config_paths::absolute(path)?;
    let document = if kind.is_toml() {
        let value: toml::Value = toml::from_str(&contents)
            .map_err(|_| anyhow::anyhow!("Invalid TOML in import source {}", path.display()))?;
        serde_json::to_value(value)?
    } else {
        json_document(&path, &contents)?
    };
    let object = document
        .as_object()
        .with_context(|| format!("Import source {} must be an object", path.display()))?;
    let mut entries = Map::new();
    for key in kind.containers() {
        if let Some(container) = object.get(*key) {
            let container = container.as_object().with_context(|| {
                format!(
                    "Import source {} needs an object named {key}",
                    path.display()
                )
            })?;
            for (name, value) in container {
                entries.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
    }
    if kind.is_editor()
        && kind != ImportKind::OpenCode
        && kind
            .containers()
            .iter()
            .all(|key| !object.contains_key(*key))
    {
        entries.extend(
            object
                .iter()
                .filter(|(_, value)| {
                    value
                        .as_object()
                        .is_some_and(crate::server_config::is_server_entry)
                })
                .map(|(name, value)| (name.clone(), value.clone())),
        );
    }
    if kind == ImportKind::Mcporter
        && document
            .get("imports")
            .and_then(Value::as_array)
            .is_some_and(|values| !values.is_empty())
    {
        batch.warnings.push(format!(
            "{} has legacy imports; import those sources explicitly.",
            path.display()
        ));
    }
    insert_entries(batch, &entries, kind, &path, project, replace)
}

fn read_claude_local(batch: &mut ImportBatch, path: &Path, project: &Path) -> Result<()> {
    let Some(contents) = read_text(path, true)? else {
        return Ok(());
    };
    let document = json_document(path, &contents)?;
    let Some(projects) = document.get("projects").and_then(Value::as_object) else {
        return Ok(());
    };
    let project = crate::config_paths::absolute(project)?;
    for (directory, settings) in projects {
        if crate::config_paths::absolute(Path::new(directory))? != project {
            continue;
        }
        if let Some(entries) = settings.get("mcpServers") {
            let entries = entries
                .as_object()
                .context("Claude project mcpServers must be an object")?;
            insert_entries(batch, entries, ImportKind::Claude, path, &project, true)?;
        }
    }
    Ok(())
}

fn insert_entries(
    batch: &mut ImportBatch,
    entries: &Map<String, Value>,
    kind: ImportKind,
    source: &Path,
    project: &Path,
    replace: bool,
) -> Result<()> {
    for (name, value) in entries {
        if !batch.requested_names.is_empty() && !batch.requested_names.contains(name) {
            continue;
        }
        if name.trim().is_empty() {
            bail!("Import source contains an empty server name");
        }
        let Some(mut entry) = convert::entry(kind, value, name, &mut batch.warnings)
            .with_context(|| format!("Server '{name}' in {}", source.display()))?
        else {
            continue;
        };
        let base = if matches!(kind, ImportKind::Mcporter | ImportKind::Native)
            || (kind.is_editor() && !source.starts_with(project))
        {
            source.parent().context("Import source has no parent")?
        } else {
            project
        };
        convert::relocate(&mut entry, base, source)?;
        if let Some(previous) = batch.servers.get(name)
            && !replace
        {
            bail!(
                "Server '{name}' occurs in both {} and {}; import each source separately",
                previous.source.display(),
                source.display()
            );
        }
        batch.servers.insert(
            name.clone(),
            ImportedServer {
                entry,
                source: source.to_path_buf(),
            },
        );
    }
    Ok(())
}
