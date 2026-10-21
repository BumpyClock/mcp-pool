use anyhow::{Context, Result};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::tests::{Server, read, reply, send};
use super::{McpClient, ProtocolPreference};
use crate::mcp_protocol::{self, Era};
use std::time::Duration;

async fn legacy_handshake(server: &mut Server, version: &str) -> Result<()> {
    let initialize = read(server).await?;
    assert_eq!(initialize.get("method"), Some(&json!("initialize")));
    assert_eq!(
        initialize.pointer("/params/protocolVersion"),
        Some(&json!("2025-11-25"))
    );
    reply(
        server,
        &initialize,
        json!({"protocolVersion":version,"capabilities":{"tools":{}}}),
    )
    .await?;
    assert_eq!(
        read(server).await?.get("method"),
        Some(&json!("notifications/initialized"))
    );
    Ok(())
}

#[tokio::test]
async fn legacy_mode_supports_current_and_older_initialize_versions_without_probe() -> Result<()> {
    for version in ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"] {
        let (stream, server) = tokio::io::duplex(4096);
        let mut server = BufReader::new(server);
        let (client, served) = tokio::join!(
            McpClient::negotiate(
                Box::new(stream),
                Duration::from_secs(1),
                ProtocolPreference::Legacy
            ),
            legacy_handshake(&mut server, version)
        );
        served?;
        let client = client?;
        let info = client.connection_info().context("connection metadata")?;
        assert_eq!(info.protocol_version, version);
        assert_eq!(info.era, Era::Legacy);
    }
    Ok(())
}

#[tokio::test]
async fn modern_negotiation_skips_initialize_and_stamps_later_requests() -> Result<()> {
    for preference in [ProtocolPreference::Auto, ProtocolPreference::Modern] {
        let (stream, server) = tokio::io::duplex(4096);
        let mut server = BufReader::new(server);
        let server_work = async {
            let discovery = read(&mut server).await?;
            assert_eq!(discovery.get("method"), Some(&json!("server/discover")));
            assert!(mcp_protocol::is_modern(
                discovery.get("params").context("discovery params")?
            ));
            reply(
                &mut server,
                &discovery,
                json!({
                    "supportedVersions":["2026-07-28"],
                    "capabilities":{"tools":{}},
                    "instructions":"Synthetic modern server."
                }),
            )
            .await?;
            let call = read(&mut server).await?;
            assert_eq!(call.get("method"), Some(&json!("tools/call")));
            assert!(mcp_protocol::is_modern(
                call.get("params").context("call params")?
            ));
            assert_eq!(call.pointer("/params/_meta/trace"), Some(&json!("fixture")));
            reply(
                &mut server,
                &call,
                json!({"resultType":"complete","content":[]}),
            )
            .await
        };
        let client_work = async {
            let mut client =
                McpClient::negotiate(Box::new(stream), Duration::from_secs(1), preference).await?;
            let info = client.connection_info().context("connection metadata")?;
            assert_eq!(info.protocol_version, "2026-07-28");
            assert_eq!(info.era, Era::Modern);
            assert_eq!(
                info.instructions.as_deref(),
                Some("Synthetic modern server.")
            );
            assert_eq!(
                client
                    .request(
                        "tools/call",
                        json!({"name":"echo","_meta":{"trace":"fixture"}})
                    )
                    .await?,
                json!({"resultType":"complete","content":[]})
            );
            Ok::<(), anyhow::Error>(())
        };
        let (result, served) = tokio::join!(client_work, server_work);
        result?;
        served?;
    }
    Ok(())
}

#[tokio::test]
async fn auto_falls_back_on_definitive_discovery_rejection_over_same_stream() -> Result<()> {
    for rejection in [
        json!({"code":-32601,"message":"Unknown method"}),
        json!({"code":-32004,"message":"Unsupported negotiation","data":{"httpStatus":404}}),
    ] {
        let (stream, server) = tokio::io::duplex(4096);
        let mut server = BufReader::new(server);
        let server_work = async {
            let discovery = read(&mut server).await?;
            assert_eq!(discovery.get("method"), Some(&json!("server/discover")));
            send(
                &mut server,
                json!({"jsonrpc":"2.0","id":discovery.get("id"),"error":rejection}),
            )
            .await?;
            legacy_handshake(&mut server, "2025-11-25").await
        };
        let (client, served) = tokio::join!(
            McpClient::negotiate(
                Box::new(stream),
                Duration::from_secs(1),
                ProtocolPreference::Auto
            ),
            server_work
        );
        served?;
        assert_eq!(
            client?.connection_info().map(|info| info.era),
            Some(Era::Legacy)
        );
    }
    Ok(())
}

#[tokio::test]
async fn auto_falls_back_when_discovery_explicitly_offers_only_legacy() -> Result<()> {
    let (stream, server) = tokio::io::duplex(4096);
    let mut server = BufReader::new(server);
    let server_work = async {
        let discovery = read(&mut server).await?;
        reply(
            &mut server,
            &discovery,
            json!({"supportedVersions":["2025-11-25"]}),
        )
        .await?;
        legacy_handshake(&mut server, "2025-11-25").await
    };
    let (result, served) = tokio::join!(
        McpClient::negotiate(
            Box::new(stream),
            Duration::from_secs(1),
            ProtocolPreference::Auto
        ),
        server_work
    );
    served?;
    assert_eq!(
        result?.connection_info().map(|info| info.era),
        Some(Era::Legacy)
    );
    Ok(())
}

#[tokio::test]
async fn pins_auth_errors_and_malformed_successes_never_fall_back_or_call() -> Result<()> {
    for (preference, response) in [
        (
            ProtocolPreference::Modern,
            json!({"error":{"code":-32601,"message":"No discovery"}}),
        ),
        (
            ProtocolPreference::Modern,
            json!({"result":{"supportedVersions":["2025-11-25"]}}),
        ),
        (
            ProtocolPreference::Auto,
            json!({"error":{"code":-32000,"message":"Authorization required"}}),
        ),
        (
            ProtocolPreference::Auto,
            json!({"error":{"code":-32004,"message":"Authorization required","data":{"httpStatus":401}}}),
        ),
        (ProtocolPreference::Auto, json!({"result":{}})),
        (
            ProtocolPreference::Auto,
            json!({"result":{"supportedVersions":["2026-07-28"]}}),
        ),
    ] {
        let (stream, server) = tokio::io::duplex(4096);
        let mut server = BufReader::new(server);
        let server_work = async {
            let discovery = read(&mut server).await?;
            let mut envelope = response;
            let object = envelope
                .as_object_mut()
                .context("response fixture object")?;
            object.insert("jsonrpc".into(), json!("2.0"));
            object.insert("id".into(), discovery.get("id").cloned().context("id")?);
            send(&mut server, envelope).await?;
            let mut unexpected = String::new();
            assert_eq!(server.read_line(&mut unexpected).await?, 0, "{unexpected}");
            Ok::<(), anyhow::Error>(())
        };
        let (result, served) = tokio::join!(
            McpClient::negotiate(Box::new(stream), Duration::from_secs(1), preference),
            server_work
        );
        assert!(result.is_err());
        served?;
    }
    Ok(())
}

#[tokio::test]
async fn unanswered_discovery_drops_only_local_connection_without_replay() -> Result<()> {
    let (stream, server) = tokio::io::duplex(4096);
    let mut server = BufReader::new(server);
    let server_work = async {
        assert_eq!(
            read(&mut server).await?.get("method"),
            Some(&json!("server/discover"))
        );
        let mut unexpected = String::new();
        assert_eq!(server.read_line(&mut unexpected).await?, 0, "{unexpected}");
        Ok::<(), anyhow::Error>(())
    };
    let (result, served) = tokio::join!(
        McpClient::negotiate(
            Box::new(stream),
            Duration::from_millis(25),
            ProtocolPreference::Auto
        ),
        server_work
    );
    assert!(result.is_err());
    served?;
    Ok(())
}
