use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config_import_sources::ImportedServer;
use crate::oauth::import::Source;
use crate::server_config::{ConfiguredEnvironment, authentication_server};

pub(super) fn read(name: &str, imported: &ImportedServer, caller_home: &Path) -> Result<Source> {
    let environment = ConfiguredEnvironment::from_raw(&imported.entry);
    let runtime_home = match environment.get("HOME")? {
        Some(home) => Some(home),
        None => environment.get("USERPROFILE")?,
    };
    let home = runtime_home.map_or_else(|| caller_home.to_path_buf(), PathBuf::from);
    let vault_root = environment
        .get("XDG_DATA_HOME")?
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map_or_else(|| home.join(".mcporter"), |path| path.join("mcporter"));
    let server = authentication_server(&imported.source, name, &imported.entry)?;
    let mut authentication = crate::oauth::context(&server)?;
    authentication.vault_path = vault_root.join("credentials.json");
    authentication.read_only = true;
    let mut components = Path::new(name).components();
    let direct_child = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
        && !name.contains('\\');
    if !authentication.directory_primary && direct_child {
        authentication
            .directory_stores
            .push(home.join(".mcporter").join(name));
    }
    Source::read(&authentication).with_context(|| format!("Reading mcporter auth for '{name}'"))
}
