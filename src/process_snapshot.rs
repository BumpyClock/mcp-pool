use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Identity {
    pub pid: u32,
    pub parent: u32,
    pub born: String,
    pub owner: String,
}

#[derive(Deserialize)]
pub(super) struct Snapshot {
    pub owner: String,
    pub processes: Vec<Identity>,
}

pub(super) async fn snapshot() -> Result<Snapshot> {
    tokio::task::spawn_blocking(platform_snapshot)
        .await
        .context("Process identity query task failed")?
}

pub(super) fn owned_tree(snapshot: &Snapshot, root: u32) -> Result<Vec<Identity>> {
    let processes: BTreeMap<_, _> = snapshot
        .processes
        .iter()
        .map(|process| (process.pid, process))
        .collect();
    let mut pending = vec![root];
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    while let Some(pid) = pending.pop() {
        if !seen.insert(pid) {
            continue;
        }
        let process = processes
            .get(&pid)
            .context("Legacy process disappeared during ownership verification")?;
        if process.owner != snapshot.owner || process.born.is_empty() {
            bail!("Legacy process-tree ownership cannot be verified; no stop was sent");
        }
        pending.extend(
            snapshot
                .processes
                .iter()
                .filter(|candidate| candidate.parent == pid)
                .map(|candidate| candidate.pid),
        );
        result.push((*process).clone());
    }
    Ok(result)
}

pub(super) fn remaining(snapshot: &Snapshot, identities: &[Identity]) -> Result<Vec<Identity>> {
    let mut result = Vec::new();
    for expected in identities {
        if let Some(current) = snapshot
            .processes
            .iter()
            .find(|process| process.pid == expected.pid)
            && current.born == expected.born
        {
            if current.owner != expected.owner {
                bail!("Legacy process identity became unverifiable");
            }
            result.push(current.clone());
        }
    }
    Ok(result)
}

#[cfg(windows)]
#[allow(
    clippy::disallowed_methods,
    reason = "Process inspection runs on a blocking worker"
)]
fn platform_snapshot() -> Result<Snapshot> {
    let script = r#"
$ErrorActionPreference='Stop'
$owner=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$rows=@(Get-CimInstance Win32_Process | ForEach-Object {
  $process=$_
  $identity=Invoke-CimMethod -InputObject $process -MethodName GetOwnerSid -ErrorAction SilentlyContinue
  $sid=if ($identity -and $identity.ReturnValue -eq 0) { $identity.Sid } else { '' }
  $born=if ($process.CreationDate) { $process.CreationDate.ToUniversalTime().ToString('o') } else { '' }
  [pscustomobject]@{pid=[uint32]$process.ProcessId;parent=[uint32]$process.ParentProcessId;born=$born;owner=$sid}
})
[pscustomobject]@{owner=$owner;processes=$rows}|ConvertTo-Json -Depth 4 -Compress
"#;
    let root = std::env::var_os("SystemRoot").context("Windows system directory is unavailable")?;
    let executable = std::path::PathBuf::from(root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let output = std::process::Command::new(executable)
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()?;
    if !output.status.success() {
        bail!("Windows process ownership query failed; no stop was sent");
    }
    serde_json::from_slice(&output.stdout).context("Invalid Windows process identity response")
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn platform_snapshot() -> Result<Snapshot> {
    use std::os::unix::fs::MetadataExt;
    let owner = unsafe { libc::geteuid() }.to_string();
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("Reading process boot identity")?;
    let mut processes = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let metadata = match std::fs::metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("Inspecting process owner"),
        };
        let content = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                processes.push(Identity {
                    pid,
                    parent: 0,
                    born: String::new(),
                    owner: metadata.uid().to_string(),
                });
                continue;
            }
            Err(error) => return Err(error).context("Reading process identity"),
        };
        let (_, fields) = content
            .rsplit_once(')')
            .context("Malformed process identity")?;
        let fields: Vec<_> = fields.split_whitespace().collect();
        let parent = fields.get(1).context("Missing process parent")?.parse()?;
        let start = fields
            .get(19)
            .context("Missing process creation identity")?;
        processes.push(Identity {
            pid,
            parent,
            born: format!("{}:{start}", boot.trim()),
            owner: metadata.uid().to_string(),
        });
    }
    Ok(Snapshot { owner, processes })
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
#[allow(
    clippy::disallowed_methods,
    reason = "Process inspection runs on a blocking worker"
)]
fn platform_snapshot() -> Result<Snapshot> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,uid=,lstart="])
        .output()?;
    if !output.status.success() {
        bail!("Process ownership query failed; no stop was sent");
    }
    let mut processes = Vec::new();
    for line in std::str::from_utf8(&output.stdout)?.lines() {
        let mut fields = line.split_whitespace();
        let pid = fields.next().context("Missing process id")?.parse()?;
        let parent = fields.next().context("Missing process parent")?.parse()?;
        let owner = fields.next().context("Missing process owner")?.to_owned();
        let born = fields.collect::<Vec<_>>().join(" ");
        processes.push(Identity {
            pid,
            parent,
            owner,
            born,
        });
    }
    Ok(Snapshot {
        owner: unsafe { libc::geteuid() }.to_string(),
        processes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retirement_matches_creation_identity_and_rejects_foreign_children() -> Result<()> {
        let root = Identity {
            pid: 10,
            parent: 1,
            born: "birth-one".into(),
            owner: "owner".into(),
        };
        let child = Identity {
            pid: 11,
            parent: 10,
            born: "child-birth".into(),
            owner: "owner".into(),
        };
        let mut snapshot = Snapshot {
            owner: "owner".into(),
            processes: vec![root, child],
        };
        let tree = owned_tree(&snapshot, 10)?;
        assert_eq!(tree.len(), 2);
        snapshot.processes.first_mut().context("root")?.born = "reused-pid".into();
        assert_eq!(remaining(&snapshot, &tree)?.len(), 1);
        snapshot.processes.last_mut().context("child")?.owner = "foreign".into();
        assert!(owned_tree(&snapshot, 10).is_err());
        Ok(())
    }
}
