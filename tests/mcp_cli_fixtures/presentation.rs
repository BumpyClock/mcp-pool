use std::io;

use serde_json::{Value, json};

use super::support::{Fixture, parse_json};

#[tokio::test]
async fn server_discovery_displays_complete_docs_and_keeps_brief_and_json_modes() -> io::Result<()>
{
    let fixture = Fixture::new().await?;
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    let server = configuration
        .pointer_mut("/mcpServers/fixture")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("missing fixture entry"))?;
    server.insert(
        "description".to_owned(),
        json!("Controlled tool documentation"),
    );
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    fixture.warm("fixture").await?;
    for command in ["list", "describe", "list-tools"] {
        let output = fixture.success(&[command, "fixture"]).await?;
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("fixture - Controlled tool documentation"));
        assert!(text.contains("  /**\n   * Fixture echo"));
        assert!(text.contains("@param label Label echoed in the result."));
        assert!(text.contains("@param count? Number of requested items. Default: 1."));
        assert!(text.contains("@param enabled? Enable the fixture option. Default: false."));
        assert!(text.contains(
            "function echo(label: string, count?: number, enabled?: boolean, delay_ms?: number);"
        ));
        assert!(text.contains("Examples:"));
        assert!(text.contains("mcp-pool call fixture.delayed --args '{\"label\":\"value\"}'"));
        assert!(!text.contains('\x1b'));
    }
    let no_color = fixture.success(&["--no-color", "list", "fixture"]).await?;
    assert!(!String::from_utf8_lossy(&no_color.stdout).contains('\x1b'));
    let brief = fixture.success(&["list", "fixture", "--brief"]).await?;
    let brief = String::from_utf8_lossy(&brief.stdout);
    assert!(brief.contains("function echo(label: string);"));
    assert!(!brief.contains("/**"));
    assert!(!brief.contains("@param"));
    let one_tool = fixture.success(&["list", "fixture.echo"]).await?;
    let one_tool = String::from_utf8_lossy(&one_tool.stdout);
    assert!(one_tool.contains("function echo("));
    assert!(!one_tool.contains("function delayed("));
    let json_output = parse_json(&fixture.success(&["list", "fixture.echo", "--json"]).await?)?;
    assert_eq!(
        json_output.pointer("/tools/0/inputSchema/properties/count/default"),
        Some(&json!(1))
    );
    assert_eq!(
        json_output.pointer("/tools/0/options/1/description"),
        Some(&json!("Number of requested items."))
    );
    let extra_argument = fixture.command(&["list", "fixture", "list"]).await?;
    assert!(!extra_argument.status.success());
    assert!(String::from_utf8_lossy(&extra_argument.stderr).contains("Usage: list"));
    fixture.finish().await
}

#[tokio::test]
async fn delayed_piped_commands_preserve_results_without_progress_output() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    for format in ["text", "json", "raw"] {
        let output = fixture
            .command_input_environment(
                &[
                    "call",
                    "fixture.delayed",
                    "label=progress-fixture",
                    "delay_ms=250",
                    "--output",
                    format,
                ],
                None,
                &[
                    ("CI", "0"),
                    ("TERM", "xterm"),
                    ("MCP_POOL_NO_PROGRESS", "0"),
                ],
            )
            .await?;
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("progress-fixture"));
        if format == "json" {
            assert_eq!(
                parse_json(&output)?.pointer("/arguments/label"),
                Some(&json!("progress-fixture"))
            );
        }
    }
    for arguments in [&["list", "--json"][..], &["list", "fixture", "--quiet"][..]] {
        let output = fixture.success(arguments).await?;
        assert!(output.stderr.is_empty(), "{output:?}");
        if arguments.contains(&"--quiet") {
            assert!(output.stdout.is_empty());
        } else {
            assert!(parse_json(&output)?.get("servers").is_some());
        }
    }
    fixture.finish().await
}
