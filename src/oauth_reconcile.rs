use anyhow::{Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::string;

const COMMIT: &str = "__mcp_pool_commit";

fn fingerprint(value: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn token_identity(tokens: &Value) -> Result<String> {
    let mut value = tokens.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove(COMMIT);
    }
    fingerprint(&value)
}

fn revision(tokens: &Value) -> u64 {
    tokens
        .get(COMMIT)
        .and_then(|commit| commit.get("revision"))
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

fn parent(tokens: &Value) -> Option<&str> {
    tokens
        .get(COMMIT)
        .and_then(|commit| string(commit, "parent"))
}

pub(super) fn stamp(entry: &mut Value, previous: &Value, candidates: &[Value]) -> Result<()> {
    let revision = candidates
        .iter()
        .filter_map(|snapshot| snapshot.get("tokens"))
        .map(revision)
        .max()
        .unwrap_or_default()
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("credential commit sequence exhausted"))?;
    let predecessor = previous.get("tokens").map(token_identity).transpose()?;
    let client = entry
        .get("clientInfo")
        .filter(|value| !value.is_null())
        .map(fingerprint)
        .transpose()?;
    let tokens = entry
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("credential tokens must be an object"))?;
    tokens.insert(
        COMMIT.to_owned(),
        json!({"revision":revision,"parent":predecessor,"client":client}),
    );
    Ok(())
}

pub(super) fn validate_client(snapshot: &Value) -> Result<()> {
    let expected = snapshot
        .get("tokens")
        .and_then(|tokens| tokens.get(COMMIT))
        .and_then(|commit| string(commit, "client"));
    if let Some(expected) = expected {
        let client = snapshot
            .get("clientInfo")
            .filter(|value| !value.is_null())
            .ok_or_else(|| {
                anyhow::anyhow!("credential generation is missing its bound client registration")
            })?;
        if fingerprint(client)? != expected {
            bail!(
                "credential generation does not match stored client registration; explicitly reauthorize"
            );
        }
    }
    Ok(())
}

pub(super) fn select(candidates: &[Value]) -> Result<Value> {
    let mut selected = candidates
        .iter()
        .find(|candidate| candidate.get("tokens").is_some())
        .or_else(|| {
            candidates
                .iter()
                .find(|candidate| candidate.get("clientInfo").is_some())
        })
        .cloned()
        .unwrap_or_else(|| json!({}));
    for candidate in candidates {
        let (Some(current), Some(proposed)) = (selected.get("tokens"), candidate.get("tokens"))
        else {
            continue;
        };
        let current_identity = token_identity(current)?;
        let proposed_identity = token_identity(proposed)?;
        if current_identity == proposed_identity {
            continue;
        }
        let current_revision = revision(current);
        let proposed_revision = revision(proposed);
        if current_revision > 0 && proposed_revision > 0 {
            if current_revision == proposed_revision {
                bail!("credential stores contain conflicting committed generations");
            }
            if proposed_revision > current_revision {
                selected = candidate.clone();
            }
            continue;
        }
        if parent(proposed) == Some(current_identity.as_str()) {
            selected = candidate.clone();
            continue;
        }
        if parent(current) == Some(proposed_identity.as_str()) {
            continue;
        }
        let different_refresh = string(current, "refresh_token").is_some()
            && string(proposed, "refresh_token").is_some()
            && string(current, "refresh_token") != string(proposed, "refresh_token");
        if different_refresh {
            bail!(
                "credential stores disagree without verified commit ordering; explicitly reauthorize instead of replaying a refresh token"
            );
        }
    }
    if let Some(tokens) = selected.get("tokens") {
        let identity = token_identity(tokens)?;
        for candidate in candidates {
            if candidate
                .get("tokens")
                .map(token_identity)
                .transpose()?
                .as_deref()
                != Some(identity.as_str())
            {
                continue;
            }
            if let (Some(selected_client), Some(candidate_client)) =
                (selected.get("clientInfo"), candidate.get("clientInfo"))
                && selected_client != candidate_client
            {
                bail!("one credential generation has conflicting client registrations");
            }
            if let Some(object) = selected.as_object_mut() {
                for field in [
                    "clientInfo",
                    "discoveryState",
                    "authorizationServerUrl",
                    "resourceUrl",
                ] {
                    if !object.contains_key(field)
                        && let Some(value) = candidate.get(field)
                    {
                        object.insert(field.to_owned(), value.clone());
                    }
                }
            }
        }
    }
    validate_client(&selected)?;
    Ok(selected)
}
