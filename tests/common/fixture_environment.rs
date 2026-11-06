use std::process::Command;

/// Keep unrelated host settings (including PATH) while isolating every pool alias.
pub fn isolate(command: &mut Command) {
    let inherited = std::env::vars_os().map(|(name, _)| name);
    let explicit = command.get_envs().map(|(name, _)| name.to_owned());
    let names: Vec<_> = inherited.chain(explicit).collect();
    for name in names {
        let normalized = name.to_string_lossy().to_ascii_uppercase();
        if normalized.starts_with("MCP_CLI_") || normalized.starts_with("MCP_POOL_") {
            command.env_remove(name);
        }
    }
}
