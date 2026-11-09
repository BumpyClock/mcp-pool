use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

use super::authorization_tests::complete;
use super::test_support::{Fixture, Mock};
use super::{authorization_header, context, store};

#[test]
fn registration_method_selection_respects_metadata_and_configuration() -> Result<()> {
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    let mut authentication = context(&fixture.server)?;
    for (metadata, expected) in [
        (json!({}), Some("client_secret_basic")),
        (
            json!({"token_endpoint_auth_methods_supported":["client_secret_post", "none", "client_secret_basic"]}),
            Some("none"),
        ),
        (
            json!({"token_endpoint_auth_methods_supported":["client_secret_post", "client_secret_basic"]}),
            Some("client_secret_basic"),
        ),
        (
            json!({"token_endpoint_auth_methods_supported":["private_key_jwt"]}),
            None,
        ),
        (json!({"token_endpoint_auth_methods_supported":[]}), None),
    ] {
        assert_eq!(
            super::protocol::registration_auth_method(&authentication, &metadata).ok(),
            expected
        );
    }
    let metadata = json!({"token_endpoint_auth_methods_supported":["none", "client_secret_basic", "client_secret_post", "private_key_jwt"]});
    authentication.token_auth_method = Some("client_secret_post".to_owned());
    assert_eq!(
        super::protocol::registration_auth_method(&authentication, &metadata)?,
        "client_secret_post"
    );
    authentication.token_auth_method = Some("private_key_jwt".to_owned());
    assert!(super::protocol::registration_auth_method(&authentication, &metadata).is_err());
    authentication.token_auth_method = Some("none".to_owned());
    assert!(super::protocol::registration_auth_method(&authentication, &json!({})).is_err());
    Ok(())
}

#[tokio::test]
async fn registration_negotiates_issuer_authentication_and_reuses_it_for_refresh() -> Result<()> {
    for method in ["none", "client_secret_basic", "client_secret_post"] {
        for explicit in [false, true] {
            let mock = Mock::with_auth_method(false, method).await?;
            let mut fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
            if explicit {
                fixture.server.raw = json!({"auth":"oauth", "oauthTokenEndpointAuthMethod":method});
            }
            assert_eq!(
                complete(&fixture, &mock).await?.get("status"),
                Some(&json!("authenticated"))
            );
            let authentication = context(&fixture.server)?;
            let mut snapshot = store::read(&authentication)?;
            assert_eq!(
                snapshot.pointer("/clientInfo/token_endpoint_auth_method"),
                Some(&json!(method))
            );
            assert_eq!(
                snapshot
                    .pointer("/clientInfo/client_secret")
                    .and_then(Value::as_str)
                    .is_some(),
                method != "none"
            );
            *snapshot
                .pointer_mut("/tokens/expires_at")
                .context("token expiration")? = json!(1);
            fixture.seed(json!({"tokens":snapshot.get("tokens").context("stored tokens")?}))?;
            assert_eq!(
                authorization_header(&authentication).await?.as_deref(),
                Some("Bearer synthetic-new")
            );
            assert_eq!(mock.registrations.load(Ordering::SeqCst), 1);
            assert_eq!(mock.token_requests.load(Ordering::SeqCst), 2);
        }
    }
    Ok(())
}
