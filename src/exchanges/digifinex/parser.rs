#![allow(dead_code)]

use crate::base_classes::types::Ts;
use crate::base_classes::ws::{AppHeartbeat, ExchangeHandler, HeartbeatPayload};
use crate::exchanges::digifinex::orderbook::DigiFinexDepthMsg;
use crate::exchanges::endpoints::DigiFinexWs;
use crate::utils::parsing::{log_parse_drop, log_parse_drop_bytes};
use flate2::read::ZlibDecoder;
use serde_json::{self, Value};
use std::io::Read;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct DigiFinexFrame {
    pub ts: Ts,
    pub recv_instant: Instant,
    pub raw: Vec<u8>,
    json_cache: Option<Value>,
    depth_cache: Option<DigiFinexDepthMsg>,
    event_cache: Option<String>,
}

pub struct DigiFinexHandler {
    instrument_id: String,
    subs: Vec<String>,
}

impl DigiFinexHandler {
    pub fn new<S: Into<String>>(symbol: S) -> Self {
        let instrument_id = normalize_instrument_id(&symbol.into());
        let mut subs = Vec::with_capacity(3);
        subs.push(DigiFinexWs::sub_depth(&instrument_id, 10));
        subs.push(DigiFinexWs::sub_trades(&instrument_id));
        subs.push(DigiFinexWs::sub_ticker(&instrument_id));
        Self {
            instrument_id,
            subs,
        }
    }
}

impl ExchangeHandler for DigiFinexHandler {
    type Out = DigiFinexFrame;

    fn url(&self) -> &str {
        DigiFinexWs::BASE
    }

    fn initial_subscriptions(&self) -> &[String] {
        &self.subs
    }

    fn parse_text(&self, text: &str, ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        if !is_json_object_text(text) {
            return None;
        }
        if is_control_event_text(text) {
            return None;
        }
        let mut frame = DigiFinexFrame::from_text(text, ts, recv_instant);
        frame.preparse_text(text);
        Some(frame)
    }

    fn parse_binary(&self, data: &[u8], ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        // DigiFinex swap WS pushes zlib-compressed JSON on binary frames.
        let text = match decode_digifinex_payload(data) {
            Some(text) => text,
            None => return None,
        };
        if !is_json_object_text(&text) || is_control_event_text(&text) {
            return None;
        }
        let mut frame = DigiFinexFrame::from_text(&text, ts, recv_instant);
        frame.preparse_text(&text);
        Some(frame)
    }

    fn app_heartbeat(&self) -> Option<AppHeartbeat> {
        Some(AppHeartbeat {
            interval_secs: 20,
            payload: HeartbeatPayload::Text(DigiFinexWs::ping(0)),
        })
    }

    fn sequence_key_text(&self, text: &str) -> Option<(u64, u64)> {
        let event = find_json_string(text, "event")?;
        if event != DigiFinexWs::DEPTH_UPDATE {
            return None;
        }
        let ts = find_json_u64(text, "timestamp")?;
        let mut key = fnv1a64(self.instrument_id.as_bytes());
        key ^= 0x4446_5F44; // "DF_D"
        Some((key, ts))
    }

    fn label(&self) -> String {
        format!("digifinex:{}", self.instrument_id)
    }
}

impl DigiFinexFrame {
    pub fn from_text(text: &str, ts: Ts, recv_instant: Instant) -> Self {
        Self {
            ts,
            recv_instant,
            raw: text.as_bytes().to_vec(),
            json_cache: None,
            depth_cache: None,
            event_cache: None,
        }
    }

    pub fn from_bytes(raw: Vec<u8>, ts: Ts, recv_instant: Instant) -> Self {
        Self {
            ts,
            recv_instant,
            raw,
            json_cache: None,
            depth_cache: None,
            event_cache: None,
        }
    }

    pub fn preparse_text(&mut self, text: &str) {
        let flags = digifinex_preparse_flags(text);
        if self.event_cache.is_none() {
            if let Some(ev) = find_json_string(text, "event") {
                self.event_cache = Some(ev.to_string());
            }
        }
        if flags.needs_json && self.json_cache.is_none() {
            match serde_json::from_str::<Value>(text) {
                Ok(value) => {
                    if self.event_cache.is_none() {
                        self.event_cache = value
                            .get("event")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                    }
                    self.json_cache = Some(value);
                }
                Err(err) => {
                    log_parse_drop("digifinex_parser", "json", &err, text);
                }
            }
        }
        if flags.needs_depth && self.depth_cache.is_none() {
            match serde_json::from_str::<DigiFinexDepthMsg>(text) {
                Ok(msg) => self.depth_cache = Some(msg),
                Err(err) => {
                    log_parse_drop("digifinex_parser", "depth", &err, text);
                }
            }
        }
    }

    pub fn preparse_binary(&mut self) {
        let text = match core::str::from_utf8(&self.raw) {
            Ok(text) => text,
            Err(err) => {
                log_parse_drop_bytes("digifinex_parser", "utf8", &err, &self.raw);
                return;
            }
        };
        let flags = digifinex_preparse_flags(text);
        if !flags.needs_json && !flags.needs_depth {
            return;
        }
        if self.event_cache.is_none() {
            if let Some(ev) = find_json_string(text, "event") {
                self.event_cache = Some(ev.to_string());
            }
        }
        if flags.needs_json && self.json_cache.is_none() {
            match serde_json::from_slice::<Value>(&self.raw) {
                Ok(value) => {
                    if self.event_cache.is_none() {
                        self.event_cache = value
                            .get("event")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                    }
                    self.json_cache = Some(value);
                }
                Err(err) => {
                    log_parse_drop_bytes("digifinex_parser", "json", &err, &self.raw);
                }
            }
        }
        if flags.needs_depth && self.depth_cache.is_none() {
            match serde_json::from_slice::<DigiFinexDepthMsg>(&self.raw) {
                Ok(msg) => self.depth_cache = Some(msg),
                Err(err) => {
                    log_parse_drop_bytes("digifinex_parser", "depth", &err, &self.raw);
                }
            }
        }
    }

    pub fn event(&self) -> Option<&str> {
        self.event_cache.as_deref()
    }

    pub fn text(&self) -> Option<&str> {
        match core::str::from_utf8(&self.raw) {
            Ok(text) => Some(text),
            Err(err) => {
                log_parse_drop_bytes("digifinex_parser", "utf8", &err, &self.raw);
                None
            }
        }
    }

    pub fn json(&mut self) -> Option<&Value> {
        if self.json_cache.is_none() {
            match serde_json::from_slice::<Value>(&self.raw) {
                Ok(value) => {
                    self.event_cache = value
                        .get("event")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    self.json_cache = Some(value);
                }
                Err(err) => {
                    log_parse_drop_bytes("digifinex_parser", "json", &err, &self.raw);
                }
            }
        }
        self.json_cache.as_ref()
    }

    pub fn depth_msg(&mut self) -> Option<&DigiFinexDepthMsg> {
        if self.depth_cache.is_none()
            && self.event().map_or(false, |ev| ev == DigiFinexWs::DEPTH_UPDATE)
        {
            match serde_json::from_slice::<DigiFinexDepthMsg>(&self.raw) {
                Ok(msg) => self.depth_cache = Some(msg),
                Err(err) => {
                    log_parse_drop_bytes("digifinex_parser", "depth", &err, &self.raw);
                }
            }
        }
        self.depth_cache.as_ref()
    }
}

/// Normalize user-facing symbols to DigiFinex `instrument_id` format.
pub fn normalize_instrument_id(symbol: &str) -> String {
    let mut s = symbol
        .trim()
        .replace(['_', '-', '/'], "")
        .to_ascii_uppercase();
    if s.ends_with("USDT") && !s.ends_with("USDTPERP") {
        s.push_str("PERP");
    }
    s
}

struct DigiFinexParseFlags {
    needs_json: bool,
    needs_depth: bool,
}

#[inline(always)]
fn digifinex_preparse_flags(text: &str) -> DigiFinexParseFlags {
    let is_depth = text.contains("\"event\":\"depth.update\"");
    let is_trades = text.contains("\"event\":\"trades.update\"");
    let is_ticker = text.contains("\"event\":\"ticker.update\"");
    DigiFinexParseFlags {
        needs_json: is_trades || is_ticker,
        needs_depth: is_depth,
    }
}

#[inline(always)]
fn is_json_object_text(text: &str) -> bool {
    text.trim_start().starts_with('{')
}

#[inline(always)]
fn is_control_event_text(text: &str) -> bool {
    matches!(
        find_json_string(text, "event"),
        Some("server.ping")
            | Some("server.pong")
            | Some("server.time")
            | Some("ping")
            | Some("pong")
            | Some("depth.subscribe")
            | Some("trades.subscribe")
            | Some("ticker.subscribe")
            | Some("depth.unsubscribe")
            | Some("trades.unsubscribe")
            | Some("ticker.unsubscribe")
    )
}

/// DigiFinex public WS payloads are zlib-compressed JSON bytes (header `0x78 0xda` …).
/// Plain UTF-8 JSON is also accepted for robustness / tests.
pub fn decode_digifinex_payload(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    if let Ok(text) = core::str::from_utf8(data) {
        if text.starts_with('{') {
            return Some(text.to_string());
        }
    }
    let mut decoder = ZlibDecoder::new(data);
    let mut out = String::new();
    match decoder.read_to_string(&mut out) {
        Ok(_) if out.starts_with('{') => Some(out),
        Ok(_) => {
            log_parse_drop_bytes(
                "digifinex_parser",
                "zlib_not_json",
                &"decompressed payload is not a JSON object",
                data,
            );
            None
        }
        Err(err) => {
            log_parse_drop_bytes("digifinex_parser", "zlib", &err, data);
            None
        }
    }
}

#[inline(always)]
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn find_json_string<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let pos = s.find(&needle)?;
    let rest = &s[pos + needle.len()..];
    let colon = rest.find(':')?;
    let rest = &rest[colon + 1..];
    let start = rest.find('"')?;
    let rest = &rest[start + 1..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn find_json_u64(s: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let pos = s.find(&needle)?;
    let rest = &s[pos + needle.len()..];
    let mut value: u64 = 0;
    let mut found = false;
    for ch in rest.bytes() {
        if ch.is_ascii_digit() {
            found = true;
            value = value.saturating_mul(10).saturating_add((ch - b'0') as u64);
        } else if found {
            break;
        } else if ch == b' ' {
            continue;
        } else {
            return None;
        }
    }
    if found { Some(value) } else { None }
}

#[cfg(test)]
mod tests {
    use super::{normalize_instrument_id, DigiFinexHandler};
    use crate::base_classes::ws::ExchangeHandler;
    use std::time::Instant;

    #[test]
    fn normalize_instrument_id_examples() {
        assert_eq!(normalize_instrument_id("BTC_USDT"), "BTCUSDTPERP");
        assert_eq!(normalize_instrument_id("BTCUSDT"), "BTCUSDTPERP");
        assert_eq!(normalize_instrument_id("BTCUSDTPERP"), "BTCUSDTPERP");
    }

    #[test]
    fn parse_text_ignores_server_ping_control_frame() {
        let handler = DigiFinexHandler::new("ETH_USDT");
        let text = r#"{"event":"server.ping","id":1,"code":1,"msg":"success","data":"pong"}"#;
        let parsed = handler.parse_text(text, 1, Instant::now());
        assert!(parsed.is_none(), "server.ping control frame must be ignored");
    }

    #[test]
    fn parse_text_accepts_depth_update_frame() {
        let handler = DigiFinexHandler::new("ETH_USDT");
        let text = r#"{"event":"depth.update","data":{"instrument_id":"ETHUSDTPERP","level":10,"timestamp":1662173255498,"asks":[["1","1"]],"bids":[["1","1"]]}}"#;
        let parsed = handler.parse_text(text, 1, Instant::now());
        assert!(parsed.is_some(), "depth.update frame must be parsed");
    }
}
