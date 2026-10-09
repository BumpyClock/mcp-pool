use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{AuthOptions, context, ensure_writable};
use crate::server_config::ConfiguredServer;
use crate::upstream_process::OwnedProcess;

pub(super) async fn authorize(
    server: &ConfiguredServer,
    options: AuthOptions,
    timeout_ms: Option<u64>,
) -> Result<Value> {
    let authentication = context(server)?;
    ensure_writable(&authentication)?;
    if options.reset {
        let server = server.clone();
        tokio::task::spawn_blocking(move || super::vault_clear(&server)).await??;
    }
    let helper = server.raw.get("oauthCommand").or_else(|| server.raw.get("oauth_command"))
        .and_then(|value| value.get("args")).and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("stdio server has no configured oauthCommand; configure its real authorization helper"))?;
    #[cfg(windows)]
    let executable = crate::upstream_stdio::resolve_windows_command(
        &server.definition.command,
        &server.definition.env,
        server.definition.cwd.as_deref(),
        server.definition.clear_env,
    )
    .map_err(|_| anyhow::anyhow!("could not resolve configured stdio OAuth helper"))?;
    #[cfg(unix)]
    let executable = &server.definition.command;
    let mut command = tokio::process::Command::new(executable);
    command.args(&server.definition.args);
    for argument in helper {
        command.arg(
            argument
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("OAuth helper arguments must be strings"))?,
        );
    }
    if server.definition.clear_env {
        command.env_clear();
    }
    command.envs(&server.definition.env);
    if let Some(directory) = &server.definition.cwd {
        command.current_dir(directory);
    }
    if options.no_browser {
        command.env("MCPORTER_OAUTH_NO_BROWSER", "1");
    }
    let mut process = OwnedProcess::spawn(command)
        .await
        .map_err(|_| anyhow::anyhow!("stdio OAuth helper failed to start"))?;
    let result = tokio::select! {
        result = tokio::time::timeout(Duration::from_millis(timeout_ms.unwrap_or(authentication.timeout_ms)), process.wait_for_exit()) => {
            result.map_err(|_| anyhow::anyhow!("stdio OAuth helper timed out"))
                .and_then(|result| result.map_err(|_| anyhow::anyhow!("stdio OAuth helper wait failed")))
        }
        result = tokio::signal::ctrl_c() => {
            match result {
                Ok(()) => Err(anyhow::anyhow!("stdio OAuth helper cancelled")),
                Err(_) => Err(anyhow::anyhow!("could not install OAuth cancellation handler")),
            }
        }
    };
    process.retire().await.map_err(|_| {
        anyhow::anyhow!("stdio OAuth helper process-tree retirement was not confirmed")
    })?;
    result?;
    let status = process
        .child
        .wait()
        .await
        .map_err(|_| anyhow::anyhow!("stdio OAuth helper exit status unavailable"))?;
    if !status.success() {
        bail!("stdio OAuth helper failed");
    }
    Ok(json!({"server":server.name,"status":"helper_completed"}))
}
