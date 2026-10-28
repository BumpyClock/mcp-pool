use super::*;
use crate::request_deadline::SharedDeadline;
use crate::request_deadline::{TIMEOUT_FIELD, timeout_ms};

pub(super) struct Request {
    pub(super) line: String,
    pub(super) identifier: Option<Value>,
    pub(super) initialize: bool,
    pub(super) discover: bool,
    pub(super) modern: bool,
    pub(super) method: Option<String>,
    pub(super) name: Option<String>,
    pub(super) initialization_barrier: bool,
    timeout_ms: Option<u64>,
    shared: bool,
    pub(super) budget: SharedDeadline,
}

impl Request {
    pub(super) fn parse(
        envelope: crate::upstream::UpstreamRequest,
        options: &Options,
    ) -> Result<Self, String> {
        let line = envelope.line;
        if line.len() > sse_parser::FRAME_LIMIT {
            return Err("HTTP request exceeds size limit".into());
        }
        let mut value: Value =
            serde_json::from_str(&line).map_err(|_| "HTTP request is not valid JSON")?;
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
        let initialize = method == Some("initialize");
        let discover = method == Some("server/discover");
        let modern = value
            .get("params")
            .is_some_and(crate::mcp_protocol::is_modern);
        let initialization_barrier =
            initialize || discover || method == Some("notifications/initialized");
        let method = method.map(str::to_owned);
        let name = value
            .pointer("/params/name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let line = if timeout_ms.is_some() {
            value.to_string()
        } else {
            line
        };
        let budget = match envelope.deadline {
            Some(budget) => budget,
            None => {
                let deadline = std::time::Instant::now()
                    .checked_add(Self::deadline(timeout_ms, shared, options))
                    .ok_or("HTTP request deadline exceeds the clock range")?;
                SharedDeadline::new(deadline)
            }
        };
        Ok(Self {
            line,
            identifier,
            initialize,
            discover,
            modern,
            method,
            name,
            initialization_barrier,
            timeout_ms,
            shared,
            budget,
        })
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
    ) -> Result<Option<Vec<u8>>, String> {
        if self.shared {
            read_chunk_with_deadline(response, &self.budget).await
        } else {
            read_chunk_with_timeout(response, self.read_deadline(options)).await
        }
    }

    pub(super) fn forwarded(&self) -> Result<crate::upstream::UpstreamRequest, String> {
        let mut value: Value =
            serde_json::from_str(&self.line).map_err(|_| "Pool fallback request is invalid")?;
        if let Some(milliseconds) = self.timeout_ms {
            value
                .as_object_mut()
                .ok_or("Pool fallback request must be an object")?
                .insert(TIMEOUT_FIELD.into(), Value::from(milliseconds));
        }
        Ok(crate::upstream::UpstreamRequest {
            line: value.to_string(),
            deadline: Some(self.budget.clone()),
        })
    }
}
