use super::*;

/// Registers readiness before rechecking the sender to prevent a lost wakeup.
pub(super) async fn acquire_request_sender(
    request_tx: &Mutex<Option<mpsc::Sender<crate::upstream::UpstreamRequest>>>,
    upstream_ready: &Notify,
    shutdown: &AtomicBool,
    client_id: &str,
) -> Option<mpsc::Sender<crate::upstream::UpstreamRequest>> {
    if let Some(sender) = request_tx.lock().clone() {
        return Some(sender);
    }

    diagnostics::log(format!("pool_upstream_wait client_id={}", client_id));
    loop {
        let ready = upstream_ready.notified();
        tokio::pin!(ready);
        ready.as_mut().enable();

        if let Some(sender) = request_tx.lock().clone() {
            return Some(sender);
        }
        if shutdown.load(Ordering::SeqCst) {
            return None;
        }
        ready.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deferred_sender_waits_until_publication_or_shutdown() -> io::Result<()> {
        let generation = Generation::new();
        let waiting = acquire_request_sender(
            &generation.request_tx,
            &generation.upstream_ready,
            &generation.shutdown,
            "deferred",
        );
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(40), &mut waiting)
                .await
                .is_err()
        );
        let (sender, mut receiver) = mpsc::channel(1);
        *generation.request_tx.lock() = Some(sender);
        generation.upstream_ready.notify_waiters();
        let sender = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("deferred sender lost"))?;
        sender
            .send("initialize".into())
            .await
            .map_err(io::Error::other)?;
        assert_eq!(receiver.recv().await.as_deref(), Some("initialize"));

        let stopped = Generation::new();
        let waiting = acquire_request_sender(
            &stopped.request_tx,
            &stopped.upstream_ready,
            &stopped.shutdown,
            "stopped",
        );
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(40), &mut waiting)
                .await
                .is_err()
        );
        stopped.signal_shutdown();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .map_err(io::Error::other)?
                .is_none()
        );
        Ok(())
    }
}
