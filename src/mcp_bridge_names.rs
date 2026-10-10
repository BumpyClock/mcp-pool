const TOOL_SEPARATOR: &str = "__";

pub(crate) fn encode_tool_name(server: &str, tool: &str) -> String {
    format!(
        "{}{}{}",
        encode_tool_name_part(server, true, false),
        TOOL_SEPARATOR,
        encode_tool_name_part(tool, false, true)
    )
}

pub(crate) fn decode_tool_name(name: &str, servers: &[String]) -> Option<(String, String)> {
    let mut candidates: Vec<(&String, String)> = servers
        .iter()
        .map(|server| {
            (
                server,
                format!(
                    "{}{}",
                    encode_tool_name_part(server, true, false),
                    TOOL_SEPARATOR
                ),
            )
        })
        .collect();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.1.len()));

    for (server, prefix) in candidates {
        let Some(encoded_tool) = name.strip_prefix(&prefix) else {
            continue;
        };
        let tool = decode_tool_name_part(encoded_tool);
        if !tool.is_empty() {
            return Some((server.clone(), tool));
        }
    }
    None
}

pub(crate) fn describe_tool(server: &str, description: Option<&str>) -> String {
    match description.filter(|description| !description.is_empty()) {
        Some(description) => format!("[{server}] {description}"),
        None => format!("Tool from MCP server '{server}'."),
    }
}

fn encode_tool_name_part(
    value: &str,
    escape_trailing_underscore: bool,
    escape_leading_underscore: bool,
) -> String {
    let mut encoded = value.replace('%', "%25").replace(TOOL_SEPARATOR, "%5F%5F");
    if escape_leading_underscore && let Some(rest) = encoded.strip_prefix('_') {
        encoded = format!("%5F{rest}");
    }
    if escape_trailing_underscore && let Some(prefix) = encoded.strip_suffix('_') {
        encoded = format!("{prefix}%5F");
    }
    encoded
}

fn decode_tool_name_part(value: &str) -> String {
    value.replace("%5F", "_").replace("%25", "%")
}
