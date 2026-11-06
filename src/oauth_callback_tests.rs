use anyhow::{Result, bail};
use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::flow;

async fn request(address: std::net::SocketAddr, request: String) -> Result<String> {
    let mut stream = TcpStream::connect(address).await?;
    stream.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response)
}

#[tokio::test]
async fn invalid_denial_callbacks_are_ignored_before_a_valid_code() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let redirect = Url::parse(&format!("http://{address}/callback"))?;
    let issuer = "https://issuer.invalid";
    let driver = async {
        let valid = "state=expected&iss=https%3A%2F%2Fissuer.invalid&error=access_denied";
        for (method, path, host, query) in [
            ("POST", "/callback", address.to_string(), valid.to_owned()),
            ("GET", "/unrelated", address.to_string(), valid.to_owned()),
            (
                "GET",
                "/callback",
                "other.invalid".to_owned(),
                valid.to_owned(),
            ),
            (
                "GET",
                "/callback",
                address.to_string(),
                format!("{valid}&state=wrong"),
            ),
            (
                "GET",
                "/callback",
                address.to_string(),
                "state=expected&error=access_denied&iss=https%3A%2F%2Fother.invalid".to_owned(),
            ),
            (
                "GET",
                "/callback",
                address.to_string(),
                "state=expected&error=access_denied".to_owned(),
            ),
            (
                "GET",
                "/callback",
                address.to_string(),
                format!("{valid}&error=duplicate"),
            ),
            (
                "GET",
                "/callback",
                address.to_string(),
                format!("{valid}&code=one&code=two"),
            ),
        ] {
            let response = request(
                address,
                format!("{method} {path}?{query} HTTP/1.1\r\nHost: {host}\r\n\r\n"),
            )
            .await?;
            assert!(response.starts_with("HTTP/1.1 400"));
        }
        let response = request(address, format!("GET /callback?state=expected&code=accepted&iss=https%3A%2F%2Fissuer.invalid HTTP/1.1\r\nHost: {address}\r\n\r\n")).await?;
        assert!(response.starts_with("HTTP/1.1 200"));
        Ok::<(), anyhow::Error>(())
    };
    let (code, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::try_join!(
            flow::callback(&listener, &redirect, "expected", issuer, true),
            driver
        )
    })
    .await??;
    assert_eq!(code, "accepted");
    Ok(())
}

#[tokio::test]
async fn validated_denial_is_terminal() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let redirect = Url::parse(&format!("http://{address}/callback"))?;
    let driver = request(
        address,
        format!(
            "GET /callback?state=expected&error=access_denied&iss=https%3A%2F%2Fissuer.invalid HTTP/1.1\r\nHost: {address}\r\n\r\n"
        ),
    );
    let (result, response) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            flow::callback(
                &listener,
                &redirect,
                "expected",
                "https://issuer.invalid",
                true
            ),
            driver
        )
    })
    .await?;
    assert!(response?.starts_with("HTTP/1.1 400"));
    let Err(error) = result else {
        bail!("validated consent denial must terminate authorization");
    };
    assert_eq!(
        error.to_string(),
        "OAuth consent denied by authorization server"
    );
    Ok(())
}
