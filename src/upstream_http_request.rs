use super::*;
use crate::request_deadline::SharedDeadline;
use crate::request_deadline::{TIMEOUT_FIELD, timeout_ms};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ExchangeKind {
    Initialize(crate::mcp_protocol::Era),
    Discovery(crate::mcp_protocol::Era),
    Initialized(crate::mcp_protocol::Era),
    Operation(crate::mcp_protocol::Era),
}

impl ExchangeKind {
    pub(super) fn is_modern(self) -> bool {
        let (Self::Initialize(era)
        | Self::Discovery(era)
        | Self::Initialized(era)
        | Self::Operation(era)) = self;
        era == crate::mcp_protocol::Era::Modern
    }

    pub(super) fn is_initialize(self) -> bool {
        matches!(self, Self::Initialize(_))
    }

    pub(super) fn is_discovery(self) -> bool {
        matches!(self, Self::Discovery(_))
    }
    pub(super) fn is_handshake(self) -> bool {
        matches!(self, Self::Initialize(_) | Self::Discovery(_))
    }
    pub(super) fn is_barrier(self) -> bool {
        self.is_handshake() || matches!(self, Self::Initialized(_))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ExchangeMode {
    InitialLegacy,
    Standard,
}

#[derive(Default)]
pub(super) enum Negotiation {
    #[default]
    Probing,
    Active,
}

impl Negotiation {
    pub(super) fn admit(&mut self, kind: ExchangeKind, quiescent: bool) -> ExchangeMode {
        let mode = if matches!(self, Self::Probing) && quiescent && kind.is_initialize() {
            ExchangeMode::InitialLegacy
        } else {
            ExchangeMode::Standard
        };
        if !kind.is_discovery() {
            *self = Self::Active;
        }
        mode
    }
}

pub(super) struct Request {
    pub(super) line: String,
    pub(super) identifier: Option<Value>,
    pub(super) kind: ExchangeKind,
    pub(super) method: Option<String>,
    pub(super) name: Option<String>,
    timeout_ms: Option<u64>,
    shared: bool,
    pub(super) budget: SharedDeadline,
}

impl Request {
    pub(super) fn parse(
        mut envelope: crate::upstream::UpstreamRequest,
        options: &Options,
    ) -> Result<Self, (crate::upstream::UpstreamRequest, HttpFailure)> {
        let parsed = (|| {
            let line = &envelope.line;
            if line.len() > MAX_REQUEST_BYTES {
                return Err("HTTP request exceeds size limit".into());
            }
            let mut value: Value =
                serde_json::from_str(line).map_err(|_| "HTTP request is not valid JSON")?;
            if !value.is_object() {
                return Err("HTTP transport requires a JSON-RPC object".into());
            }
            let timeout_ms = timeout_ms(&value)?;
            value
                .as_object_mut()
                .ok_or("HTTP transport requires a JSON-RPC object")?
                .remove(TIMEOUT_FIELD);
            let shared = crate::mcp_session::cacheable_request(&value).is_some();
            let method = value.get("method").and_then(Value::as_str);
            let identifier = method.and_then(|_| {
                value
                    .get("id")
                    .filter(|identifier| !identifier.is_null())
                    .cloned()
            });
            let era = if value
                .get("params")
                .is_some_and(crate::mcp_protocol::is_modern)
            {
                crate::mcp_protocol::Era::Modern
            } else {
                crate::mcp_protocol::Era::Legacy
            };
            let kind = match method {
                Some("initialize") => ExchangeKind::Initialize(era),
                Some("server/discover") => ExchangeKind::Discovery(era),
                Some("notifications/initialized") => ExchangeKind::Initialized(era),
                _ => ExchangeKind::Operation(era),
            };
            let method = method.map(str::to_owned);
            let name = value
                .pointer("/params/name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let budget = match envelope.deadline.take() {
                Some(budget) => budget,
                None => {
                    let deadline = std::time::Instant::now()
                        .checked_add(Self::deadline(timeout_ms, shared, options))
                        .ok_or("HTTP request deadline exceeds the clock range")?;
                    SharedDeadline::new(deadline)
                }
            };
            let line = if timeout_ms.is_some() {
                value.to_string()
            } else {
                std::mem::take(&mut envelope.line)
            };
            Ok(Self {
                line,
                identifier,
                kind,
                method,
                name,
                timeout_ms,
                shared,
                budget,
            })
        })();
        parsed.map_err(|error| (envelope, error))
    }

    fn deadline(timeout_ms: Option<u64>, shared: bool, options: &Options) -> Duration {
        match timeout_ms {
            Some(milliseconds) => {
                let duration = Duration::from_millis(milliseconds);
                if shared {
                    duration.max(options.request_timeout)
                } else {
                    duration
                }
            }
            None => options.request_timeout,
        }
    }

    pub(super) fn read_deadline(&self, options: &Options) -> Duration {
        if self.timeout_ms.is_some() {
            Self::deadline(self.timeout_ms, self.shared, options)
        } else {
            options.read_timeout
        }
    }

    pub(super) async fn read_chunk(
        &self,
        response: &mut reqwest::Response,
        options: &Options,
    ) -> Result<Option<Vec<u8>>, HttpFailure> {
        if self.shared {
            read_chunk_with_deadline(response, &self.budget).await
        } else {
            read_chunk_with_timeout(response, self.read_deadline(options)).await
        }
    }
}
