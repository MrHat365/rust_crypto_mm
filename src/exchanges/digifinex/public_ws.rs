use std::io::Read;
use std::time::Instant;

use flate2::read::ZlibDecoder;
use serde::Deserialize;

use crate::base_classes::types::Ts;
use crate::base_classes::ws::{ExchangeHandler, HeartbeatPayload};
use crate::utils::parsing::{log_parse_drop, log_parse_drop_bytes};

pub const DIGIFINEX_SWAP_WS_URL: &str = "wss://openapi.digifinex.com/swap_ws/v2/";

#[derive(Debug, Clone)]
pub struct DigiFinexFrame {
    pub ts: Ts,
    pub recv_instant: Instant,
    pub event: DigiFinexMarketEvent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DigiFinexMarketEvent {
    Depth(DepthUpdate),
    Trades(Vec<TradeUpdate>),
    Ticker(TickerUpdate),
    Control(ControlMessage),
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DepthUpdate {
    pub instrument_id: String,
    pub level: u16,
    pub timestamp: u64,
    pub asks: Vec<(String, f64)>,
    pub bids: Vec<(String, f64)>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TradeUpdate {
    pub instrument_id: String,
    pub trade_id: String,
    pub trade_time: u64,
    pub volume: String,
    pub price: String,
    pub direction: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TickerUpdate {
    pub instrument_id: String,
    pub best_bid: String,
    pub best_bid_size: String,
    pub best_ask: String,
    pub best_ask_size: String,
    pub last: String,
    pub last_qty: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ControlMessage {
    pub event: String,
    pub id: Option<u64>,
    pub code: i64,
    pub msg: String,
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    event: String,
    data: T,
}

pub struct DigiFinexHandler {
    instrument_id: String,
    subscriptions: Vec<String>,
}

impl DigiFinexHandler {
    pub fn new(symbol: impl AsRef<str>) -> Self {
        let instrument_id = normalize_instrument_id(symbol.as_ref());
        let subscriptions = vec![
            format!(
                r#"{{"event":"depth.subscribe","id":1,"instrument_id":"{instrument_id}","level":20}}"#
            ),
            format!(
                r#"{{"event":"trades.subscribe","id":2,"instrument_id":"{instrument_id}"}}"#
            ),
            format!(
                r#"{{"event":"ticker.subscribe","id":3,"instrument_id":"{instrument_id}"}}"#
            ),
        ];
        Self {
            instrument_id,
            subscriptions,
        }
    }
}

impl ExchangeHandler for DigiFinexHandler {
    type Out = DigiFinexFrame;

    fn url(&self) -> &str {
        DIGIFINEX_SWAP_WS_URL
    }

    fn initial_subscriptions(&self) -> &[String] {
        &self.subscriptions
    }

    fn parse_text(&self, text: &str, ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        parse_payload(text.as_bytes(), ts, recv_instant)
    }

    fn parse_binary(&self, data: &[u8], ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        let mut decoder = ZlibDecoder::new(data);
        let mut decoded = Vec::new();
        if let Err(err) = decoder.read_to_end(&mut decoded) {
            log_parse_drop_bytes("digifinex_ws", "zlib", &err, data);
            return None;
        }
        parse_payload(&decoded, ts, recv_instant)
    }

    fn app_heartbeat_interval(&self) -> Option<u64> {
        Some(30)
    }

    fn build_app_heartbeat(&self) -> Option<HeartbeatPayload> {
        Some(HeartbeatPayload::Text(
            r#"{"event":"server.ping","id":9000}"#.to_string(),
        ))
    }

    fn label(&self) -> String {
        format!("digifinex:{}", self.instrument_id)
    }
}

fn parse_payload(data: &[u8], ts: Ts, recv_instant: Instant) -> Option<DigiFinexFrame> {
    let value: serde_json::Value = match serde_json::from_slice(data) {
        Ok(value) => value,
        Err(err) => {
            log_parse_drop_bytes("digifinex_ws", "json", &err, data);
            return None;
        }
    };
    let event_name = match value.get("event").and_then(|value| value.as_str()) {
        Some(event) => event,
        None => {
            log_parse_drop_bytes(
                "digifinex_ws",
                "missing_event",
                &"expected string field 'event'",
                data,
            );
            return None;
        }
    };

    let event = match event_name {
        "depth.update" => decode_envelope::<DepthUpdate>(value, data)
            .map(|envelope| DigiFinexMarketEvent::Depth(envelope.data)),
        "trades.update" => decode_envelope::<Vec<TradeUpdate>>(value, data)
            .map(|envelope| DigiFinexMarketEvent::Trades(envelope.data)),
        "ticker.update" => decode_envelope::<TickerUpdate>(value, data)
            .map(|envelope| DigiFinexMarketEvent::Ticker(envelope.data)),
        name if name.ends_with(".subscribe")
            || name == "server.ping"
            || name == "server.time"
            || name == "server.auth" =>
        {
            match serde_json::from_value::<ControlMessage>(value) {
                Ok(control) if control.code == 1 => Some(DigiFinexMarketEvent::Control(control)),
                Ok(control) => {
                    log_parse_drop(
                        "digifinex_ws",
                        "control_error",
                        &format!("code={} msg={}", control.code, control.msg),
                        &String::from_utf8_lossy(data),
                    );
                    None
                }
                Err(err) => {
                    log_parse_drop_bytes("digifinex_ws", "control", &err, data);
                    None
                }
            }
        }
        other => {
            log_parse_drop(
                "digifinex_ws",
                "unexpected_event",
                &format!("unsupported event '{other}'"),
                &String::from_utf8_lossy(data),
            );
            None
        }
    }?;
    Some(DigiFinexFrame {
        ts,
        recv_instant,
        event,
    })
}

fn decode_envelope<T: for<'de> Deserialize<'de>>(
    value: serde_json::Value,
    raw: &[u8],
) -> Option<Envelope<T>> {
    match serde_json::from_value::<Envelope<T>>(value) {
        Ok(envelope) => {
            debug_assert!(!envelope.event.is_empty());
            Some(envelope)
        }
        Err(err) => {
            log_parse_drop_bytes("digifinex_ws", "event_schema", &err, raw);
            None
        }
    }
}

pub fn normalize_instrument_id(symbol: &str) -> String {
    let compact = symbol
        .trim()
        .replace(['_', '-', '/'], "")
        .to_ascii_uppercase();
    if compact.ends_with("PERP") {
        compact
    } else {
        format!("{compact}PERP")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_depth_update() {
        let raw = br#"{"event":"depth.update","data":{"instrument_id":"BTCUSDTPERP","level":20,"timestamp":1662173255498,"asks":[["19964.25",561]],"bids":[["19928.54",1001]]}}"#;
        let frame = parse_payload(raw, 1, Instant::now()).expect("valid frame");
        let DigiFinexMarketEvent::Depth(depth) = frame.event else {
            panic!("expected depth");
        };
        assert_eq!(depth.instrument_id, "BTCUSDTPERP");
        assert_eq!(depth.bids[0], ("19928.54".to_string(), 1001.0));
    }

    #[test]
    fn normalizes_swap_symbols() {
        assert_eq!(normalize_instrument_id("btc_usdt"), "BTCUSDTPERP");
        assert_eq!(normalize_instrument_id("BTCUSDTPERP"), "BTCUSDTPERP");
    }
}
