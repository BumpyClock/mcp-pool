use super::{ControlRequest, OutputMode};

pub(super) fn print_response_data(
    request: &ControlRequest,
    data: Option<serde_json::Value>,
    mode: OutputMode,
    color: bool,
) {
    if mode == OutputMode::Json {
        let text = data
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or_else(|| "{}".to_string());
        println!("{text}");
        return;
    }
    let (green, yellow, _red, reset) = colors(color);

    match request {
        ControlRequest::Start { name } => println!("{green}started{reset} {name}"),
        ControlRequest::StartAll => print_start_all(data.as_ref(), color),
        ControlRequest::Stop { name } => println!("{yellow}stopped{reset} {name}"),
        ControlRequest::Restart { name } => println!("{green}restarted{reset} {name}"),
        ControlRequest::Shutdown => println!("{yellow}daemon shutting down{reset}"),
        ControlRequest::Status { name } => {
            print_status_table(data.as_ref(), name.as_deref(), color)
        }
    }
}

fn print_start_all(data: Option<&serde_json::Value>, color: bool) {
    let (green, _yellow, red, reset) = colors(color);
    let servers = data
        .and_then(|value| value.get("servers"))
        .and_then(|value| value.as_array())
        .map(|array| array.as_slice())
        .unwrap_or(&[]);

    if servers.is_empty() {
        println!("no servers configured");
        return;
    }

    for server in servers {
        let name = str_field(server, "name");
        let ok = server
            .get("ok")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if ok {
            println!("{green}started{reset} {name}");
        } else {
            let error = server
                .get("error")
                .and_then(|value| value.as_str())
                .unwrap_or("error");
            println!("{red}failed{reset} {name}: {error}");
        }
    }
}

fn colors(color: bool) -> (&'static str, &'static str, &'static str, &'static str) {
    if color {
        ("\x1b[32m", "\x1b[33m", "\x1b[31m", "\x1b[0m")
    } else {
        ("", "", "", "")
    }
}

fn str_field<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
    value.get(key).and_then(|v| v.as_str()).unwrap_or("?")
}

fn print_status_table(data: Option<&serde_json::Value>, filter: Option<&str>, color: bool) {
    let (green, yellow, red, reset) = colors(color);
    let Some(data) = data else {
        eprintln!("mcp-pool: status response omitted data");
        std::process::exit(1);
    };
    let status: crate::types::PoolStatusResponse = match serde_json::from_value(data.clone()) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("mcp-pool: invalid status response: {error}");
            std::process::exit(1);
        }
    };
    let matches = |server: &crate::types::McpServerStatus| match filter {
        Some(target) => server.name == target,
        None => true,
    };
    let shown: Vec<_> = status
        .servers
        .iter()
        .filter(|server| matches(server))
        .collect();

    if shown.is_empty() {
        match filter {
            Some(name) => println!("server '{name}' is not running"),
            None => println!("no servers running"),
        }
        return;
    }

    println!(
        "{:<20} {:<10} {:<10} {:<10} {:<6} SOCKET",
        "NAME", "STATUS", "READINESS", "TRANSPORT", "CONNS"
    );
    for server in shown {
        use crate::types::ServerStatus;
        let (status, prefix, suffix) = match server.status {
            ServerStatus::Running => ("running", green, reset),
            ServerStatus::Starting => ("starting", yellow, reset),
            ServerStatus::Stopping => ("stopping", yellow, reset),
            ServerStatus::Stopped => ("stopped", red, reset),
            ServerStatus::Failed => ("failed", red, reset),
        };
        let readiness = &server.readiness;
        let ready = if readiness.mcp_initialize_result_received {
            "mcp"
        } else if readiness.upstream_transport_ready {
            "transport"
        } else if readiness.local_socket_bound {
            "socket"
        } else {
            "-"
        };
        println!(
            "{:<20} {prefix}{:<10}{suffix} {:<10} {:<10} {:<6} {}",
            server.name,
            status,
            ready,
            server.transport,
            server.connection_count,
            server.socket_path,
        );
        for (field, error) in [
            ("startup_error", &readiness.startup_error),
            ("retirement_error", &readiness.retirement_error),
        ] {
            if let Some(error) = error {
                println!("  {field}: {error}");
            }
        }
    }
}
