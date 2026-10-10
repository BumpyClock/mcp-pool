use std::io;

use serde_json::{Value, json};

use super::support::{Fixture, parse_json};

#[tokio::test]
async fn remove_retires_old_environment_view_and_preserves_unrelated_pool() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    let environment = configuration
        .pointer_mut("/mcpServers/fixture/env")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("fixture environment missing"))?;
    environment.insert("MCP_POOL_TEST_VALUE".into(), json!("$env:FIXTURE_VALUE"));
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    let first_environment = [("FIXTURE_VALUE", "A")];
    fixture
        .warm_environment("fixture", &first_environment)
        .await?;
    let affected = fixture
        .command_input_environment(
            &["call", "fixture.echo", "label=affected", "--output", "json"],
            None,
            &first_environment,
        )
        .await?;
    assert!(affected.status.success(), "{affected:?}");
    assert_eq!(parse_json(&affected)?.get("environment"), Some(&json!("A")));
    let initial_status = parse_json(&fixture.success(&["pool", "status", "--json"]).await?)?;
    let initial_servers = initial_status
        .get("servers")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("initial status omitted servers"))?;
    assert_eq!(initial_servers.len(), 1);
    let affected_name = initial_servers
        .first()
        .and_then(|server| server.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other("affected name missing"))?
        .to_owned();

    fixture
        .warm_environment("array", &first_environment)
        .await?;
    let unrelated = fixture
        .command_input_environment(
            &[
                "call",
                "array.echo",
                "label=unrelated-before",
                "--output",
                "json",
            ],
            None,
            &first_environment,
        )
        .await?;
    assert!(unrelated.status.success(), "{unrelated:?}");
    assert_eq!(
        parse_json(&unrelated)?.pointer("/arguments/label"),
        Some(&json!("unrelated-before"))
    );
    let before = parse_json(&fixture.success(&["pool", "status", "--json"]).await?)?;
    let before_servers = before
        .get("servers")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("status before removal omitted servers"))?;
    assert_eq!(before_servers.len(), 2);
    let unrelated_name = before_servers
        .iter()
        .filter_map(|server| server.get("name").and_then(Value::as_str))
        .find(|name| *name != affected_name)
        .ok_or_else(|| io::Error::other("unrelated pool missing"))?
        .to_owned();
    let events = tokio::fs::read_to_string(&fixture.counter).await?;
    let process_ids = events
        .lines()
        .filter_map(|line| line.strip_prefix("pid="))
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    assert_eq!(process_ids.len(), 2);
    let affected_process = *process_ids
        .first()
        .ok_or_else(|| io::Error::other("affected PID missing"))?;
    let unrelated_process = *process_ids
        .get(1)
        .ok_or_else(|| io::Error::other("unrelated PID missing"))?;
    assert!(process_is_alive(affected_process)?);
    assert!(process_is_alive(unrelated_process)?);

    let changed_environment = [("FIXTURE_VALUE", "B")];
    let removed = fixture
        .command_input_environment(&["config", "remove", "fixture"], None, &changed_environment)
        .await?;
    assert!(removed.status.success(), "{removed:?}");
    let saved: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    assert!(saved.pointer("/mcpServers/fixture").is_none());
    assert!(saved.pointer("/mcpServers/array").is_some());
    let after = parse_json(&fixture.success(&["pool", "status", "--json"]).await?)?;
    let after_servers = after
        .get("servers")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("status after removal omitted servers"))?;
    assert_eq!(
        after_servers.len(),
        1,
        "old resolved pool survived removal: {after}"
    );
    assert_eq!(
        after.pointer("/servers/0/name"),
        Some(&json!(unrelated_name))
    );
    assert_eq!(after.pointer("/servers/0/status"), Some(&json!("running")));
    assert!(
        !after_servers
            .iter()
            .any(|server| server.get("name") == Some(&json!(affected_name)))
    );
    assert!(
        !process_is_alive(affected_process)?,
        "removed entry's upstream process survived"
    );
    assert!(
        process_is_alive(unrelated_process)?,
        "unrelated upstream process was retired"
    );

    let surviving = fixture
        .command_input_environment(
            &[
                "call",
                "array.echo",
                "label=unrelated-after",
                "--output",
                "json",
            ],
            None,
            &changed_environment,
        )
        .await?;
    assert!(surviving.status.success(), "{surviving:?}");
    assert_eq!(
        parse_json(&surviving)?.pointer("/arguments/label"),
        Some(&json!("unrelated-after"))
    );
    assert_eq!(fixture.event_count("spawn").await?, 2);
    assert_eq!(fixture.event_count("initialize").await?, 2);
    fixture.finish().await
}

#[cfg(windows)]
fn process_is_alive(process_id: u32) -> io::Result<bool> {
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
fn process_is_alive(process_id: u32) -> io::Result<bool> {
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

#[tokio::test]
async fn irrelevant_ambient_session_id_does_not_split_one_configured_pool() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture
        .warm_environment("fixture", &[("SESSION_ID", "synthetic-session-one")])
        .await?;
    for (session, label) in [
        ("synthetic-session-one", "first"),
        ("synthetic-session-two", "second"),
    ] {
        let argument = format!("label={label}");
        let response = fixture
            .command_input_environment(
                &["call", "fixture.echo", &argument, "--output", "json"],
                None,
                &[("SESSION_ID", session)],
            )
            .await?;
        assert!(response.status.success(), "{response:?}");
        assert_eq!(
            parse_json(&response)?.pointer("/arguments/label"),
            Some(&json!(label))
        );
    }
    let status = parse_json(&fixture.success(&["pool", "status", "--json"]).await?)?;
    let servers = status
        .get("servers")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("status omitted servers"))?;
    assert_eq!(
        servers.len(),
        1,
        "irrelevant ambient variable split the pool: {status}"
    );
    assert_eq!(status.pointer("/servers/0/status"), Some(&json!("running")));
    assert_eq!(fixture.event_count("spawn").await?, 1);
    assert_eq!(fixture.event_count("initialize").await?, 1);
    assert_eq!(fixture.event_count("tools/call").await?, 2);
    fixture.finish().await
}
