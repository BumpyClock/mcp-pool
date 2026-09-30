use std::collections::BTreeMap;
use std::io;
use std::time::Duration;

use tokio::sync::mpsc;

use super::spawn;
use crate::upstream::UpstreamHandle;

const FIXTURE: &str = "MCP_POOL_PROCESS_FIXTURE";
const FIXTURE_TEST: &str = "upstream_stdio::tests::process_fixture";
const FINAL_RESPONSE: &str = r#"{"jsonrpc":"2.0","id":19,"result":{"final":true}}"#;

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "This synchronous fixture runs in a separate subprocess, outside the daemon runtime."
)]
fn process_fixture() -> io::Result<()> {
    let Ok(role) = std::env::var(FIXTURE) else {
        return Ok(());
    };
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    if role == "descendant" || role == "silent-descendant" {
        if role == "descendant" {
            println!("POOL_DESCENDANT:{}", std::process::id());
        } else {
            eprintln!("POOL_DESCENDANT_READY");
        }
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    if role == "exit" {
        return Ok(());
    }
    if role == "final" || role == "final-flood" {
        if role == "final-flood" {
            for _ in 0..128 {
                println!("queued-before-final");
            }
        }
        println!("{FINAL_RESPONSE}");
        std::process::exit(0);
    }
    if role == "echo" {
        println!("POOL_READY");
        for line in std::io::BufRead::lines(std::io::stdin().lock()) {
            println!("{}", line?);
        }
        return Ok(());
    }
    let inherited_stdout = role == "final-descendant";
    let mut descendant = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", FIXTURE_TEST, "--nocapture"])
        .env(
            FIXTURE,
            if inherited_stdout {
                "silent-descendant"
            } else {
                "descendant"
            },
        )
        .stdin(std::process::Stdio::null())
        .stdout(if inherited_stdout {
            std::process::Stdio::inherit()
        } else {
            std::process::Stdio::piped()
        })
        .stderr(if inherited_stdout {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .spawn()?;
    if inherited_stdout {
        let stderr = descendant
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing descendant readiness pipe"))?;
        for line in std::io::BufRead::lines(std::io::BufReader::new(stderr)) {
            if line? == "POOL_DESCENDANT_READY" {
                break;
            }
        }
    } else {
        let stdout = descendant
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing descendant pipe"))?;
        for line in std::io::BufRead::lines(std::io::BufReader::new(stdout)) {
            if line?.contains("POOL_DESCENDANT:") {
                break;
            }
        }
    }
    println!("POOL_TREE:{},{}", std::process::id(), descendant.id());
    if inherited_stdout {
        println!("{FINAL_RESPONSE}");
        std::process::exit(0);
    }
    if role == "natural" {
        return Ok(());
    }
    if role == "invalid" {
        use std::io::Write;
        std::io::stdout().write_all(&[0xff, b'\n'])?;
    }
    if role == "flood" {
        loop {
            println!("response");
        }
    }
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

async fn fixture(
    role: &str,
    capacity: usize,
) -> io::Result<(UpstreamHandle, mpsc::Receiver<String>)> {
    let (responses, receiver) = mpsc::channel(capacity);
    let handle = spawn(
        std::env::current_exe()?.to_string_lossy().into_owned(),
        vec![
            "--exact".to_string(),
            FIXTURE_TEST.to_string(),
            "--nocapture".to_string(),
        ],
        BTreeMap::from([(FIXTURE.to_string(), role.to_string())]),
        responses,
    )
    .await?;
    Ok((handle, receiver))
}

async fn tree(receiver: &mut mpsc::Receiver<String>) -> io::Result<Vec<u32>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(line) = receiver.recv().await {
            if let Some((_, identifiers)) = line.split_once("POOL_TREE:") {
                return identifiers
                    .split(',')
                    .map(|identifier| identifier.trim().parse::<u32>().map_err(io::Error::other))
                    .collect::<io::Result<Vec<_>>>();
            }
        }
        Err(io::Error::other(
            "fixture closed before reporting process tree",
        ))
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(windows)]
fn is_alive(process_id: u32) -> io::Result<bool> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
    };

    let handle = unsafe { OpenProcess(SYNCHRONIZATION_SYNCHRONIZE, 0, process_id) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            Ok(false)
        } else {
            Err(error)
        };
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    match unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), 0) } {
        WAIT_OBJECT_0 => Ok(false),
        WAIT_TIMEOUT => Ok(true),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(unix)]
fn is_alive(process_id: u32) -> io::Result<bool> {
    let process_id = libc::pid_t::try_from(process_id).map_err(io::Error::other)?;
    if unsafe { libc::kill(process_id, 0) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error)
    }
}

fn assert_retired(identifiers: &[u32]) -> io::Result<()> {
    assert_eq!(
        identifiers.len(),
        2,
        "fixture must report a child and a descendant"
    );
    for identifier in identifiers {
        assert!(
            !is_alive(*identifier)?,
            "owned process {identifier} survived retirement"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn unix_descendants_share_a_group_isolated_from_the_daemon() -> io::Result<()> {
    let (mut handle, mut responses) = fixture("hold", 32).await?;
    let identifiers = tree(&mut responses).await?;
    let leader = identifiers
        .first()
        .copied()
        .ok_or_else(|| io::Error::other("fixture did not report its leader"))?;
    let leader = libc::pid_t::try_from(leader).map_err(io::Error::other)?;
    let group = unsafe { libc::getpgid(leader) };
    if group < 0 {
        return Err(io::Error::last_os_error());
    }
    assert_eq!(
        group, leader,
        "the upstream must lead its own process group"
    );
    assert_ne!(
        group,
        unsafe { libc::getpgrp() },
        "the daemon must not share the upstream group"
    );
    for identifier in &identifiers {
        let process_id = libc::pid_t::try_from(*identifier).map_err(io::Error::other)?;
        let member_group = unsafe { libc::getpgid(process_id) };
        if member_group < 0 {
            return Err(io::Error::last_os_error());
        }
        assert_eq!(
            member_group, group,
            "a normal descendant must inherit ownership"
        );
    }
    tokio::time::timeout(Duration::from_secs(15), handle.shutdown())
        .await
        .map_err(io::Error::other)??;
    assert_retired(&identifiers)
}

#[tokio::test]
async fn shutdown_retires_uncooperative_descendants_before_restart() -> io::Result<()> {
    let (mut first, mut responses) = fixture("hold", 32).await?;
    let first_identifiers = tree(&mut responses).await?;
    first.shutdown().await?;
    assert_retired(&first_identifiers)?;
    first.wait_for_exit().await?;
    first.shutdown().await?;

    let (mut replacement, mut responses) = fixture("hold", 32).await?;
    let replacement_identifiers = tree(&mut responses).await?;
    assert_retired(&first_identifiers)?;
    for identifier in &replacement_identifiers {
        assert!(is_alive(*identifier)?);
    }
    replacement.shutdown().await?;
    assert_retired(&replacement_identifiers)
}

#[tokio::test]
async fn natural_exit_retires_remaining_descendants() -> io::Result<()> {
    let (mut handle, mut responses) = fixture("natural", 32).await?;
    let identifiers = tree(&mut responses).await?;
    tokio::time::timeout(Duration::from_secs(15), handle.wait_for_exit())
        .await
        .map_err(io::Error::other)??;
    assert_retired(&identifiers)?;
    handle.wait_for_exit().await
}

#[tokio::test]
async fn backpressure_does_not_block_retirement() -> io::Result<()> {
    let (mut handle, mut responses) = fixture("flood", 1).await?;
    let identifiers = tree(&mut responses).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(15), handle.shutdown())
        .await
        .map_err(io::Error::other)??;
    assert_retired(&identifiers)
}

#[tokio::test]
async fn dropping_handle_retires_process_tree() -> io::Result<()> {
    let (handle, mut responses) = fixture("hold", 32).await?;
    let identifiers = tree(&mut responses).await?;
    drop(handle);
    tokio::time::timeout(Duration::from_secs(15), async {
        while responses.recv().await.is_some() {}
    })
    .await
    .map_err(io::Error::other)?;
    // Pipe closure happens before verified retirement, so observe the processes.
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let mut alive = false;
            for identifier in &identifiers {
                alive |= is_alive(*identifier)?;
            }
            if !alive {
                return Ok::<(), io::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(io::Error::other)??;
    assert_retired(&identifiers)
}

#[tokio::test]
async fn closed_response_channel_retires_process_tree() -> io::Result<()> {
    let (mut handle, mut responses) = fixture("hold", 32).await?;
    let identifiers = tree(&mut responses).await?;
    drop(responses);
    tokio::time::timeout(Duration::from_secs(15), handle.wait_for_exit())
        .await
        .map_err(io::Error::other)??;
    assert_retired(&identifiers)
}

#[tokio::test]
async fn closed_request_channel_retires_process_tree() -> io::Result<()> {
    let (mut handle, mut responses) = fixture("hold", 32).await?;
    let identifiers = tree(&mut responses).await?;
    let (unrelated_sender, _unrelated_receiver) = mpsc::channel(1);
    handle.request_tx = unrelated_sender;
    tokio::time::timeout(Duration::from_secs(15), handle.wait_for_exit())
        .await
        .map_err(io::Error::other)??;
    assert_retired(&identifiers)
}

#[tokio::test]
async fn stdout_io_failure_retires_process_tree() -> io::Result<()> {
    let (mut handle, mut responses) = fixture("invalid", 32).await?;
    let identifiers = tree(&mut responses).await?;
    tokio::time::timeout(Duration::from_secs(15), handle.wait_for_exit())
        .await
        .map_err(io::Error::other)??;
    assert_retired(&identifiers)
}

#[tokio::test]
async fn request_response_round_trip_is_preserved() -> io::Result<()> {
    let (mut handle, mut responses) = fixture("echo", 32).await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(line) = responses.recv().await {
            if line.contains("POOL_READY") {
                return Ok::<(), io::Error>(());
            }
        }
        Err(io::Error::other("echo fixture did not become ready"))
    })
    .await
    .map_err(io::Error::other)??;
    let expected = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;
    handle
        .request_tx
        .send(expected.to_string())
        .await
        .map_err(io::Error::other)?;
    let response = tokio::time::timeout(Duration::from_secs(15), responses.recv())
        .await
        .map_err(io::Error::other)?;
    assert_eq!(response.as_deref(), Some(expected));
    handle.shutdown().await
}

#[tokio::test]
async fn natural_exit_without_descendants_completes() -> io::Result<()> {
    let (mut handle, _responses) = fixture("exit", 32).await?;
    tokio::time::timeout(Duration::from_secs(15), handle.wait_for_exit())
        .await
        .map_err(io::Error::other)??;
    handle.shutdown().await
}

#[tokio::test]
async fn missing_command_fails_before_returning_handle() {
    let (responses, _receiver) = mpsc::channel(1);
    let result = spawn(
        "mcp-pool-test-nonexistent-executable-58c099ec".to_string(),
        Vec::new(),
        BTreeMap::new(),
        responses,
    )
    .await;
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(io::ErrorKind::NotFound)
    );
}

#[tokio::test]
async fn invalid_environment_fails_before_returning_handle() -> io::Result<()> {
    let (responses, _receiver) = mpsc::channel(1);
    let result = spawn(
        std::env::current_exe()?.to_string_lossy().into_owned(),
        Vec::new(),
        BTreeMap::from([(
            "MCP_POOL_TEST_VALUE".to_string(),
            "embedded\0nul".to_string(),
        )]),
        responses,
    )
    .await;
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(io::ErrorKind::InvalidInput)
    );
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn windows_cmd_launcher_preserves_stdio() -> io::Result<()> {
    struct Launcher(std::path::PathBuf);
    impl Drop for Launcher {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_file(&self.0) {
                eprintln!("test launcher cleanup failed: {error}");
            }
        }
    }
    let launcher = Launcher(std::env::current_dir()?.join(format!(
        "upstream_process_launcher_{}.cmd",
        std::process::id()
    )));
    std::fs::write(
        &launcher.0,
        format!(
            "@echo off\r\n\"{}\" --exact {FIXTURE_TEST} --nocapture\r\n",
            std::env::current_exe()?.display()
        ),
    )?;
    let (responses, mut receiver) = mpsc::channel(32);
    let mut handle = spawn(
        launcher.0.to_string_lossy().into_owned(),
        Vec::new(),
        BTreeMap::from([(FIXTURE.to_string(), "echo".to_string())]),
        responses,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(line) = receiver.recv().await {
            if line.contains("POOL_READY") {
                return Ok::<(), io::Error>(());
            }
        }
        Err(io::Error::other("cmd fixture did not become ready"))
    })
    .await
    .map_err(io::Error::other)??;
    handle
        .request_tx
        .send("launcher-response".to_string())
        .await
        .map_err(io::Error::other)?;
    let response = tokio::time::timeout(Duration::from_secs(15), receiver.recv())
        .await
        .map_err(io::Error::other)?;
    assert_eq!(response.as_deref(), Some("launcher-response"));
    handle.shutdown().await
}

#[path = "upstream_process_terminal_tests.rs"]
mod terminal;
