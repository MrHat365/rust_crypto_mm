#![allow(dead_code)]

use crate::base_classes::types::Ts;
use crate::base_classes::ws::{ExchangeHandler, HeartbeatPayload};
use crate::exchanges::endpoints::WeexWs;
use crate::exchanges::weex::orderbook::WeexDepthMsg;
use crate::exchanges::weex::rest::normalize_symbol;
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
    event_cache: Option<String>,
}

pub struct WeexHandler {
    symbol: String,
    subs: Vec<String>,
}

impl WeexHandler {
    pub fn new<S: Into<String>>(symbol: S) -> Self {
        let symbol = normalize_symbol(&symbol.into());
        let subs = vec![WeexWs::subscribe(&symbol)];
        Self { symbol, subs }
    }
}

impl ExchangeHandler for WeexHandler {
    type Out = WeexFrame;

    fn url(&self) -> &str {
        WeexWs::PUBLIC_BASE
    }

    fn initial_subscriptions(&self) -> &[String] {
        &self.subs
    }

    fn parse_text(&self, text: &str, ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        let mut frame = WeexFrame::from_text(text, ts, recv_instant);
        frame.preparse_text(text);
        Some(frame)
    }

    fn parse_binary(&self, data: &[u8], ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        let text = match core::str::from_utf8(data) {
            Ok(text) => text,
            Err(err) => {
                log_parse_drop_bytes("weex_parser", "utf8", &err, data);
                return None;
            }
        };
        self.parse_text(text, ts, recv_instant)
    }

    fn app_heartbeat_interval(&self) -> Option<u64> {
        Some(15)
    }

    fn build_app_heartbeat(&self) -> Option<HeartbeatPayload> {
        let time_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Some(HeartbeatPayload::Text(format!(
            r#"{{"event":"ping","time":"{time_ms}"}}"#
        )))
    }

    fn extra_headers(&self) -> Vec<(String, String)> {
        vec![(
            "User-Agent".to_string(),
            "rust-crypto-mm/0.1".to_string(),
        )]
    }

    fn sequence_key_text(&self, text: &str) -> Option<(u64, u64)> {
        if !text.contains("\"e\":\"depth\"") {
            return None;
        }
        let u = find_json_u64(text, "u")?;
        let mut key = fnv1a64(self.symbol.as_bytes());
        key ^= 0x5758_4450_5448; // WXDPTH
        Some((key, u))
    }

    fn label(&self) -> String {
        format!("weex:{}", self.symbol)
    }
}

impl WeexFrame {
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

    pub fn preparse_text(&mut self, text: &str) {
        if self.event_cache.is_none() {
            self.event_cache = find_json_string(text, "e").map(|s| s.to_string());
        }
        let event = self.event_cache.as_deref().unwrap_or("");
        let needs_depth = event == "depth";
        let needs_json = needs_depth || event == "ticker" || event == "trade";
        if needs_json && self.json_cache.is_none() {
            self.json_cache = match serde_json::from_str::<Value>(text) {
                Ok(val) => Some(val),
                Err(err) => {
                    log_parse_drop("weex_parser", "json", &err, text);
                    None
                }
            };
        }
        if needs_depth && self.depth_cache.is_none() {
            self.depth_cache = match serde_json::from_str::<WeexDepthMsg>(text) {
                Ok(val) => Some(val),
                Err(err) => {
                    log_parse_drop("weex_parser", "depth", &err, text);
                    None
                }
            };
        }
    }

    #[inline(always)]
    pub fn text(&self) -> Option<&str> {
        match core::str::from_utf8(&self.raw) {
            Ok(text) => Some(text),
            Err(err) => {
                log_parse_drop_bytes("weex_parser", "utf8", &err, &self.raw);
                None
            }
        }
    }

    #[inline(always)]
    pub fn event(&self) -> &str {
        self.event_cache.as_deref().unwrap_or("(unknown)")
    }

    #[inline(always)]
    pub fn json(&mut self) -> Option<&Value> {
        if self.json_cache.is_none() {
            self.json_cache = match serde_json::from_slice(&self.raw) {
                Ok(val) => Some(val),
                Err(err) => {
                    log_parse_drop_bytes("weex_parser", "json", &err, &self.raw);
                    None
                }
            };
        }
        self.json_cache.as_ref()
    }

    #[inline(always)]
    pub fn depth_msg(&mut self) -> Option<&WeexDepthMsg> {
        if self.depth_cache.is_none() {
            self.depth_cache = match serde_json::from_slice(&self.raw) {
                Ok(val) => Some(val),
                Err(err) => {
                    log_parse_drop_bytes("weex_parser", "depth", &err, &self.raw);
                    None
                }
            };
        }
        self.depth_cache.as_ref()
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
    let quote = rest.find('"')?;
    let rest2 = &rest[quote + 1..];
    let end = rest2.find('"')?;
    Some(&rest2[..end])
}

fn find_json_u64(s: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let pos = s.find(&needle)?;
    let rest = &s[pos + needle.len()..];
    let mut v: u64 = 0;
    let mut found = false;
    for ch in rest.bytes() {
        if ch.is_ascii_digit() {
            found = true;
            v = v.saturating_mul(10).saturating_add((ch - b'0') as u64);
        } else if found {
            break;
        } else if ch == b' ' {
            continue;
        } else {
            return None;
        }
    }
    if found { Some(v) } else { None }
}
