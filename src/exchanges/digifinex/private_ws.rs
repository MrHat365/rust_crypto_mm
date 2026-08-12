use serde::{Deserialize, Serialize};

use crate::utils::parsing::log_parse_drop;

use super::signing::hmac_sha256_base64;

pub const DIGIFINEX_PRIVATE_WS_URL: &str = "wss://openapi.digifinex.com/swap_ws/v2/";

#[derive(Debug, Clone)]
pub struct DigiFinexPrivateSubscription {
    api_key: String,
    api_secret: String,
    instrument_id: String,
}

impl DigiFinexPrivateSubscription {
    pub fn new(
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        instrument_id: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let api_key = api_key.into();
        let api_secret = api_secret.into();
        let instrument_id = instrument_id.into();
        if api_key.trim().is_empty() || api_secret.trim().is_empty() {
            anyhow::bail!("DigiFinex private websocket credentials must be non-empty");
        }
        if instrument_id.trim().is_empty() {
            anyhow::bail!("DigiFinex private websocket instrument_id must be non-empty");
        }
        Ok(Self {
            api_key,
            api_secret,
            instrument_id,
        })
    }

    /// Send this first and wait for a successful `server.auth` response.
    pub fn auth_message(&self, timestamp_ms: u64, id: u64) -> String {
        let timestamp = timestamp_ms.to_string();
        let signature = hmac_sha256_base64(&self.api_secret, &timestamp);
        serde_json::json!({
            "event": "server.auth",
            "id": id,
            "apikey": self.api_key,
            "timestamp": timestamp_ms,
            "signature": signature,
        })
        .to_string()
    }

    /// These messages are valid only after `server.auth` returns code=1.
    pub fn channel_messages(&self, first_id: u64) -> [String; 3] {
        [
            serde_json::json!({
                "event": "account.subscribe",
                "id": first_id,
            })
            .to_string(),
            serde_json::json!({
                "event": "position.subscribe",
                "id": first_id.saturating_add(1),
                "instrument_id": self.instrument_id,
            })
            .to_string(),
            serde_json::json!({
                "event": "order.subscribe",
                "id": first_id.saturating_add(2),
                "instrument_id": self.instrument_id,
            })
            .to_string(),
        ]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DigiFinexPrivateEvent {
    Authenticated,
    Account(Vec<AccountUpdate>),
    Positions(Vec<PositionUpdate>),
    Orders(Vec<OrderUpdate>),
    Subscribed { event: String, id: u64 },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct AccountUpdate {
    pub currency: String,
    pub equity: String,
    pub avail_balance: String,
    pub margin: String,
    pub frozen_margin: String,
    pub realized_pnl: String,
    pub unrealized_pnl: String,
    pub time_stamp: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PositionUpdate {
    pub instrument_id: String,
    pub side: String,
    pub position: String,
    pub avail_position: String,
    pub leverage: String,
    pub margin_mode: String,
    pub avg_cost: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct OrderUpdate {
    pub order_id: String,
    pub instrument_id: String,
    pub state: i64,
    pub price: String,
    pub size: String,
    pub filled_qty: String,
    pub price_avg: String,
    pub time_stamp: u64,
}

pub fn parse_private_message(text: &str) -> anyhow::Result<DigiFinexPrivateEvent> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|err| {
        log_parse_drop("digifinex_private_ws", "json", &err, text);
        anyhow::anyhow!("invalid DigiFinex private websocket JSON: {err}")
    })?;
    let event = value
        .get("event")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("DigiFinex private message missing string event"))?;

    if let Some(code) = value.get("code").and_then(|value| value.as_i64()) {
        if code != 1 {
            let msg = value
                .get("msg")
                .and_then(|value| value.as_str())
                .unwrap_or("<missing msg>");
            anyhow::bail!("DigiFinex private websocket event={event} code={code} msg={msg}");
        }
        if event == "server.auth" {
            return Ok(DigiFinexPrivateEvent::Authenticated);
        }
        let id = value
            .get("id")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| anyhow::anyhow!("successful subscription ack missing id"))?;
        return Ok(DigiFinexPrivateEvent::Subscribed {
            event: event.to_string(),
            id,
        });
    }

    let data = value
        .get("data")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("DigiFinex private update event={event} missing data"))?;
    match event {
        "account.update" => Ok(DigiFinexPrivateEvent::Account(
            serde_json::from_value(data)
                .map_err(|err| anyhow::anyhow!("invalid account.update schema: {err}"))?,
        )),
        "position.update" => Ok(DigiFinexPrivateEvent::Positions(
            serde_json::from_value(data)
                .map_err(|err| anyhow::anyhow!("invalid position.update schema: {err}"))?,
        )),
        "order.update" => Ok(DigiFinexPrivateEvent::Orders(
            serde_json::from_value(data)
                .map_err(|err| anyhow::anyhow!("invalid order.update schema: {err}"))?,
        )),
        _ => anyhow::bail!("unexpected DigiFinex private websocket event '{event}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_message_uses_timestamp_hmac() {
        let sub = DigiFinexPrivateSubscription::new("key", "secret", "BTCUSDTPERP")
            .expect("valid subscription");
        let value: serde_json::Value =
            serde_json::from_str(&sub.auth_message(1_662_346_006_093, 1)).expect("valid JSON");
        assert_eq!(value["event"], "server.auth");
        assert_eq!(value["timestamp"], 1_662_346_006_093_u64);
        assert!(value["signature"].as_str().is_some_and(|v| !v.is_empty()));
    }

    #[test]
    fn rejects_failed_auth() {
        let err =
            parse_private_message(r#"{"event":"server.auth","id":1,"code":3,"msg":"auth fail"}"#)
                .expect_err("auth failure must be returned");
        assert!(err.to_string().contains("code=3"));
    }
}
