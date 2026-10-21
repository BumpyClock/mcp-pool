use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn deliver(
    body: &str,
    request: &Request,
    session_identifier: Option<HeaderValue>,
    session: &Arc<Mutex<Session>>,
    response_tx: &mpsc::Sender<String>,
    answered: &mut bool,
    established: &mut Option<oneshot::Sender<Option<Request>>>,
) -> Result<(), String> {
    let value = parse_message(body)?;
    let matches = request.identifier.as_ref().is_some_and(|identifier| {
        value.get("id") == Some(identifier)
            && (value.get("result").is_some() || value.get("error").is_some())
    });
    if matches && request.initialize && value.get("result").is_some() {
        if session_identifier.as_ref().is_some_and(|identifier| {
            identifier.is_empty()
                || !identifier
                    .as_bytes()
                    .iter()
                    .all(|byte| (0x21..=0x7e).contains(byte))
        }) {
            return Err("MCP initialize response contains an invalid session ID".into());
        }
        let protocol = value
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
            .ok_or("MCP initialize response omitted protocolVersion")?;
        if protocol.is_empty() {
            return Err("MCP initialize response contains an invalid protocolVersion".into());
        }
        let protocol = HeaderValue::from_str(protocol)
            .map_err(|_| "MCP initialize response contains an invalid protocolVersion")?;
        *session.lock().await = Session::Ready {
            identifier: session_identifier,
            protocol,
        };
    } else if matches
        && request.discover
        && let Some(result) = value.get("result")
        && crate::mcp_protocol::discovery_offers_modern(result)
            .map_err(|_| "MCP server/discover returned invalid supportedVersions")?
    {
        *session.lock().await = Session::Ready {
            identifier: None,
            protocol: HeaderValue::from_static(crate::mcp_protocol::MODERN_VERSION),
        };
    }
    response_tx
        .send(value.to_string())
        .await
        .map_err(|_| "HTTP response receiver closed")?;
    *answered |= matches;
    if matches {
        release_initialization(established);
    }
    Ok(())
}
