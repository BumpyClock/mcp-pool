use std::error::Error;
use tokio::process::Command;

#[tokio::test]
async fn built_version_reports_the_source_build_identity() -> Result<(), Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_mcp-pool"))
        .arg("--version")
        .output()
        .await?;
    if !output.status.success() {
        return Err(format!("--version failed with status {}", output.status).into());
    }

    let stdout = String::from_utf8(output.stdout)?;
    let expected = format!("mcp-pool {}", env!("MCP_POOL_BUILD_VERSION"));
    assert!(
        stdout.lines().any(|line| line.trim() == expected),
        "expected {expected:?} in {stdout:?}"
    );
    assert!(
        env!("MCP_POOL_BUILD_VERSION").starts_with(concat!(env!("CARGO_PKG_VERSION"), "+")),
        "build version lacks Cargo package version: {}",
        env!("MCP_POOL_BUILD_VERSION")
    );
    assert!(
        env!("MCP_POOL_BUILD_VERSION").contains(".src."),
        "build version lacks source fingerprint: {}",
        env!("MCP_POOL_BUILD_VERSION")
    );
    Ok(())
}
