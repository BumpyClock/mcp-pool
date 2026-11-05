use std::ffi::OsString;

pub(crate) const READ_ONLY_NAMES: [&str; 2] = [
    "MCP_CLI_CREDENTIALS_READ_ONLY",
    "MCP_POOL_CREDENTIALS_READ_ONLY",
];

pub(crate) fn value(name: &str) -> Option<OsString> {
    let legacy = name
        .strip_prefix("MCP_CLI_")
        .map(|suffix| format!("MCP_POOL_{suffix}"));
    std::env::var_os(name).or_else(|| legacy.and_then(std::env::var_os))
}

pub(crate) fn string(name: &str) -> Option<String> {
    value(name).and_then(|value| value.into_string().ok())
}

pub(crate) fn credentials_read_only() -> bool {
    READ_ONLY_NAMES
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| value == "1"))
}
