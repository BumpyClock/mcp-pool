use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use base64::Engine;
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{context, files, store};
use crate::config::ServerDef;
use crate::server_config::ConfiguredServer;

pub(super) struct Fixture {
    pub root: PathBuf,
    pub server: ConfiguredServer,
}

impl Fixture {
    pub fn new(url: &str) -> Result<Self> {
        let root = std::env::current_dir()?
            .join("target")
            .join("oauth-fixtures")
            .join(oauth2::CsrfToken::new_random().secret());
        std::fs::create_dir_all(&root)?;
        let mut environment = BTreeMap::new();
        environment.insert(
            "HOME".to_owned(),
            root.join("home").to_string_lossy().into_owned(),
        );
        environment.insert(
            "USERPROFILE".to_owned(),
            root.join("home").to_string_lossy().into_owned(),
        );
        environment.insert(
            "XDG_DATA_HOME".to_owned(),
            root.join("data").to_string_lossy().into_owned(),
        );
        let definition = ServerDef {
            url: url.to_owned(),
            env: environment,
            ..ServerDef::default()
        };
        let server = ConfiguredServer {
            name: "synthetic".to_owned(),
            definition,
            raw: json!({"auth":"oauth"}),
            source: root.join("config.json"),
        };
        Ok(Self { root, server })
    }

    pub fn seed(&self, patch: Value) -> Result<()> {
        let authentication = context(&self.server)?;
        let _locks = store::transaction(&authentication)?;
        store::save(&authentication, &patch, None)
    }

    pub fn valid(&self) -> Result<()> {
        self.seed(json!({"tokens":{"access_token":"synthetic-current","token_type":"Bearer","expires_at":files::now()+3600}}))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.root)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("could not clean synthetic OAuth fixture");
        }
    }
}

pub(super) struct Mock {
    pub origin: String,
    pub token_requests: Arc<AtomicUsize>,
    pub registrations: Arc<AtomicUsize>,
    pub authorization: Arc<Mutex<Option<Url>>>,
    pub forms: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Mock {
    pub async fn new(reject_refresh: bool) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let token_requests = Arc::new(AtomicUsize::new(0));
        let registrations = Arc::new(AtomicUsize::new(0));
        let authorization = Arc::new(Mutex::new(None::<Url>));
        let forms = Arc::new(Mutex::new(Vec::new()));
        let origin_copy = origin.clone();
        let count = token_requests.clone();
        let registered = registrations.clone();
        let authorization_copy = authorization.clone();
        let form_copy = forms.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let origin = origin_copy.clone();
                let count = count.clone();
                let registered = registered.clone();
                let authorization = authorization_copy.clone();
                let forms = form_copy.clone();
                tokio::spawn(async move {
                    let result: Result<()> = async {
                        let mut bytes = Vec::new();
                        let mut buffer = [0_u8; 2048];
                        let boundary = loop {
                            let read = stream.read(&mut buffer).await?;
                            if read == 0 { return Ok(()); }
                            bytes.extend_from_slice(buffer.get(..read).ok_or_else(|| anyhow::anyhow!("mock read invalid"))?);
                            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                                break position + 4;
                            }
                        };
                        let header = String::from_utf8_lossy(bytes.get(..boundary).ok_or_else(|| anyhow::anyhow!("mock header invalid"))?).into_owned();
                        let length = header.lines().filter_map(|line| line.split_once(':'))
                            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok()).unwrap_or_default();
                        while bytes.len() < boundary + length {
                            let read = stream.read(&mut buffer).await?;
                            if read == 0 { break; }
                            bytes.extend_from_slice(buffer.get(..read).ok_or_else(|| anyhow::anyhow!("mock read invalid"))?);
                        }
                        let path = header.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or_default();
                        let body = bytes.get(boundary..).unwrap_or_default();
                        let mut status = 200;
                        let response = match path {
                            "/.well-known/oauth-protected-resource/mcp" => json!({
                                "resource":format!("{origin}/mcp"),"authorization_servers":[origin],"scopes_supported":["synthetic:tools"]
                            }),
                            "/.well-known/oauth-authorization-server" => metadata(&origin),
                            "/register" => {
                                registered.fetch_add(1, Ordering::SeqCst);
                                let mut value: Value = serde_json::from_slice(body)?;
                                if let Some(object) = value.as_object_mut() { object.insert("client_id".to_owned(), json!("synthetic-client")); }
                                value
                            }
                            "/token" => {
                                count.fetch_add(1, Ordering::SeqCst);
                                let text = std::str::from_utf8(body)?;
                                let form: BTreeMap<String, String> = Url::parse(&format!("http://127.0.0.1/?{text}"))?
                                    .query_pairs().map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
                                let mut accepted = form.get("client_id").is_some_and(|value| value == "synthetic-client")
                                    && form.get("resource").is_some_and(|value| value == &format!("{origin}/mcp"));
                                if form.get("grant_type").is_some_and(|value| value == "authorization_code") {
                                    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                                    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
                                    let saved = authorization.lock().map_err(|_| anyhow::anyhow!("mock authorization lock poisoned"))?;
                                    accepted &= saved.as_ref().is_some_and(|url| url.query_pairs().any(|(key, value)| key == "code_challenge" && value == challenge));
                                } else { accepted &= !reject_refresh; }
                                forms.lock().map_err(|_| anyhow::anyhow!("mock form lock poisoned"))?.push(form);
                                if !accepted {
                                    status = 400;
                                    json!({"error":"invalid_grant"})
                                } else {
                                    json!({"access_token":"synthetic-new","refresh_token":"synthetic-rotated","token_type":"Bearer","expires_in":3600})
                                }
                            }
                            _ => { status = 404; json!({"error":"not_found"}) }
                        };
                        let encoded = serde_json::to_vec(&response)?;
                        let header = format!("HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", encoded.len());
                        stream.write_all(header.as_bytes()).await?;
                        stream.write_all(&encoded).await?;
                        stream.shutdown().await?;
                        Ok(())
                    }.await;
                    if result.is_err() {
                        eprintln!("synthetic OAuth mock request failed");
                    }
                });
            }
        });
        Ok(Self {
            origin,
            token_requests,
            registrations,
            authorization,
            forms,
            task,
        })
    }

    pub fn discovery(&self) -> Value {
        json!({
            "authorizationServerUrl":self.origin,
            "resourceMetadata":{"resource":format!("{}/mcp",self.origin),"authorization_servers":[self.origin]},
            "authorizationServerMetadata":metadata(&self.origin)
        })
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn metadata(origin: &str) -> Value {
    json!({
        "issuer":origin,"authorization_endpoint":format!("{origin}/authorize"),
        "token_endpoint":format!("{origin}/token"),"registration_endpoint":format!("{origin}/register"),
        "code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"],
        "authorization_response_iss_parameter_supported":true
    })
}
