use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use super::tests::{fixture, read, send};

#[tokio::test]
async fn listener_returns_notifications_without_retiring_connection() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    for notification in [
        json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
        json!({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"hello"}}),
    ] {
        let (received, sent) = tokio::join!(
            client.wait_for_notification(),
            send(&mut server, notification.clone())
        );
        sent?;
        assert_eq!(received?, Some(notification));
        assert!(!client.is_closed());
    }
    Ok(())
}

#[tokio::test]
async fn listener_answers_callbacks_before_returning_notification() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    let server_work = async {
        send(
            &mut server,
            json!({"jsonrpc":"2.0","id":"ping-id","method":"ping"}),
        )
        .await?;
        assert_eq!(
            read(&mut server).await?,
            json!({"jsonrpc":"2.0","id":"ping-id","result":{}})
        );
        send(
            &mut server,
            json!({"jsonrpc":"2.0","id":17,"method":"roots/list","params":{}}),
        )
        .await?;
        assert_eq!(
            read(&mut server).await?,
            json!({"jsonrpc":"2.0","id":17,"error":{"code":-32601,"message":"Unsupported client method: roots/list"}})
        );
        send(
            &mut server,
            json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{}}),
        )
        .await
    };
    let (notification, served) = tokio::join!(client.wait_for_notification(), server_work);
    served?;
    assert_eq!(
        notification?,
        Some(json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{}}))
    );
    assert!(!client.is_closed());
    Ok(())
}

#[tokio::test]
async fn listener_reports_clean_eof_and_retires_connection() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    server.get_mut().shutdown().await?;
    assert_eq!(client.wait_for_notification().await?, None);
    assert!(client.is_closed());
    assert!(client.wait_for_notification().await.is_err());
    Ok(())
}

#[tokio::test]
async fn listener_rejects_partial_eof_and_invalid_frames() -> Result<()> {
    for frame in [
        "not-json\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}",
        "{\"jsonrpc\":\"1.0\",\"method\":\"notifications/tools/list_changed\"}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":null}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":null}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"result\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n",
    ] {
        let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
        let server_work = async {
            server.get_mut().write_all(frame.as_bytes()).await?;
            server.get_mut().shutdown().await?;
            Ok::<(), anyhow::Error>(())
        };
        let (result, served) = tokio::join!(client.wait_for_notification(), server_work);
        served?;
        assert!(result.is_err(), "listener accepted invalid frame: {frame}");
        assert!(client.is_closed());
    }
    Ok(())
}

#[tokio::test]
async fn cancelled_listener_read_retires_connection() -> Result<()> {
    let (mut client, _server) = fixture(Duration::from_secs(1)).await?;
    tokio::select! {
        result = client.wait_for_notification() => {
            anyhow::bail!("listener unexpectedly completed: {result:?}");
        }
        () = tokio::time::sleep(Duration::from_millis(20)) => {}
    }
    assert!(client.is_closed());
    assert!(client.request("ping", json!({})).await.is_err());
    Ok(())
}

#[tokio::test]
async fn caller_deadline_bounds_listener_even_with_callback_traffic() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    let server_work = async {
        let request = json!({"jsonrpc":"2.0","id":"ping-id","method":"ping"});
        loop {
            if send(&mut server, request.clone()).await.is_err() {
                return Ok::<(), anyhow::Error>(());
            }
            match read(&mut server).await {
                Ok(response) => {
                    assert_eq!(response.get("result"), Some(&json!({})));
                }
                Err(_) => return Ok(()),
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    let (result, served) = tokio::join!(
        tokio::time::timeout(Duration::from_millis(30), client.wait_for_notification()),
        server_work
    );
    served?;
    assert!(result.is_err());
    assert!(client.is_closed());
    Ok(())
}

#[tokio::test]
async fn unbounded_listener_stays_idle_beyond_call_deadline_and_handles_callbacks() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    client.deadline = Duration::from_millis(20);
    let server_work = async {
        tokio::time::sleep(Duration::from_millis(60)).await;
        send(
            &mut server,
            json!({"jsonrpc":"2.0","id":"idle-ping","method":"ping"}),
        )
        .await?;
        assert_eq!(
            read(&mut server).await?,
            json!({"jsonrpc":"2.0","id":"idle-ping","result":{}})
        );
        send(
            &mut server,
            json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
        )
        .await
    };
    let (notification, served) = tokio::join!(client.wait_for_notification(), server_work);
    served?;
    assert_eq!(
        notification?,
        Some(json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}))
    );
    assert!(!client.is_closed());
    Ok(())
}

#[tokio::test]
async fn caller_cancellation_retires_unbounded_listener_socket() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), client.wait_for_notification())
            .await
            .is_err()
    );
    assert!(client.is_closed());
    let mut frame = String::new();
    assert_eq!(server.read_line(&mut frame).await?, 0);
    assert!(client.wait_for_notification().await.is_err());
    Ok(())
}
