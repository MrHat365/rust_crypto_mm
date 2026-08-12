#![allow(dead_code)]

use crate::base_classes::types::Ts;
use crate::base_classes::ws::{ExchangeHandler, HeartbeatPayload};
use crate::exchanges::endpoints::WeexWs;
use crate::exchanges::weex::orderbook::WeexDepthMsg;
use crate::utils::parsing::{log_parse_drop, log_parse_drop_bytes};
use serde_json::{self, Value};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct WeexFrame {
    pub ts: Ts,
    pub recv_instant: Instant,
    pub raw: Vec<u8>,
    json_cache: Option<Value>,
    depth_cache: Option<WeexDepthMsg>,
    channel_cache: Option<String>,
}

pub struct WeexHandler {
    symbol: String,
    subs: Vec<String>,
}

impl WeexHandler {
    pub fn new<S: Into<String>>(symbol: S) -> Self {
        let symbol = crate::exchanges::weex::rest::normalize_symbol(symbol.into());
        let subs = vec![WeexWs::subscribe_multi(
            &symbol,
            &[
                WeexWs::DEPTH15,
                WeexWs::TRADE,
                WeexWs::TICKER,
            ],
        )];
        Self { symbol, subs }
    }
}

impl ExchangeHandler for WeexHandler {
    type Out = WeexFrame;

    fn url(&self) -> &str {
        WeexWs::BASE
    }

    fn initial_subscriptions(&self) -> &[String] {
        &self.subs
    }

    fn parse_text(&self, text: &str, ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        if should_drop_control_message(text) {
            return None;
        }
        let mut frame = WeexFrame::from_text(text, ts, recv_instant);
        frame.preparse_text(text);
        Some(frame)
    }

    fn parse_binary(&self, data: &[u8], ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        let text = match core::str::from_utf8(data) {
            Ok(text) => text,
            Err(_) => {
                let mut frame = WeexFrame::from_bytes(data.to_vec(), ts, recv_instant);
                frame.preparse_binary();
                return Some(frame);
            }
        };
        if should_drop_control_message(text) {
            return None;
        }
        let mut frame = WeexFrame::from_bytes(data.to_vec(), ts, recv_instant);
        frame.preparse_binary();
        Some(frame)
    }

    fn app_heartbeat_interval(&self) -> Option<u64> {
        Some(15)
    }

    fn build_app_heartbeat(&self) -> Option<HeartbeatPayload> {
        let ms = now_ms();
        Some(HeartbeatPayload::Text(format!(
            r#"{{"event":"ping","time":"{ms}"}}"#
        )))
    }

    fn connect_headers(&self) -> Vec<(String, String)> {
        vec![("User-Agent".into(), "rust_test-weex/0.1".into())]
    }

    fn label(&self) -> String {
        format!("weex:{}", self.symbol)
    }
}

/// Drop server heartbeats and subscription acks. Server JSON pings are answered in `ws.rs`.
fn should_drop_control_message(text: &str) -> bool {
    if text.contains("\"event\":\"ping\"") || text.contains("\"event\": \"ping\"") {
        return true;
    }
    if text.contains("\"result\"") && !text.contains("\"e\"") {
        return true;
    }
    false
}

#[inline(always)]
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn find_event_type(text: &str) -> Option<&str> {
    find_json_string(text, "e")
}

fn find_json_string<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let pos = s.find(&needle)?;
    let rest = &s[pos + needle.len()..];
    let colon = rest.find(':')?;
    let rest = &rest[colon + 1..];
    let quote = rest.find('"')?;
    let rest2 = &rest[quote + 1..];
    let end = rest2.find('"')?;
    Some(&rest2[..end])
}

fn weex_preparse_flags(text: &str) -> (Option<&str>, bool, bool) {
    let event = find_event_type(text);
    let needs_depth = matches!(event, Some("depth"));
    let needs_json = matches!(event, Some("depth" | "trade" | "ticker"));
    (event, needs_json, needs_depth)
}

impl WeexFrame {
    pub fn from_text(text: &str, ts: Ts, recv_instant: Instant) -> Self {
        Self {
            ts,
            recv_instant,
            raw: text.as_bytes().to_vec(),
            json_cache: None,
            depth_cache: None,
            channel_cache: None,
        }
    }

    pub fn from_bytes(raw: Vec<u8>, ts: Ts, recv_instant: Instant) -> Self {
        Self {
            ts,
            recv_instant,
            raw,
            json_cache: None,
            depth_cache: None,
            channel_cache: None,
        }
    }

    pub fn preparse_text(&mut self, text: &str) {
        let (event_opt, needs_json, needs_depth) = weex_preparse_flags(text);
        if self.channel_cache.is_none() {
            if let Some(ev) = event_opt {
                self.channel_cache = Some(ev.to_string());
            }
        }

        if needs_json && self.json_cache.is_none() {
            match serde_json::from_str::<Value>(text) {
                Ok(value) => {
                    if self.channel_cache.is_none() {
                        self.channel_cache = value
                            .get("e")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                    }
                    self.json_cache = Some(value);
                }
                Err(err) => {
                    log_parse_drop("weex_parser", "json", &err, text);
                }
            }
        }

        if needs_depth && self.depth_cache.is_none() {
            match serde_json::from_str::<WeexDepthMsg>(text) {
                Ok(msg) => self.depth_cache = Some(msg),
                Err(err) => {
                    log_parse_drop("weex_parser", "depth", &err, text);
                }
            }
        }
    }

    pub fn preparse_binary(&mut self) {
        let text = match core::str::from_utf8(&self.raw) {
            Ok(text) => text,
            Err(err) => {
                log_parse_drop_bytes("weex_parser", "utf8", &err, &self.raw);
                return;
            }
        };
        let (event_opt, needs_json, needs_depth) = weex_preparse_flags(text);
        if !needs_json && !needs_depth && event_opt.is_none() {
            return;
        }
        if self.channel_cache.is_none() {
            if let Some(ev) = event_opt {
                self.channel_cache = Some(ev.to_string());
            }
        }
        if needs_json && self.json_cache.is_none() {
            match serde_json::from_slice::<Value>(&self.raw) {
                Ok(value) => {
                    if self.channel_cache.is_none() {
                        self.channel_cache = value
                            .get("e")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                    }
                    self.json_cache = Some(value);
                }
                Err(err) => {
                    log_parse_drop_bytes("weex_parser", "json", &err, &self.raw);
                }
            }
        }
        if needs_depth && self.depth_cache.is_none() {
            match serde_json::from_slice::<WeexDepthMsg>(&self.raw) {
                Ok(msg) => self.depth_cache = Some(msg),
                Err(err) => {
                    log_parse_drop_bytes("weex_parser", "depth", &err, &self.raw);
                }
            }
        }
    }

    pub fn channel(&self) -> Option<&str> {
        self.channel_cache.as_deref()
    }

    pub fn text(&self) -> Option<&str> {
        match core::str::from_utf8(&self.raw) {
            Ok(text) => Some(text),
            Err(err) => {
                log_parse_drop_bytes("weex_parser", "utf8", &err, &self.raw);
                None
            }
        }
    }

    pub fn json(&mut self) -> Option<&Value> {
        if self.json_cache.is_none() {
            match serde_json::from_slice::<Value>(&self.raw) {
                Ok(value) => {
                    self.channel_cache = value
                        .get("e")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    self.json_cache = Some(value);
                }
                Err(err) => {
                    log_parse_drop_bytes("weex_parser", "json", &err, &self.raw);
                }
            }
        }
        self.json_cache.as_ref()
    }

    pub fn depth_msg(&mut self) -> Option<&WeexDepthMsg> {
        if self.depth_cache.is_none() && self.channel().map_or(false, |ch| ch == "depth") {
            match serde_json::from_slice::<WeexDepthMsg>(&self.raw) {
                Ok(msg) => self.depth_cache = Some(msg),
                Err(err) => {
                    log_parse_drop_bytes("weex_parser", "depth", &err, &self.raw);
                }
            }
        }
        self.depth_cache.as_ref()
    }
}
