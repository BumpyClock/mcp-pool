/// Long-lived daemon: binds the control socket, holds the `Pool`, and dispatches
/// control requests from CLI clients.
pub async fn serve() -> anyhow::Result<()> {
    todo!("bind control socket, init Pool, discover_existing_sockets, accept loop, dispatch ControlRequest -> ops")
}
