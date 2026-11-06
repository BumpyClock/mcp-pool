use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone)]
pub enum UpstreamSpec {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
        clear_env: bool,
    },
    Http {
        url: String,
        sse: bool,
        headers: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
        auth: Option<Box<crate::oauth::HttpAuth>>,
    },
}

impl std::fmt::Debug for UpstreamSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio { cwd, .. } => formatter
                .debug_struct("Stdio")
                .field("cwd", cwd)
                .finish_non_exhaustive(),
            Self::Http {
                sse, timeout_ms, ..
            } => formatter
                .debug_struct("Http")
                .field("sse", sse)
                .field("timeout_ms", timeout_ms)
                .finish_non_exhaustive(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SetupFailure {
    #[error(transparent)]
    Failed(#[from] io::Error),
    #[error("{0}")]
    RetirementUnverified(io::Error),
}

impl SetupFailure {
    pub(crate) fn retirement_unverified(&self) -> bool {
        matches!(self, Self::RetirementUnverified(_))
    }
}

impl From<SetupFailure> for io::Error {
    fn from(failure: SetupFailure) -> Self {
        match failure {
            SetupFailure::Failed(error) => error,
            failure @ SetupFailure::RetirementUnverified(_) => io::Error::other(failure),
        }
    }
}

pub(crate) type Completion = Option<Result<(), String>>;

#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    pub line: String,
    pub deadline: Option<crate::request_deadline::SharedDeadline>,
}

impl From<String> for UpstreamRequest {
    fn from(line: String) -> Self {
        Self {
            line,
            deadline: None,
        }
    }
}

#[cfg(test)]
impl From<&str> for UpstreamRequest {
    fn from(line: &str) -> Self {
        line.to_string().into()
    }
}

impl std::ops::Deref for UpstreamRequest {
    type Target = str;

    fn deref(&self) -> &str {
        &self.line
    }
}

pub struct UpstreamHandle {
    pub request_tx: mpsc::Sender<UpstreamRequest>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    completion: watch::Receiver<Completion>,
    log: crate::diagnostics::ServerLog,
}

impl UpstreamHandle {
    pub(crate) fn new(
        request_tx: mpsc::Sender<UpstreamRequest>,
        shutdown_tx: oneshot::Sender<()>,
        completion: watch::Receiver<Completion>,
    ) -> Self {
        Self {
            request_tx,
            shutdown_tx: Some(shutdown_tx),
            completion,
            log: crate::diagnostics::ServerLog::default(),
        }
    }

    pub(crate) fn with_log(mut self, log: crate::diagnostics::ServerLog) -> Self {
        self.log = log;
        self
    }

    pub async fn spawn(
        spec: UpstreamSpec,
        response_tx: mpsc::Sender<String>,
        log: crate::diagnostics::ServerLog,
    ) -> Result<Self, SetupFailure> {
        match spec {
            UpstreamSpec::Stdio {
                command,
                args,
                env,
                cwd,
                clear_env,
            } => {
                crate::upstream_stdio::spawn_configured(
                    command,
                    args,
                    env,
                    cwd,
                    clear_env,
                    response_tx,
                    log,
                )
                .await
            }
            UpstreamSpec::Http {
                url,
                sse,
                headers,
                timeout_ms,
                auth,
            } => crate::upstream_http::spawn_configured(
                url,
                sse,
                headers,
                timeout_ms,
                auth.map(|auth| *auth),
                response_tx,
                log,
            )
            .await
            .map_err(SetupFailure::from),
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
            self.log.log("upstream_shutdown_owner_already_exited");
        }
        self.wait_for_exit().await
    }
}

impl Drop for UpstreamHandle {
    fn drop(&mut self) {
        if let Some(sender) = self.shutdown_tx.take()
            && sender.send(()).is_err()
        {
            self.log.log("upstream_drop_owner_already_exited");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_retirement_is_independent_of_io_error_kind() {
        let ordinary = SetupFailure::from(io::Error::new(io::ErrorKind::ResourceBusy, "ordinary"));
        assert!(!ordinary.retirement_unverified());
        let unverified = SetupFailure::RetirementUnverified(io::Error::other("unconfirmed"));
        assert!(unverified.retirement_unverified());
        assert_eq!(unverified.to_string(), "unconfirmed");
    }

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
