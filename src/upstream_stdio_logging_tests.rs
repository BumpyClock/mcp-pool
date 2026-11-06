use super::*;

const FIXTURE: &str = "MCP_CLI_STDERR_LOG_FIXTURE";
const SECRET: &str = "synthetic-stderr-secret-916d";
const SELECTED: &str = "upstream_stdio::logging_tests::selected_stderr_source";
const EXCLUDED: &str = "upstream_stdio::logging_tests::excluded_stderr_source";

#[test]
fn selected_stderr_source() {
    if std::env::var_os(FIXTURE).is_some() {
        eprintln!("selected stderr {SECRET}");
    }
}

#[test]
fn excluded_stderr_source() {
    if std::env::var_os(FIXTURE).is_some() {
        eprintln!("excluded stderr {SECRET}");
    }
}

#[tokio::test]
async fn production_stderr_filters_logical_names_and_redacts_secrets() -> anyhow::Result<()> {
    let directory = std::env::temp_dir().join(format!("mcp-cli-stderr-log-{}", std::process::id()));
    std::fs::create_dir(&directory)?;
    let path = directory.join("daemon.log");
    let output = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "upstream_stdio::logging_tests::filtered_stderr_fixture",
            "--nocapture",
        ])
        .env(FIXTURE, "active")
        .env("MCP_CLI_STDERR_LOG_PATH", &path)
        .output()
        .await?;
    std::fs::remove_file(&path)?;
    std::fs::remove_dir(&directory)?;
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("selected stderr [redacted]"), "{stderr}");
    assert!(!stderr.contains("excluded stderr"), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");
    Ok(())
}

#[tokio::test]
async fn filtered_stderr_fixture() -> anyhow::Result<()> {
    if std::env::var_os(FIXTURE).is_none() {
        return Ok(());
    }
    let path = std::path::PathBuf::from(
        std::env::var_os("MCP_CLI_STDERR_LOG_PATH")
            .ok_or_else(|| io::Error::other("missing log path"))?,
    );
    diagnostics::logging::configure(path.clone(), ["selected logical name".to_owned()].into())?;
    diagnostics::set_stderr_mirror(true);
    diagnostics::logging::register_identity("runtime-selected", "selected logical name")?;
    diagnostics::logging::register_identity("runtime-excluded", "excluded logical name")?;
    for (test, runtime) in [
        (SELECTED, "runtime-selected"),
        (EXCLUDED, "runtime-excluded"),
    ] {
        let mut source = Command::new(std::env::current_exe()?);
        source
            .args(["--exact", test, "--nocapture"])
            .env(FIXTURE, "active")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let log = diagnostics::ServerLog::for_runtime(runtime).with_redactions([SECRET.to_owned()]);
        let mut process = OwnedProcess::spawn(source, log.clone()).await?;
        let stderr = process
            .child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing stderr"))?;
        let read = tokio::time::timeout(Duration::from_secs(10), read_stderr(stderr, &log)).await;
        let retired = process.retire().await;
        read??;
        retired?;
    }
    diagnostics::log("generic daemon record name=runtime-excluded");
    let contents = std::fs::read_to_string(&path)?;
    assert!(
        contents.contains("selected stderr [redacted]"),
        "{contents}"
    );
    assert!(!contents.contains("excluded stderr"), "{contents}");
    assert!(contents.contains("generic daemon record name=runtime-excluded"));
    assert!(!contents.contains(SECRET), "{contents}");
    Ok(())
}
