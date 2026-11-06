use super::*;
use std::io;
use std::path::PathBuf;

#[path = "fixture_home.rs"]
mod fixture_home;

struct Sources(PathBuf);

impl Sources {
    fn new() -> io::Result<Self> {
        let root = fixture_home::create("mcp-build")?;
        fs::create_dir(root.join("src"))?;
        for input in ["Cargo.toml", "Cargo.lock", "build.rs", "src/main.rs"] {
            fs::write(root.join(input), format!("fixture {input}\n"))?;
        }
        Ok(Self(root))
    }
}

impl Drop for Sources {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "Could not remove build identity fixture {}: {error}",
                self.0.display()
            );
        }
    }
}

#[test]
fn identity_tracks_source_bytes_not_checkout_location() -> io::Result<()> {
    let first = Sources::new()?;
    let relocated = Sources::new()?;
    let initial = source_fingerprint(&first.0);
    assert!(initial.errors.is_empty());
    assert_eq!(initial.value, source_fingerprint(&relocated.0).value);
    for input in SOURCE_INPUTS {
        let path = if *input == "src" {
            "src/main.rs"
        } else {
            input
        };
        let original = fs::read(first.0.join(path))?;
        fs::write(first.0.join(path), b"changed bytes")?;
        assert_ne!(
            initial.value,
            source_fingerprint(&first.0).value,
            "unchanged fingerprint after changing {path}"
        );
        fs::write(first.0.join(path), original)?;
    }
    fs::write(first.0.join("src/new.rs"), b"new module")?;
    assert_ne!(initial.value, source_fingerprint(&first.0).value);
    Ok(())
}

#[test]
fn missing_inputs_are_distinct_from_complete_source_without_git() -> io::Result<()> {
    let source = Sources::new()?;
    let initial = source_fingerprint(&source.0);
    assert!(initial.errors.is_empty());
    assert_eq!(git_identity(&source.0).value, "nogit");
    fs::remove_file(source.0.join("Cargo.lock"))?;
    let incomplete = source_fingerprint(&source.0);
    assert!(!incomplete.errors.is_empty());
    assert_ne!(initial.value, incomplete.value);
    Ok(())
}
