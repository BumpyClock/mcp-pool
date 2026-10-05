use std::collections::BTreeMap;
use std::io;

use tokio::sync::{mpsc, oneshot, watch};

#[derive(Debug, Clone)]
pub enum UpstreamSpec {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        sse: bool,
    },
}

pub(crate) type Completion = Option<Result<(), String>>;

pub struct UpstreamHandle {
    pub request_tx: mpsc::Sender<String>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    completion: watch::Receiver<Completion>,
}

impl UpstreamHandle {
    pub(crate) fn new(
        request_tx: mpsc::Sender<String>,
        shutdown_tx: oneshot::Sender<()>,
        completion: watch::Receiver<Completion>,
    ) -> Self {
        Self {
            request_tx,
            shutdown_tx: Some(shutdown_tx),
            completion,
        }
    }

    pub async fn spawn(spec: UpstreamSpec, response_tx: mpsc::Sender<String>) -> io::Result<Self> {
        match spec {
            UpstreamSpec::Stdio { command, args, env } => {
                crate::upstream_stdio::spawn(command, args, env, response_tx).await
            }
            UpstreamSpec::Http { url, sse } => {
                crate::upstream_http::spawn(url, sse, response_tx).await
            }
        }
    }

    pub async fn wait_for_exit(&mut self) -> io::Result<()> {
        loop {
            if let Some(result) = self.completion.borrow().clone() {
                return result.map_err(io::Error::other);
            }
            self.completion.changed().await.map_err(|_| {
                io::Error::other("upstream owner exited without confirming retirement")
            })?;
        }
    }

    pub async fn shutdown(&mut self) -> io::Result<()> {
        if let Some(sender) = self.shutdown_tx.take()
            && sender.send(()).is_err()
        {
            crate::diagnostics::log("upstream_shutdown_owner_already_exited");
        }
        self.wait_for_exit().await
    }
}

impl Drop for UpstreamHandle {
    fn drop(&mut self) {
        if let Some(sender) = self.shutdown_tx.take()
            && sender.send(()).is_err()
        {
            crate::diagnostics::log("upstream_drop_owner_already_exited");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_waits_for_confirmed_retirement() -> io::Result<()> {
        let (request_tx, _request_rx) = mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (completion_tx, completion_rx) = watch::channel(None);
        let mut handle = UpstreamHandle::new(request_tx, shutdown_tx, completion_rx);
        let (retired_tx, retired_rx) = oneshot::channel();
        let owner = tokio::spawn(async move {
            shutdown_rx.await.map_err(io::Error::other)?;
            retired_rx.await.map_err(io::Error::other)?;
            completion_tx.send(Some(Ok(()))).map_err(io::Error::other)
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), handle.shutdown())
                .await
                .is_err(),
            "requesting shutdown must not imply retirement"
        );
        retired_tx
            .send(())
            .map_err(|_| io::Error::other("retirement owner disappeared"))?;
        handle.shutdown().await?;
        handle.wait_for_exit().await?;
        owner.await.map_err(io::Error::other)??;
        Ok(())
    }

    #[tokio::test]
    async fn failed_retirement_remains_an_error() -> io::Result<()> {
        let (request_tx, _request_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        let (completion_tx, completion_rx) = watch::channel(None);
        let mut handle = UpstreamHandle::new(request_tx, shutdown_tx, completion_rx);
        completion_tx
            .send(Some(Err("retirement unverified".to_string())))
            .map_err(io::Error::other)?;
        assert_eq!(
            handle.shutdown().await.err().map(|error| error.to_string()),
            Some("retirement unverified".to_string())
        );
        assert_eq!(
            handle
                .wait_for_exit()
                .await
                .err()
                .map(|error| error.to_string()),
            Some("retirement unverified".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_retirement_confirmation_is_an_error() {
        let (request_tx, _request_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        let (completion_tx, completion_rx) = watch::channel(None);
        let mut handle = UpstreamHandle::new(request_tx, shutdown_tx, completion_rx);
        drop(completion_tx);
        assert_eq!(
            handle
                .wait_for_exit()
                .await
                .err()
                .map(|error| error.to_string()),
            Some("upstream owner exited without confirming retirement".to_string())
        );
    }
}
