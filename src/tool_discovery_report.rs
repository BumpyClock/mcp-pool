use anyhow::Result;
use serde_json::{Value, json};

use super::errors::attach_error;
use crate::mcp_protocol::ConnectionInfo;
use crate::server_config::ConfiguredServer;
use crate::tool_documentation::Style;

pub(super) struct ServerReport {
    pub(super) server: ConfiguredServer,
    pub(super) duration: u64,
    pub(super) outcome: Outcome,
}

pub(super) enum Outcome {
    Listed {
        tools: Vec<Value>,
        connection: Option<ConnectionInfo>,
    },
    /// The server answered, so its connection metadata is still reported.
    ToolSelectionFailed {
        connection: Option<ConnectionInfo>,
        error: String,
    },
    DiscoveryFailed {
        error: String,
    },
}

pub(super) struct View {
    pub(super) detailed: bool,
    pub(super) schema: bool,
    pub(super) brief: bool,
    pub(super) all_parameters: bool,
    pub(super) sources: bool,
}

impl ServerReport {
    pub(super) fn is_failure(&self) -> bool {
        !matches!(self.outcome, Outcome::Listed { .. })
    }

    pub(super) fn is_tool_selection_failure(&self) -> bool {
        matches!(self.outcome, Outcome::ToolSelectionFailed { .. })
    }

    fn tools(&self) -> Option<&[Value]> {
        match &self.outcome {
            Outcome::Listed { tools, .. } => Some(tools),
            Outcome::ToolSelectionFailed { .. } => Some(&[]),
            Outcome::DiscoveryFailed { .. } => None,
        }
    }

    fn connection(&self) -> Option<&ConnectionInfo> {
        match &self.outcome {
            Outcome::Listed { connection, .. }
            | Outcome::ToolSelectionFailed { connection, .. } => connection.as_ref(),
            Outcome::DiscoveryFailed { .. } => None,
        }
    }

    fn error(&self) -> Option<&str> {
        match &self.outcome {
            Outcome::Listed { .. } => None,
            Outcome::ToolSelectionFailed { error, .. } | Outcome::DiscoveryFailed { error } => {
                Some(error)
            }
        }
    }

    /// Key order is part of the JSON contract because serde_json preserves insertion order.
    pub(super) fn to_json(&self, view: &View) -> Result<Value> {
        let mut entry = base(&self.server, self.duration);
        if let Some(info) = self.connection()
            && let Some(object) = entry.as_object_mut()
            && let Value::Object(metadata) = serde_json::to_value(info)?
        {
            object.extend(metadata);
        }
        if let Some(error) = self.error() {
            attach_error(&mut entry, &self.server.name, error);
        }
        if let Some(object) = entry.as_object_mut() {
            if let Some(tools) = self.tools() {
                let projected: Vec<Value> = tools.iter().map(|tool| project(tool, view)).collect();
                object.insert("tools".to_owned(), json!(projected));
            }
            if view.sources
                && let Some(source) = object.get("source").cloned()
            {
                object.insert("sources".to_owned(), json!([source]));
            }
            if view.detailed {
                object.insert("mode".to_owned(), json!("server"));
            }
        }
        Ok(entry)
    }

    pub(super) fn print_text(&self, view: &View, style: &Style) -> Result<()> {
        if let Some(tools) = self.tools() {
            if view.detailed {
                self.print_detailed(tools, view, style)?;
            } else {
                println!(
                    "{} ({} tools; {}ms)",
                    self.server.name,
                    tools.len(),
                    self.duration
                );
            }
        }
        if let Some(error) = self.error() {
            println!("{} — {error}", self.server.name);
        }
        Ok(())
    }

    fn print_detailed(&self, tools: &[Value], view: &View, style: &Style) -> Result<()> {
        let server = &self.server;
        if server.definition.description.is_empty() {
            println!("{}", style.heading(&server.name));
        } else {
            println!(
                "{} - {}",
                style.heading(&server.name),
                style.muted(&server.definition.description)
            );
        }
        println!();
        if let Some(instructions) = self
            .connection()
            .and_then(|info| info.instructions.as_deref())
            .filter(|instructions| !instructions.is_empty())
        {
            println!("{}\n", style.muted(instructions));
        }
        let mut optional_hidden = false;
        for tool in tools {
            if view.brief {
                println!("  {}", style.brief(tool));
            } else {
                let documentation = style.render(tool, view.all_parameters);
                optional_hidden |= documentation.hidden_parameters;
                println!("{}", documentation.text);
            }
            if view.schema
                && let Some(schema) = tool.get("inputSchema")
            {
                println!("{}", serde_json::to_string_pretty(schema)?);
            }
        }
        if !view.brief
            && let Some(tool) = tools.first()
        {
            println!("  {}", style.heading("Examples:"));
            println!("    {}\n", style.example(&server.name, tool));
        }
        if optional_hidden {
            println!(
                "{}\n",
                style.muted(
                    "  Optional parameters hidden; run with --all-parameters to view all fields."
                )
            );
        }
        println!(
            "{}",
            style.muted(&format!(
                "  {} tools · {}ms · {}",
                tools.len(),
                self.duration,
                display_transport(server)
            ))
        );
        Ok(())
    }
}

pub(super) fn list_json(entries: Vec<Value>) -> Value {
    let mut counts = json!({"ok":0,"auth":0,"offline":0,"http":0,"error":0});
    for entry in &entries {
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("error");
        if let Some(count) = counts.get_mut(status) {
            *count = json!(count.as_u64().unwrap_or(0) + 1);
        }
    }
    json!({"mode":"list","counts":counts,"servers":entries})
}

fn project(tool: &Value, view: &View) -> Value {
    let mut metadata = serde_json::Map::new();
    for key in ["name", "description"] {
        if let Some(value) = tool.get(key) {
            metadata.insert(key.to_owned(), value.clone());
        }
    }
    if view.detailed || view.schema {
        for key in ["inputSchema", "outputSchema"] {
            if let Some(value) = tool.get(key) {
                metadata.insert(key.to_owned(), value.clone());
            }
        }
    }
    if view.detailed {
        metadata.insert(
            "options".to_owned(),
            json!(crate::tool_output::options(tool)),
        );
    }
    Value::Object(metadata)
}

fn base(server: &ConfiguredServer, duration: u64) -> Value {
    let mut value = json!({
        "name":server.name,"status":"ok","durationMs":duration,
        "transport":transport(server),"source":{"kind":"local","path":server.source},
    });
    if !server.definition.description.is_empty()
        && let Some(object) = value.as_object_mut()
    {
        object.insert(
            "description".to_owned(),
            json!(server.definition.description),
        );
    }
    value
}

fn transport(server: &ConfiguredServer) -> String {
    if server.definition.is_remote() {
        let sanitized = crate::mcp_cli::display_url(&server.definition.url);
        format!("HTTP {sanitized}")
    } else {
        format!("STDIO {}", server.definition.command)
    }
}

fn display_transport(server: &ConfiguredServer) -> String {
    if server.definition.transport_kind() == "sse" {
        format!(
            "SSE {}",
            crate::mcp_cli::display_url(&server.definition.url)
        )
    } else {
        transport(server)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_transport_preserves_sse_without_changing_json_metadata() {
        let mut server = ConfiguredServer {
            name: "fixture".to_owned(),
            definition: crate::config::ServerDef {
                url: "https://username:password@example.com/mcp?token=secret#fragment".to_owned(),
                transport: "SSE".to_owned(),
                ..Default::default()
            },
            raw: Value::Null,
            source: std::path::PathBuf::from("fixture.json"),
        };
        assert_eq!(display_transport(&server), "SSE https://example.com/mcp");
        assert_eq!(
            base(&server, 0).get("transport"),
            Some(&json!("HTTP https://example.com/mcp"))
        );
        server.definition.transport = "http".to_owned();
        assert_eq!(display_transport(&server), "HTTP https://example.com/mcp");
        server.definition.url.clear();
        server.definition.command = "fixture-command".to_owned();
        assert_eq!(display_transport(&server), "STDIO fixture-command");
    }
}
