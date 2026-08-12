use anyhow::{Result, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};

use super::signing::sign_base64;

pub const WEEX_PRIVATE_WS_URL: &str = "wss://ws-contract.weex.com/v3/ws/private";
const WEEX_PRIVATE_WS_PATH: &str = "/v3/ws/private";

#[derive(Debug, Clone)]
pub struct WeexPrivateSubscription {
    api_key: String,
    api_secret: String,
    passphrase: String,
}

impl WeexPrivateSubscription {
    pub fn new(
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        passphrase: impl Into<String>,
    ) -> Result<Self> {
        let result = Self {
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            passphrase: passphrase.into(),
        };
        if result.api_key.trim().is_empty()
            || result.api_secret.trim().is_empty()
            || result.passphrase.trim().is_empty()
        {
            bail!("WEEX private websocket credentials and passphrase must be non-empty");
        }
        Ok(result)
    }

    /// Headers must be attached to the websocket HTTP upgrade request.
    pub fn handshake_headers(&self, timestamp_ms: u64) -> Result<HeaderMap> {
        let timestamp = timestamp_ms.to_string();
        let signature = sign_base64(
            &self.api_secret,
            &format!("{timestamp}{WEEX_PRIVATE_WS_PATH}"),
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("rust-crypto-mm/weex-v3"),
        );
        insert_header(&mut headers, "access-key", &self.api_key)?;
        insert_header(&mut headers, "access-passphrase", &self.passphrase)?;
        insert_header(&mut headers, "access-timestamp", &timestamp)?;
        insert_header(&mut headers, "access-sign", &signature)?;
        Ok(headers)
    }

    pub fn subscribe_message(&self, id: u64) -> String {
        serde_json::json!({
            "method": "SUBSCRIBE",
            "params": ["account", "positions", "orders", "fill"],
            "id": id,
        })
        .to_string()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WeexPrivateEvent {
    Subscribed {
        id: Option<u64>,
    },
    Orders {
        version: u64,
        data: serde_json::Value,
    },
    Positions {
        version: u64,
        data: serde_json::Value,
    },
    Account {
        version: u64,
        data: serde_json::Value,
    },
    Fills {
        version: u64,
        data: serde_json::Value,
    },
}

pub fn parse_private_message(text: &str) -> Result<WeexPrivateEvent> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|err| anyhow::anyhow!("invalid WEEX private websocket JSON: {err}"))?;
    if let Some(result) = value.get("result").and_then(|value| value.as_bool()) {
        if !result {
            let msg = value
                .get("msg")
                .and_then(|value| value.as_str())
                .unwrap_or("<missing msg>");
            bail!("WEEX private websocket subscription rejected: {msg}");
        }
        return Ok(WeexPrivateEvent::Subscribed {
            id: value.get("id").and_then(|value| value.as_u64()),
        });
    }
    let event = value
        .get("e")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("WEEX private websocket update missing event field 'e'"))?;
    let version = value
        .get("v")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| anyhow::anyhow!("WEEX private websocket event={event} missing version v"))?;
    let data = value
        .get("d")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("WEEX private websocket event={event} missing data d"))?;
    match event {
        "orders" => Ok(WeexPrivateEvent::Orders { version, data }),
        "positions" => Ok(WeexPrivateEvent::Positions { version, data }),
        "account" => Ok(WeexPrivateEvent::Account { version, data }),
        "fill" => Ok(WeexPrivateEvent::Fills { version, data }),
        _ => bail!("unexpected WEEX private websocket event '{event}'"),
    }
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<()> {
    headers.insert(
        HeaderName::from_static(name),
        HeaderValue::from_str(value)
            .map_err(|err| anyhow::anyhow!("invalid value for WEEX header {name}: {err}"))?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_all_required_handshake_headers() {
        let subscription =
            WeexPrivateSubscription::new("key", "secret", "pass").expect("valid credentials");
        let headers = subscription
            .handshake_headers(1_659_076_670_000)
            .expect("valid headers");
        for name in [
            "access-key",
            "access-passphrase",
            "access-timestamp",
            "access-sign",
        ] {
            assert!(headers.contains_key(name), "missing {name}");
        }
    }
}
