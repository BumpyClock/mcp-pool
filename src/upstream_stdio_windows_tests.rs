use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use super::{FIXTURE, FIXTURE_TEST, spawn};

struct FixtureDirectory(PathBuf);

impl FixtureDirectory {
    fn new() -> io::Result<Self> {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::current_dir()?.join("target").join(format!(
            "windows launch {} {}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory)?;
        Ok(Self(directory))
    }

    fn launcher(&self, name: &str) -> io::Result<PathBuf> {
        let launcher = self.0.join(name);
        std::fs::write(
            &launcher,
            format!(
                "@echo off\r\n\"{}\" %*\r\n",
                std::env::current_exe()?.display()
            ),
        )?;
        Ok(launcher)
    }
}

impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("Windows launch fixture cleanup failed: {error}");
        }
    }
}

fn arguments(literals: &[&str]) -> Vec<String> {
    let mut arguments = vec![
        "--exact".to_string(),
        FIXTURE_TEST.to_string(),
        "--nocapture".to_string(),
    ];
    for literal in literals {
        arguments.push("--skip".to_string());
        arguments.push((*literal).to_string());
    }
    arguments
}

async fn assert_arguments(
    command: &Path,
    literals: &[&str],
    mut environment: BTreeMap<String, String>,
) -> io::Result<()> {
    environment.insert(FIXTURE.to_string(), "argv".to_string());
    environment.insert("MCP_POOL_LITERAL".to_string(), "EXPANDED".to_string());
    let expected = arguments(literals);
    let (responses, mut receiver) = mpsc::channel(32);
    let mut handle = spawn(
        command.to_string_lossy().into_owned(),
        expected.clone(),
        environment,
        responses,
    )
    .await?;
    let observed = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(line) = receiver.recv().await {
            if let Some(json) = line.strip_prefix("POOL_ARGV:") {
                return serde_json::from_str::<Vec<String>>(json).map_err(io::Error::other);
            }
        }
        Err(io::Error::other(
            "launcher closed before reporting arguments",
        ))
    })
    .await
    .map_err(io::Error::other)
    .and_then(|result| result);
    handle.shutdown().await?;
    assert_eq!(observed?, expected);
    Ok(())
}

#[tokio::test]
async fn native_executable_path_and_arguments_with_spaces() -> io::Result<()> {
    let directory = FixtureDirectory::new()?;
    let executable = directory.0.join("argv fixture.exe");
    std::fs::copy(std::env::current_exe()?, &executable)?;
    assert_arguments(
        &executable,
        &[
            "argument with spaces",
            "",
            "trailing slash \\",
            "quote \" inside",
        ],
        BTreeMap::new(),
    )
    .await
}

#[tokio::test]
async fn native_arguments_are_not_shell_syntax() -> io::Result<()> {
    assert_arguments(
        &std::env::current_exe()?,
        &[
            "a&b",
            "a|b",
            "a^b",
            "%MCP_POOL_LITERAL%",
            "!MCP_POOL_LITERAL!",
        ],
        BTreeMap::new(),
    )
    .await
}

#[tokio::test]
async fn batch_paths_and_argument_literals() -> io::Result<()> {
    let directory = FixtureDirectory::new()?;
    for name in ["argv launcher.cmd", "argv launcher.bat"] {
        let launcher = directory.launcher(name)?;
        assert_arguments(
            &launcher,
            &[
                "argument with spaces",
                "",
                "a&b",
                "a|b",
                "a^b",
                "%MCP_POOL_LITERAL%",
                "!MCP_POOL_LITERAL!",
                "(parentheses)",
                "trailing slash \\",
                "quote \" inside",
            ],
            BTreeMap::new(),
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn batch_launcher_resolves_from_configured_path_and_pathext() -> io::Result<()> {
    let directory = FixtureDirectory::new()?;
    directory.launcher("pool-npm-launcher.cmd")?;
    assert_arguments(
        Path::new("pool-npm-launcher"),
        &["argument with spaces", "a&b", "%MCP_POOL_LITERAL%"],
        BTreeMap::from([
            (
                "pAtH".to_string(),
                directory.0.to_string_lossy().into_owned(),
            ),
            ("pAtHeXt".to_string(), ".CMD;.EXE".to_string()),
        ]),
    )
    .await
}

#[tokio::test]
async fn native_launcher_resolves_the_selected_path_candidate() -> io::Result<()> {
    let directory = FixtureDirectory::new()?;
    let executable = directory.0.join("pool-native-launcher.exe");
    std::fs::copy(std::env::current_exe()?, &executable)?;
    assert_arguments(
        Path::new("pool-native-launcher"),
        &["argument with spaces", "a&b", "%MCP_POOL_LITERAL%"],
        BTreeMap::from([
            (
                "PATH".to_string(),
                directory.0.to_string_lossy().into_owned(),
            ),
            ("PATHEXT".to_string(), ".EXE".to_string()),
        ]),
    )
    .await
}

#[tokio::test]
async fn batch_arguments_with_newlines_fail_before_launch() -> io::Result<()> {
    let directory = FixtureDirectory::new()?;
    let launcher = directory.launcher("argv launcher.cmd")?;
    for argument in ["line\nbreak", "line\rbreak"] {
        let (responses, _receiver) = mpsc::channel(1);
        let result = spawn(
            launcher.to_string_lossy().into_owned(),
            vec![argument.to_string()],
            BTreeMap::new(),
            responses,
        )
        .await;
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(io::ErrorKind::InvalidInput)
        );
    }
    Ok(())
}

#[tokio::test]
async fn batch_launcher_descendants_retire_with_the_job() -> io::Result<()> {
    let directory = FixtureDirectory::new()?;
    let launcher = directory.launcher("tree launcher.cmd")?;
    let (responses, mut receiver) = mpsc::channel(32);
    let mut handle = spawn(
        launcher.to_string_lossy().into_owned(),
        arguments(&[]),
        BTreeMap::from([(FIXTURE.to_string(), "hold".to_string())]),
        responses,
    )
    .await?;
    let identifiers = super::tree(&mut receiver).await?;
    handle.shutdown().await?;
    super::assert_retired(&identifiers)
}
