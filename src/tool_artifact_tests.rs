use super::*;
use serde_json::json;

fn directory(label: &str) -> Result<PathBuf> {
    Ok(std::env::current_dir()?
        .join("target")
        .join(format!("artifact-{label}-{}", std::process::id())))
}

#[test]
fn image_collisions_preserve_existing_bytes_and_exhaust_the_attempt_bound() -> Result<()> {
    let directory = directory("collision")?;
    std::fs::create_dir_all(&directory)?;
    let outcome = (|| {
        let images = vec![json!({"type":"image","data":"aGVsbG8=","mimeType":"image/png"})];
        let existing = directory.join("mcp-image-42-0-0.png");
        std::fs::write(&existing, b"prior bytes")?;
        let paths = save_at(&directory, &images, 42)?;
        assert_eq!(paths, vec![directory.join("mcp-image-42-0-1.png")]);
        assert_eq!(std::fs::read(&existing)?, b"prior bytes");
        assert_eq!(
            std::fs::read(paths.first().context("saved image")?)?,
            b"hello"
        );
        for attempt in 0..100 {
            std::fs::write(
                directory.join(format!("mcp-image-99-0-{attempt}.png")),
                b"occupied",
            )?;
        }
        let error = save_at(&directory, &images, 99)
            .err()
            .context("all names must collide")?;
        assert!(error.to_string().contains("collision-free"));
        for attempt in 0..100 {
            assert_eq!(
                std::fs::read(directory.join(format!("mcp-image-99-0-{attempt}.png")))?,
                b"occupied"
            );
        }
        assert_eq!(
            std::fs::read_dir(&directory)?
                .collect::<std::io::Result<Vec<_>>>()?
                .len(),
            102
        );
        Ok(())
    })();
    std::fs::remove_dir_all(directory)?;
    outcome
}

#[test]
fn image_size_limits_accept_the_boundary_and_reject_before_writing() -> Result<()> {
    let directory = directory("size")?;
    let outcome = (|| {
        let data = vec![42; 16 * 1024 * 1024];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        let images = vec![json!({"type":"image","data":encoded,"mimeType":"image/png"})];
        let paths = save_at(&directory, &images, 1)?;
        assert_eq!(
            std::fs::read(paths.first().context("boundary image")?)?,
            data
        );
        let oversized =
            base64::engine::general_purpose::STANDARD.encode(vec![42; 16 * 1024 * 1024 + 1]);
        let images = vec![json!({"type":"image","data":oversized})];
        let error = save_at(&directory, &images, 2)
            .err()
            .context("decoded limit must reject")?;
        assert!(error.to_string().contains("16 MiB"));
        let images = vec![json!({"type":"image","data":"!".repeat(24 * 1024 * 1024 + 1)})];
        let error = save_at(&directory, &images, 3)
            .err()
            .context("encoded limit must reject before decode")?;
        assert!(error.to_string().contains("16 MiB"));
        assert_eq!(
            std::fs::read_dir(&directory)?
                .collect::<std::io::Result<Vec<_>>>()?
                .len(),
            1
        );
        Ok(())
    })();
    if directory.exists() {
        std::fs::remove_dir_all(directory)?;
    }
    outcome
}

#[test]
fn log_tails_bound_bytes_and_lines_and_reject_relative_paths() -> Result<()> {
    let directory = directory("log")?;
    std::fs::create_dir_all(&directory)?;
    let outcome = (|| {
        let log = directory.join("fixture.log");
        std::fs::write(
            &log,
            (0..30)
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )?;
        let tail = read_tail(&log)?;
        assert_eq!(tail.lines().count(), 20);
        assert!(tail.starts_with("10\n"));
        for contents in ["", "one", "one\ntwo\n"] {
            std::fs::write(&log, contents)?;
            assert_eq!(read_tail(&log)?, contents.trim_end_matches('\n'));
        }
        std::fs::write(&log, format!("{}partial\nlast\n", "x".repeat(1024 * 1024)))?;
        assert_eq!(read_tail(&log)?, "last");
        assert!(read_tail(Path::new("relative.log")).is_err());
        Ok(())
    })();
    std::fs::remove_dir_all(directory)?;
    outcome
}

#[tokio::test]
async fn artifact_extraction_only_uses_top_level_content() -> Result<()> {
    let directory = directory("envelope")?;
    save_images(
        &json!({"result":{"content":[{"type":"image","data":"aGVsbG8="}]},
        "structuredContent":{"content":[{"type":"image","data":"aGVsbG8="}]}}),
        Some(&directory),
    )
    .await?;
    assert!(!directory.exists());
    Ok(())
}
