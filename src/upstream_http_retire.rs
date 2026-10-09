use super::*;

#[cfg(not(test))]
pub(super) const DELETE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
pub(super) const DELETE_TIMEOUT: Duration = Duration::from_millis(100);

pub(super) async fn terminate_session(
    client: &reqwest::Client,
    url: &reqwest::Url,
    session: &Arc<Mutex<Session>>,
    options: &Options,
) {
    let previous = std::mem::replace(&mut *session.lock().await, Session::Expired);
    let Session::Ready {
        identifier: Some(identifier),
        protocol,
    } = previous
    else {
        return;
    };
    let request = async {
        options
            .authorize(client.delete(url.clone()))
            .await
            .map_err(|_| ())?
            .headers(HeaderMap::from_iter([(
                ACCEPT,
                HeaderValue::from_static("application/json, text/event-stream"),
            )]))
            .header("Mcp-Session-Id", identifier)
            .header("MCP-Protocol-Version", protocol)
            .send()
            .await
            .map_err(|_| ())
    };
    match timeout(DELETE_TIMEOUT, request).await {
        Ok(Ok(response)) if response.status().is_success() => {}
        Ok(Ok(response)) if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED => {
            crate::diagnostics::log("upstream_http_session_delete_unsupported");
        }
        _ => {
            // Remote cleanup cannot establish or invalidate verified local task retirement.
            crate::diagnostics::log("upstream_http_session_delete_failed");
        }
    }
}
