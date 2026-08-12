use std::time::Instant;

use serde::Deserialize;

use crate::base_classes::types::Ts;
use crate::base_classes::ws::ExchangeHandler;
use crate::utils::parsing::{log_parse_drop, log_parse_drop_bytes};

pub const WEEX_PUBLIC_WS_URL: &str = "wss://ws-contract.weex.com/v3/ws/public";

#[derive(Debug, Clone)]
pub struct WeexFrame {
    pub ts: Ts,
    pub recv_instant: Instant,
    pub event: WeexMarketEvent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WeexMarketEvent {
    Depth(WeexDepthUpdate),
    Trade(serde_json::Value),
    Ticker(serde_json::Value),
    Subscribed { id: Option<u64> },
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WeexDepthUpdate {
    #[serde(rename = "E")]
    pub event_time: u64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "U")]
    pub first_update_id: u64,
    #[serde(rename = "u")]
    pub last_update_id: u64,
    #[serde(rename = "l")]
    pub level: u16,
    #[serde(rename = "d")]
    pub depth_type: String,
    #[serde(rename = "b")]
    pub bids: Vec<(String, String)>,
    #[serde(rename = "a")]
    pub asks: Vec<(String, String)>,
}

pub struct WeexHandler {
    symbol: String,
    subscriptions: Vec<String>,
}

impl WeexHandler {
    pub fn new(symbol: impl AsRef<str>) -> Self {
        let symbol = normalize_symbol(symbol.as_ref());
        let subscriptions = vec![
            serde_json::json!({
                "method": "SUBSCRIBE",
                "params": [
                    format!("{symbol}@depth15"),
                    format!("{symbol}@trade"),
                    format!("{symbol}@ticker")
                ],
                "id": 1
            })
            .to_string(),
        ];
        Self {
            symbol,
            subscriptions,
        }
    }
}

impl ExchangeHandler for WeexHandler {
    type Out = WeexFrame;

    fn url(&self) -> &str {
        WEEX_PUBLIC_WS_URL
    }

    fn initial_subscriptions(&self) -> &[String] {
        &self.subscriptions
    }

    fn connect_headers(&self) -> Vec<(String, String)> {
        vec![(
            "User-Agent".to_string(),
            "rust-crypto-mm/weex-v3".to_string(),
        )]
    }

    fn parse_text(&self, text: &str, ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        parse_payload(text.as_bytes(), ts, recv_instant)
    }

    fn parse_binary(&self, data: &[u8], ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        parse_payload(data, ts, recv_instant)
    }

    fn sequence_key_text(&self, text: &str) -> Option<(u64, u64)> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        if value.get("e").and_then(|value| value.as_str()) != Some("depth") {
            return None;
        }
        let symbol = value.get("s").and_then(|value| value.as_str())?;
        let sequence = value.get("u").and_then(|value| value.as_u64())?;
        Some((fnv1a64(symbol.as_bytes()), sequence))
    }

    fn control_reply_text(&self, text: &str) -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let is_ping = value.get("event").and_then(|value| value.as_str()) == Some("ping")
            || value.get("type").and_then(|value| value.as_str()) == Some("ping");
        is_ping.then(|| r#"{"method":"PONG","id":1}"#.to_string())
    }

    fn label(&self) -> String {
        format!("weex:{}", self.symbol)
    }
}

fn parse_payload(data: &[u8], ts: Ts, recv_instant: Instant) -> Option<WeexFrame> {
    let value: serde_json::Value = match serde_json::from_slice(data) {
        Ok(value) => value,
        Err(err) => {
            log_parse_drop_bytes("weex_ws", "json", &err, data);
            return None;
        }
    };
    let event = if value.get("result").is_some() {
        let success = value
            .get("result")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if !success {
            log_parse_drop(
                "weex_ws",
                "subscribe_error",
                &value
                    .get("msg")
                    .and_then(|value| value.as_str())
                    .unwrap_or("subscription result was false"),
                &String::from_utf8_lossy(data),
            );
            return None;
        }
        WeexMarketEvent::Subscribed {
            id: value.get("id").and_then(|value| value.as_u64()),
        }
    } else {
        let event_name = match value.get("e").and_then(|value| value.as_str()) {
            Some(event_name) => event_name,
            None => {
                log_parse_drop_bytes("weex_ws", "missing_event", &"expected field 'e'", data);
                return None;
            }
        };
        match event_name {
            "depth" => match serde_json::from_value(value) {
                Ok(depth) => WeexMarketEvent::Depth(depth),
                Err(err) => {
                    log_parse_drop_bytes("weex_ws", "depth_schema", &err, data);
                    return None;
                }
            },
            "trade" | "trades" => WeexMarketEvent::Trade(value),
            "ticker" => WeexMarketEvent::Ticker(value),
            other => {
                log_parse_drop(
                    "weex_ws",
                    "unexpected_event",
                    &format!("unsupported event '{other}'"),
                    &String::from_utf8_lossy(data),
                );
                return None;
            }
        }
    };
    Some(WeexFrame {
        ts,
        recv_instant,
        event,
    })
}

pub fn normalize_symbol(symbol: &str) -> String {
    symbol
        .trim()
        .replace(['_', '-', '/'], "")
        .to_ascii_uppercase()
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_depth_message() {
        let raw = br#"{"e":"depth","E":1773295701456,"s":"BTCUSDT","U":161,"u":161,"l":15,"d":"CHANGED","b":[["103435.90","2.10000"]],"a":[["103436.10","1.21500"]]}"#;
        let frame = parse_payload(raw, 1, Instant::now()).expect("valid frame");
        let WeexMarketEvent::Depth(depth) = frame.event else {
            panic!("expected depth");
        };
        assert_eq!(depth.last_update_id, 161);
        assert_eq!(depth.bids[0].1, "2.10000");
    }

    #[test]
    fn supplies_required_header_and_replies_to_both_ping_formats() {
        let handler = WeexHandler::new("BTCUSDT");
        assert_eq!(
            handler.connect_headers(),
            vec![(
                "User-Agent".to_string(),
                "rust-crypto-mm/weex-v3".to_string()
            )]
        );
        let expected = Some(r#"{"method":"PONG","id":1}"#.to_string());
        assert_eq!(
            handler.control_reply_text(r#"{"event":"ping","time":"1693208170000"}"#),
            expected
        );
        assert_eq!(
            handler.control_reply_text(r#"{"type":"ping","time":"1693208170000"}"#),
            expected
        );
    }
}
