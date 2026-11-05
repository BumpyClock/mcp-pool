use std::io;

use serde_json::{Value, json};

use super::support::{Fixture, parse_json};

async fn set_environment(fixture: &Fixture, name: &str, value: Value) -> io::Result<()> {
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    configuration
        .pointer_mut("/mcpServers/fixture/env")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("missing fixture environment"))?
        .insert(name.to_owned(), value);
    tokio::fs::write(&fixture.config, configuration.to_string()).await
}

async fn artifact_result(fixture: &Fixture, failed: bool, data: &str) -> io::Result<Value> {
    let log = fixture.home.join("result.log");
    tokio::fs::write(&log, "log marker\n").await?;
    Ok(json!({
        "isError":failed,"content":[
            {"type":"text","text":"completed result"},
            {"type":"image","data":data,"mimeType":"image/png"}
        ],"logPath":log
    }))
}

#[tokio::test]
async fn artifact_flags_render_first_and_export_only_when_requested() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let result = artifact_result(&fixture, false, "aGVsbG8=").await?;
    set_environment(&fixture, "MCP_CLI_TEST_RESULT", json!(result.to_string())).await?;
    fixture.warm("fixture").await?;
    for (index, save, tail) in [
        (0, false, false),
        (1, true, false),
        (2, false, true),
        (3, true, true),
    ] {
        let directory = fixture.home.join(format!("images-{index}"));
        let path = directory
            .to_str()
            .ok_or_else(|| io::Error::other("invalid path"))?;
        let mut arguments = vec!["call", "fixture.echo", "label=artifact", "--output", "text"];
        if save {
            arguments.extend(["--save-images", path]);
        }
        if tail {
            arguments.push("--tail-log");
        }
        let output = fixture.success(&arguments).await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.starts_with("completed result\n"), "{output:?}");
        assert_eq!(stdout.contains("--- tail "), tail);
        assert_eq!(stdout.contains("log marker"), tail);
        assert_eq!(directory.exists(), save);
        if save {
            let mut entries = tokio::fs::read_dir(directory).await?;
            let file = entries
                .next_entry()
                .await?
                .ok_or_else(|| io::Error::other("image was not saved"))?;
            assert_eq!(tokio::fs::read(file.path()).await?, b"hello");
            assert!(entries.next_entry().await?.is_none());
        }
    }
    assert_eq!(fixture.event_count("tools/call").await?, 4);
    fixture.finish().await
}

#[tokio::test]
async fn error_result_still_exports_images_and_tails_after_rendering() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let result = artifact_result(&fixture, true, "aGVsbG8=").await?;
    set_environment(&fixture, "MCP_CLI_TEST_RESULT", json!(result.to_string())).await?;
    fixture.warm("fixture").await?;
    let directory = fixture.home.join("images");
    let output = fixture
        .command(&[
            "call",
            "fixture.echo",
            "label=artifact",
            "--output",
            "text",
            "--save-images",
            directory
                .to_str()
                .ok_or_else(|| io::Error::other("invalid path"))?,
            "--tail-log",
        ])
        .await?;
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("completed result\n--- tail "),
        "{output:?}"
    );
    assert!(stdout.contains("log marker"));
    assert!(directory.exists());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("MCP operation returned an error result")
    );
    assert_eq!(fixture.event_count("tools/call").await?, 1);
    fixture.finish().await
}

#[tokio::test]
async fn failed_image_export_keeps_completed_result_and_rendered_error_priority() -> io::Result<()>
{
    for failed in [false, true] {
        let fixture = Fixture::new().await?;
        let result = artifact_result(&fixture, failed, "not-base64").await?;
        set_environment(&fixture, "MCP_CLI_TEST_RESULT", json!(result.to_string())).await?;
        fixture.warm("fixture").await?;
        let directory = fixture.home.join("images");
        let output = fixture
            .command(&[
                "call",
                "fixture.echo",
                "label=artifact",
                "--output",
                "text",
                "--save-images",
                directory
                    .to_str()
                    .ok_or_else(|| io::Error::other("invalid path"))?,
                "--tail-log",
            ])
            .await?;
        assert!(!output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "completed result\n"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Tool result completed; image export failed"),
            "{output:?}"
        );
        if failed {
            assert!(stderr.contains("MCP operation returned an error result"));
            assert!(!stderr.contains("do not retry the tool call"));
        } else {
            assert!(stderr.contains("do not retry the tool call"));
        }
        assert!(!directory.exists());
        assert_eq!(fixture.event_count("tools/call").await?, 1);
        fixture.finish().await?;
    }
    Ok(())
}

#[tokio::test]
async fn rpc_failure_does_not_run_artifact_actions() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    let directory = fixture.home.join("images");
    let output = fixture
        .command(&[
            "call",
            "fixture.fail_rpc",
            "label=artifact",
            "--output",
            "json",
            "--save-images",
            directory
                .to_str()
                .ok_or_else(|| io::Error::other("invalid path"))?,
            "--tail-log",
        ])
        .await?;
    assert!(!output.status.success());
    assert_eq!(parse_json(&output)?.get("server"), Some(&json!("fixture")));
    assert!(
        parse_json(&output)?
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.contains("fixture RPC failure"))
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("--tail-log"));
    assert!(!directory.exists());
    fixture.finish().await
}

#[tokio::test]
async fn large_resource_output_preserves_raw_envelope_and_text() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let text = "resource body\n".repeat(16384);
    let resource_file = fixture.home.join("large-resource.txt");
    tokio::fs::write(&resource_file, &text).await?;
    set_environment(&fixture, "MCP_CLI_TEST_RESOURCE_FILE", json!(resource_file)).await?;
    fixture.warm("fixture").await?;
    let raw = fixture
        .success(&["resource", "fixture", "fixture://large", "--output", "raw"])
        .await?;
    assert_eq!(
        parse_json(&raw)?,
        json!({"contents":[{
            "uri":"fixture://large","mimeType":"text/plain","text":text
        }]})
    );
    let output = fixture
        .success(&["resource", "fixture", "fixture://large", "--output", "text"])
        .await?;
    assert_eq!(String::from_utf8_lossy(&output.stdout), format!("{text}\n"));
    fixture.finish().await
}
