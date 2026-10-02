use super::*;

fn assert_final_queued(responses: &mut mpsc::Receiver<String>) {
    let mut final_responses = Vec::new();
    while let Ok(line) = responses.try_recv() {
        if line == FINAL_RESPONSE {
            final_responses.push(line);
        }
    }
    assert_eq!(
        final_responses,
        vec![r#"{"jsonrpc":"2.0","id":19,"result":{"final":true}}"#.to_string()]
    );
}

#[tokio::test]
async fn final_response_is_queued_before_natural_exit_completion() -> io::Result<()> {
    let mut fixtures = tokio::task::JoinSet::new();
    for _ in 0..32 {
        fixtures.spawn(async {
            let (mut handle, mut responses) = fixture("final", 32).await?;
            tokio::time::timeout(Duration::from_secs(15), handle.wait_for_exit())
                .await
                .map_err(io::Error::other)??;
            assert_final_queued(&mut responses);
            handle.wait_for_exit().await
        });
    }
    while let Some(result) = fixtures.join_next().await {
        result.map_err(io::Error::other)??;
    }
    Ok(())
}

#[tokio::test]
async fn inherited_stdout_is_closed_before_final_response_drain() -> io::Result<()> {
    let (mut handle, mut responses) = fixture("final-descendant", 32).await?;
    let identifiers = tree(&mut responses).await?;
    tokio::time::timeout(Duration::from_secs(15), handle.wait_for_exit())
        .await
        .map_err(io::Error::other)??;
    assert_retired(&identifiers)?;
    assert_final_queued(&mut responses);
    Ok(())
}

#[tokio::test]
async fn full_response_queue_does_not_hold_natural_exit_completion() -> io::Result<()> {
    let (mut handle, _responses) = fixture("final-flood", 1).await?;
    tokio::time::timeout(Duration::from_secs(15), handle.wait_for_exit())
        .await
        .map_err(io::Error::other)??;
    handle.wait_for_exit().await
}
