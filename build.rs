use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=src");

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for location in ["--git-dir", "--git-common-dir"] {
        if let Ok(directory) = git_output(manifest_dir, &["rev-parse", location])
            && !directory.is_empty()
        {
            let directory = manifest_dir.join(directory);
            for name in ["HEAD", "index", "packed-refs", "refs"] {
                println!("cargo:rerun-if-changed={}", directory.join(name).display());
            }
        }
    }
    let source_fingerprint = source_fingerprint(manifest_dir);
    let git_identity = git_identity(manifest_dir);
    let source_identity = if source_fingerprint.errors.is_empty() {
        format!("src.{}", source_fingerprint.value)
    } else {
        format!("src.incomplete.{}", source_fingerprint.value)
    };
    let build_version = format!(
        "{}+{}.{}",
        env!("CARGO_PKG_VERSION"),
        git_identity.value,
        source_identity
    );

    if !source_fingerprint.errors.is_empty() {
        println!(
            "cargo:warning=mcp-pool build identity: {} source fingerprint inputs could not be read",
            source_fingerprint.errors.len()
        );
    }
    if let Some(warning) = git_identity.warning {
        println!("cargo:warning={warning}");
    }
    println!("cargo:rustc-env=MCP_POOL_BUILD_VERSION={build_version}");
}

struct GitIdentity {
    value: String,
    warning: Option<String>,
}

struct SourceFingerprint {
    value: String,
    errors: Vec<String>,
}

fn git_identity(manifest_dir: &Path) -> GitIdentity {
    let commit = git_output(manifest_dir, &["rev-parse", "--short=12", "HEAD"])
        .ok()
        .filter(|value| !value.is_empty());
    let status = git_output(
        manifest_dir,
        &["status", "--porcelain", "--untracked-files=all"],
    );

    match (commit, status) {
        (Some(commit), Ok(status)) => {
            let state = if status.is_empty() { "clean" } else { "dirty" };
            GitIdentity {
                value: format!("git.{}.{}", sanitize_component(&commit), state),
                warning: None,
            }
        }
        (Some(commit), Err(())) => GitIdentity {
            value: format!("git.{}.unknown", sanitize_component(&commit)),
            warning: Some(
                "mcp-pool build identity: git status was unavailable; source fingerprint remains authoritative"
                    .to_owned(),
            ),
        },
        (None, _) => GitIdentity {
            value: "nogit".to_owned(),
            warning: Some(
                "mcp-pool build identity: git metadata unavailable; using source fingerprint"
                    .to_owned(),
            ),
        },
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "Build scripts run synchronously, outside the daemon runtime."
)]
fn git_output(manifest_dir: &Path, arguments: &[&str]) -> Result<String, ()> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(manifest_dir)
        .output()
        .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    }
}

fn source_fingerprint(manifest_dir: &Path) -> SourceFingerprint {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for relative_path in ["Cargo.toml", "Cargo.lock", "build.rs"] {
        collect_files(
            &manifest_dir.join(relative_path),
            Path::new(relative_path),
            &mut files,
            &mut errors,
        );
    }
    collect_files(
        &manifest_dir.join("src"),
        Path::new("src"),
        &mut files,
        &mut errors,
    );

    files.sort_by(|left, right| left.0.cmp(&right.0));
    errors.sort();

    let mut fingerprint = Fnv64::new();
    for (relative_path, content) in files {
        fingerprint.update(relative_path.as_bytes());
        fingerprint.update(&[0]);
        fingerprint.update(&content);
        fingerprint.update(&[0xff]);
    }
    for error in &errors {
        fingerprint.update(b"ERROR");
        fingerprint.update(&[0]);
        fingerprint.update(error.as_bytes());
        fingerprint.update(&[0xff]);
    }
    SourceFingerprint {
        value: format!("{:016x}", fingerprint.finish()),
        errors,
    }
}

fn collect_files(
    path: &Path,
    relative_path: &Path,
    files: &mut Vec<(String, Vec<u8>)>,
    errors: &mut Vec<String>,
) {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            errors.push(format!("missing:{}", relative_path.to_string_lossy()));
            return;
        }
        Err(error) => {
            errors.push(format!(
                "metadata:{}:{}",
                relative_path.to_string_lossy(),
                error.kind()
            ));
            return;
        }
    };

    if metadata.is_dir() {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                errors.push(format!(
                    "directory:{}:{}",
                    relative_path.to_string_lossy(),
                    error.kind()
                ));
                return;
            }
        };
        let mut children = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => children.push(entry.path()),
                Err(error) => errors.push(format!(
                    "entry:{}:{}",
                    relative_path.to_string_lossy(),
                    error.kind()
                )),
            }
        }
        children.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
        for child in children {
            let child_name = match child.file_name() {
                Some(name) => name,
                None => {
                    errors.push(format!("path:{}", child.to_string_lossy()));
                    continue;
                }
            };
            collect_files(&child, &relative_path.join(child_name), files, errors);
        }
        return;
    }

    if !metadata.is_file() {
        errors.push(format!("unsupported:{}", relative_path.to_string_lossy()));
        return;
    }

    match fs::read(path) {
        Ok(content) => files.push((relative_path.to_string_lossy().replace('\\', "/"), content)),
        Err(error) => errors.push(format!(
            "read:{}:{}",
            relative_path.to_string_lossy(),
            error.kind()
        )),
    }
}

struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}
