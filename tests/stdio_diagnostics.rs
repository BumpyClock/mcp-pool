use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const FIXTURE: &str = "MCP_POOL_STDERR_FIXTURE";

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
    let home = std::env::temp_dir().join(format!("pool-stderr-{}", std::process::id()));
    tokio::fs::create_dir(&home).await?;
    let binary = env!("CARGO_BIN_EXE_mcp-pool");
    let fixture = std::env::current_exe()?.to_string_lossy().into_owned();
    let mut add = Command::new(binary);
    add.env("MCP_POOL_HOME", &home).args([
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
    let mut daemon = Command::new(binary)
        .env("MCP_POOL_HOME", &home)
        .env(FIXTURE, "1")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            #[cfg(unix)]
            let ready =
                tokio::net::UnixStream::connect(home.join("state").join("mcp-pool-control.sock"))
                    .await
                    .is_ok();
            #[cfg(windows)]
            let ready = {
                let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
                for byte in home.to_string_lossy().as_bytes() {
                    hash ^= u64::from(*byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
                tokio::net::windows::named_pipe::ClientOptions::new()
                    .open(format!(r"\\.\pipe\mcp-pool-{:08x}-control", hash as u32))
                    .is_ok()
            };
            if ready {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    let mut proxy = Command::new(binary)
        .env("MCP_POOL_HOME", &home)
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
    let shutdown = Command::new(binary)
        .env("MCP_POOL_HOME", &home)
        .arg("shutdown")
        .output()
        .await?;
    assert!(shutdown.status.success(), "{shutdown:?}");
    drop(input);
    tokio::time::timeout(Duration::from_secs(5), proxy.wait())
        .await
        .map_err(io::Error::other)??;
    tokio::time::timeout(Duration::from_secs(5), daemon.wait())
        .await
        .map_err(io::Error::other)??;
    tokio::fs::remove_dir_all(home).await?;
    Ok(())
}
