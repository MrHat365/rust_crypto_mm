#![allow(dead_code)]

use std::time::Instant;

use serde_json::{self, Value};

use crate::base_classes::types::Ts;
use crate::base_classes::ws::ExchangeHandler;
use crate::exchanges::endpoints::WeexWs;
use crate::exchanges::weex::orderbook::WeexDepthMsg;
use crate::exchanges::weex::rest::normalize_symbol;
use crate::utils::parsing::{log_parse_drop, log_parse_drop_bytes};

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
        let subs = vec![format!(
            r#"{{"method":"SUBSCRIBE","params":["{symbol}@depth15","{symbol}@ticker","{symbol}@trade"],"id":1}}"#
        )];
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

    fn connect_headers(&self) -> &[(&str, &str)] {
        static HEADERS: [(&str, &str); 1] = [("User-Agent", "rust_test/0.1")];
        &HEADERS
    }

    fn parse_text(&self, text: &str, ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        if text.contains("\"event\":\"ping\"") {
            return None;
        }
        if text.contains("\"result\"") && !text.contains("\"e\"") {
            return None;
        }
        let mut frame = WeexFrame::from_text(text, ts, recv_instant);
        frame.preparse_text(text);
        Some(frame)
    }

    fn parse_binary(&self, data: &[u8], ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        let text = match std::str::from_utf8(data) {
            Ok(s) => s,
            Err(err) => {
                log_parse_drop_bytes("weex_parser", "utf8", &err, data);
                return None;
            }
        };
        self.parse_text(text, ts, recv_instant)
    }

    fn sequence_key_text(&self, text: &str) -> Option<(u64, u64)> {
        if !text.contains("\"e\":\"depth\"") {
            return None;
        }
        let seq = find_json_u64(text, "u")?;
        Some((0x5745_4558, seq))
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
        if self.json_cache.is_none() {
            self.json_cache = match serde_json::from_str::<Value>(text) {
                Ok(val) => Some(val),
                Err(err) => {
                    log_parse_drop("weex_parser", "json", &err, text);
                    None
                }
            };
        }
        let event = self
            .json_cache
            .as_ref()
            .and_then(|v| v.get("e").and_then(|e| e.as_str()));
        if let Some(event) = event {
            self.event_cache = Some(event.to_string());
            if event == "depth" && self.depth_cache.is_none() {
                self.depth_cache = match serde_json::from_str::<WeexDepthMsg>(text) {
                    Ok(val) => Some(val),
                    Err(err) => {
                        log_parse_drop("weex_parser", "depth", &err, text);
                        None
                    }
                };
            }
        }
    }

    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.raw).ok()
    }

    pub fn event(&self) -> Option<&str> {
        self.event_cache.as_deref()
    }

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

fn find_json_u64(s: &str, key: &str) -> Option<u64> {
    let k = format!("\"{key}\":");
    let pos = s.find(&k)?;
    let rest = &s[pos + k.len()..];
    let mut val: u64 = 0;
    let mut found = false;
    for ch in rest.bytes() {
        if ch.is_ascii_digit() {
            found = true;
            val = val.saturating_mul(10).saturating_add((ch - b'0') as u64);
        } else if found {
            break;
        }
    }
    if found { Some(val) } else { None }
}
