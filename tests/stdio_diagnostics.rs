use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[path = "common/fixture_lifecycle.rs"]
mod fixture_lifecycle;
use fixture_lifecycle::{FixtureLifecycle, fixture_home};

const FIXTURE: &str = "MCP_CLI_STDERR_FIXTURE";

#[test]
fn diagnostic_fixture() -> io::Result<()> {
    if std::env::var_os(FIXTURE).is_none() {
        return Ok(());
    }
    use std::io::Write;
    std::io::stderr().write_all(b"\xff\xfe\r\n")?;
    std::io::stderr().flush()?;
    for line in std::io::BufRead::lines(std::io::stdin().lock()) {
        println!("echo:{}", line?);
    }
    Ok(())
}

#[tokio::test]
async fn invalid_utf8_diagnostics_do_not_retire_server() -> io::Result<()> {
    let home = fixture_home::create("ps")?;
    let binary = env!("CARGO_BIN_EXE_mcp-cli");
    let mut lifecycle =
        FixtureLifecycle::new(std::path::Path::new(binary), &home, &home, &home, None);
    let fixture = std::env::current_exe()?.to_string_lossy().into_owned();
    let mut add = Command::from(lifecycle.command());
    add.args([
        "add",
        "diagnostics",
        "--",
        &fixture,
        "--exact",
        "diagnostic_fixture",
        "--nocapture",
    ]);
    let output = add.output().await?;
    assert!(output.status.success(), "{output:?}");
    let mut daemon = lifecycle.command();
    daemon.env(FIXTURE, "1").args(["pool", "serve"]);
    lifecycle.start(daemon)?;
    lifecycle.ready(Duration::from_secs(10)).await?;
    let mut proxy = Command::from(lifecycle.command())
        .env(FIXTURE, "1")
        .args(["proxy", "diagnostics"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = proxy
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing stdin"))?;
    let mut output = BufReader::new(
        proxy
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?,
    );
    input.write_all(b"first\nsecond\n").await?;
    input.flush().await?;
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            if output.read_line(&mut line).await? == 0 {
                return Err(io::Error::other("upstream retired after diagnostic bytes"));
            }
            if line.starts_with("echo:") {
                received.push(line.trim_end().to_string());
                if received.len() == 2 {
                    return Ok(());
                }
            }
        }
    })
    .await
    .map_err(io::Error::other)??;
    assert_eq!(received, ["echo:first", "echo:second"]);
    lifecycle.finish().await?;
    drop(input);
    tokio::time::timeout(Duration::from_secs(5), proxy.wait())
        .await
        .map_err(io::Error::other)??;
    Ok(())
}
