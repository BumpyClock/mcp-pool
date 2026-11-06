use super::*;
use std::time::Duration;

pub(super) fn durations(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut number = String::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character.is_ascii_digit()
            || (character == '.'
                && !number.is_empty()
                && !number.contains('.')
                && characters.peek().is_some_and(char::is_ascii_digit))
        {
            number.push(character);
            continue;
        }
        if !number.is_empty() {
            let milliseconds = character == 'm' && characters.peek() == Some(&'s');
            let seconds = character == 's' && number.contains('.');
            if milliseconds || seconds {
                normalized.push_str("Nms");
                if milliseconds {
                    characters.next();
                }
                number.clear();
                continue;
            }
            normalized.push_str(&number);
            number.clear();
        }
        normalized.push(character);
    }
    normalized.push_str(&number);
    normalized
}

#[test]
fn duration_snapshots_ignore_elapsed_time_units() {
    for value in ["999ms", "1.0s", "12.3s"] {
        assert_eq!(durations(value), "Nms");
    }
    assert_eq!(
        durations("HTTP 401, 3 tools, version 0.1.0"),
        "HTTP 401, 3 tools, version 0.1.0"
    );
    assert_eq!(durations("(999ms); 1.0s; 12.3s."), "(Nms); Nms; Nms.");
}

#[tokio::test]
async fn slow_discovery_preserves_the_output_snapshot() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let gate = fixture.home.join("delayed-discovery");
    let gate_path = gate.display().to_string();
    let placeholders = configure(&fixture, |template| {
        Map::from_iter([(
            "alpha".to_owned(),
            with_environment(template, LIST_GATE, &gate_path),
        )])
    })
    .await?;
    fixture.warm("alpha").await?;
    let release = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while fixture.event_count("tools/list").await? == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok::<(), io::Error>(())
        })
        .await
        .map_err(io::Error::other)??;
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        tokio::fs::write(&gate, b"release").await
    };
    let (output, released) = tokio::join!(fixture.command(&["list"]), release);
    released?;
    let output = output?;
    assert!(output.status.success(), "{output:?}");
    let raw = String::from_utf8_lossy(&output.stdout);
    let row = raw
        .lines()
        .find(|line| line.starts_with("- alpha "))
        .ok_or_else(|| io::Error::other(format!("missing aggregate row: {raw}")))?;
    let duration = row
        .rsplit_once(", ")
        .and_then(|(_, duration)| duration.strip_suffix("s)"))
        .ok_or_else(|| io::Error::other(format!("missing seconds duration: {row}")))?;
    let (seconds, fraction) = duration
        .split_once('.')
        .ok_or_else(|| io::Error::other(format!("duration is not decimal seconds: {row}")))?;
    assert!(!seconds.is_empty() && seconds.chars().all(|character| character.is_ascii_digit()));
    assert_eq!(fraction.len(), 1);
    assert!(fraction.chars().all(|character| character.is_ascii_digit()));
    assert_eq!(
        placeholders.text(&output),
        "mcp-cli <version> - Listing 1 server\n- alpha (4 tools, Nms)\n✓ Listed 1 server (1 healthy).\n"
    );
    fixture.finish().await
}
