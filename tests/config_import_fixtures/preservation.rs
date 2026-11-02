use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;

pub struct Snapshot {
    bytes: Vec<u8>,
    modified: SystemTime,
}

impl Snapshot {
    pub fn decorate(path: &Path) -> Result<Self> {
        let document = std::fs::read_to_string(path)?;
        let contents = format!(
            "\u{feff}// Keep this destination comment.\r\n{}\r\n \t\r\n",
            document.replace('\n', "\r\n")
        );
        std::fs::write(path, &contents)?;
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_234_567_890);
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)?
            .set_times(std::fs::FileTimes::new().set_modified(modified))?;
        Ok(Self {
            bytes: contents.into_bytes(),
            modified: std::fs::metadata(path)?.modified()?,
        })
    }

    pub fn assert_unchanged(&self, path: &Path) -> Result<()> {
        let modified = std::fs::metadata(path)?.modified()?;
        let bytes = std::fs::read(path)?;
        assert!(
            bytes == self.bytes && modified == self.modified,
            "bytes preserved: {} ({} -> {}); mtime preserved: {} ({:?} -> {:?})",
            bytes == self.bytes,
            self.bytes.len(),
            bytes.len(),
            modified == self.modified,
            self.modified,
            modified
        );
        assert!(!path.with_extension("json.lock").exists());
        assert!(
            std::fs::read_dir(path.parent().ok_or_else(|| anyhow::anyhow!("parent"))?)?
                .collect::<Result<Vec<_>, _>>()?
                .iter()
                .all(|entry| !entry.file_name().to_string_lossy().contains(".write-"))
        );
        Ok(())
    }
}
