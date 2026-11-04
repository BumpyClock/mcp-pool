use super::*;
use std::future::{Future, poll_fn};
use std::task::Poll;

async fn running(
    proxy: &Arc<SocketProxy>,
) -> io::Result<(
    oneshot::Receiver<()>,
    watch::Sender<Completion>,
    mpsc::Sender<String>,
    mpsc::Receiver<crate::upstream::UpstreamRequest>,
)> {
    let Backend {
        setup,
        handle,
        shutdown,
        retired,
        responses,
        requests,
    } = backend(proxy);
    setup
        .send(Ok(handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    proxy.start().await?;
    Ok((shutdown, retired, responses, requests))
}

async fn exercise(restart: bool, failure: bool) -> io::Result<()> {
    let first = proxy();
    let second = proxy();
    let (old_first_shutdown, old_first_retired, old_first_responses, old_first_requests) =
        running(&first).await?;
    let (old_second_shutdown, old_second_retired, old_second_responses, old_second_requests) =
        running(&second).await?;
    let pool = Arc::new(crate::pool::Pool::new());
    pool.insert_test_proxy("first", first.clone());
    pool.insert_test_proxy("second", second.clone());

    let (first_backend, second_backend, mut shutdown) = if restart {
        let next_first = backend(&first);
        let next_second = backend(&second);
        let restart_first = {
            let pool = pool.clone();
            tokio::spawn(async move { pool.restart("first").await })
        };
        let restart_second = {
            let pool = pool.clone();
            tokio::spawn(async move { pool.restart("second").await })
        };
        old_first_shutdown.await.map_err(io::Error::other)?;
        old_second_shutdown.await.map_err(io::Error::other)?;
        let mut shutdown = Box::pin(pool.shutdown());
        poll_fn(|cx| match shutdown.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("shutdown completed while restarts are admitted"),
        })
        .await;
        old_first_retired.send_replace(Some(Ok(())));
        old_second_retired.send_replace(Some(Ok(())));
        let Backend {
            setup: setup_first,
            handle: handle_first,
            shutdown: signal_first,
            retired: retired_first,
            responses: responses_first,
            requests: requests_first,
        } = next_first;
        let Backend {
            setup: setup_second,
            handle: handle_second,
            shutdown: signal_second,
            retired: retired_second,
            responses: responses_second,
            requests: requests_second,
        } = next_second;
        setup_first
            .send(Ok(handle_first))
            .map_err(|_| io::Error::other("setup lost"))?;
        setup_second
            .send(Ok(handle_second))
            .map_err(|_| io::Error::other("setup lost"))?;
        finish(restart_first).await?;
        finish(restart_second).await?;
        (
            (signal_first, retired_first, responses_first, requests_first),
            (
                signal_second,
                retired_second,
                responses_second,
                requests_second,
            ),
            shutdown,
        )
    } else {
        (
            (
                old_first_shutdown,
                old_first_retired,
                old_first_responses,
                old_first_requests,
            ),
            (
                old_second_shutdown,
                old_second_retired,
                old_second_responses,
                old_second_requests,
            ),
            Box::pin(pool.shutdown()),
        )
    };
    let (signal_first, retired_first, _responses_first, _requests_first) = first_backend;
    let (signal_second, retired_second, _responses_second, _requests_second) = second_backend;
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        let signals = async {
            signal_first.await.map_err(io::Error::other)?;
            signal_second.await.map_err(io::Error::other)?;
            Ok::<(), io::Error>(())
        };
        tokio::select! {
            result = &mut shutdown => panic!("shutdown completed before backend retirements: {result:?}"),
            result = signals => result?,
        }
        if failure {
            retired_first.send_replace(Some(Err("retirement unverified".into())));
            tokio::select! {
                result = &mut shutdown => panic!("shutdown did not drain the healthy backend: {result:?}"),
                () = async {
                    while first.status() != ServerStatus::Failed {
                        tokio::task::yield_now().await;
                    }
                } => {},
            }
        } else {
            retired_second.send_replace(Some(Ok(())));
        }
        poll_fn(|cx| match shutdown.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(result) => panic!("shutdown completed before both retirements: {result:?}"),
        }).await;
        if failure {
            retired_second.send_replace(Some(Ok(())));
        } else {
            retired_first.send_replace(Some(Ok(())));
        }
        shutdown.await
    }).await.map_err(io::Error::other)?;
    if failure {
        assert_eq!(
            result.err().map(|error| error.to_string()).as_deref(),
            Some("retirement unverified")
        );
        assert_eq!(pool.get_status().server_count, 1);
        assert_eq!(first.status(), ServerStatus::Failed);
        assert!(pool.restart("first").await.is_err());
        assert!(pool.start("first", first.spec.clone(), None).await.is_err());
    } else {
        result?;
        assert_eq!(pool.get_status().server_count, 0);
        assert!(!first.readiness().local_socket_bound);
        assert!(!second.readiness().local_socket_bound);
        assert_eq!(first.connection_count(), 0);
        assert_eq!(second.connection_count(), 0);
    }
    Ok(())
}

#[tokio::test]
async fn shutdown_signals_replacements_from_admitted_restarts_before_waiting() -> io::Result<()> {
    exercise(true, false).await
}

#[tokio::test]
async fn shutdown_signals_running_backends_before_waiting() -> io::Result<()> {
    exercise(false, false).await
}

#[tokio::test]
async fn shutdown_drains_all_and_retains_failed_generation() -> io::Result<()> {
    exercise(true, true).await
}
