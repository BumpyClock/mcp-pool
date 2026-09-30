use super::*;

const UPSTREAM_READY_TIMEOUT_SECS: u64 = 30;

/// Obtain the upstream request sender, waiting if the upstream is still starting.
/// Returns `None` only if the upstream stops or never publishes a sender within
/// the timeout, so a client's first request is queued through cold start instead
/// of being silently dropped.
pub(super) async fn acquire_request_sender(
    request_tx: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    upstream_ready: &Arc<Notify>,
    shutdown: &Arc<AtomicBool>,
    client_id: &str,
) -> Option<mpsc::Sender<String>> {
    if let Some(sender) = request_tx.lock().clone() {
        return Some(sender);
    }
    diagnostics::log(format!("pool_upstream_wait client_id={}", client_id));
    let deadline = Instant::now() + Duration::from_secs(UPSTREAM_READY_TIMEOUT_SECS);
    loop {
        // Arm the waiter before re-checking the slot so a notify firing between
        // the check and the await is not lost (lost-wakeup safe).
        let ready = upstream_ready.notified();
        tokio::pin!(ready);
        ready.as_mut().enable();

        if let Some(sender) = request_tx.lock().clone() {
            return Some(sender);
        }
        if shutdown.load(Ordering::SeqCst) {
            return None;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return request_tx.lock().clone();
        }
        if tokio::time::timeout(remaining, ready).await.is_err() {
            return request_tx.lock().clone();
        }
    }
}
