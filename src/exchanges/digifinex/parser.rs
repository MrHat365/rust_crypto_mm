#![allow(dead_code)]

use std::io::Read;
use std::time::Instant;

use flate2::read::{DeflateDecoder, ZlibDecoder};
use serde_json::{self, Value};

use crate::base_classes::types::Ts;
use crate::base_classes::ws::{ExchangeHandler, HeartbeatPayload};
use crate::exchanges::digifinex::orderbook::DigifinexDepthMsg;
use crate::exchanges::digifinex::rest::to_instrument_id;
use crate::exchanges::endpoints::DigifinexWs;
use crate::utils::parsing::{log_parse_drop, log_parse_drop_bytes};

#[derive(Debug, Clone)]
pub struct DigifinexFrame {
    pub ts: Ts,
    pub recv_instant: Instant,
    pub raw: Vec<u8>,
    json_cache: Option<Value>,
    depth_cache: Option<DigifinexDepthMsg>,
    event_cache: Option<String>,
}

pub struct DigifinexHandler {
    instrument_id: String,
    subs: Vec<String>,
    ping_id: std::sync::atomic::AtomicU64,
}

impl DigifinexHandler {
    pub fn new<S: Into<String>>(symbol: S) -> Self {
        let instrument_id = to_instrument_id(&symbol.into());
        let subs = vec![
            format!(
                r#"{{"event":"depth.subscribe","id":1,"instrument_id":"{instrument_id}","level":20}}"#
            ),
            format!(
                r#"{{"event":"ticker.subscribe","id":2,"instrument_id":"{instrument_id}"}}"#
            ),
            format!(
                r#"{{"event":"trades.subscribe","id":3,"instrument_id":"{instrument_id}"}}"#
            ),
        ];
        Self {
            instrument_id,
            subs,
            ping_id: std::sync::atomic::AtomicU64::new(100),
        }
    }
}

impl ExchangeHandler for DigifinexHandler {
    type Out = DigifinexFrame;

    fn url(&self) -> &str {
        DigifinexWs::BASE
    }

    fn initial_subscriptions(&self) -> &[String] {
        &self.subs
    }

    fn parse_text(&self, text: &str, ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        let mut frame = DigifinexFrame::from_text(text, ts, recv_instant);
        frame.preparse_text(text);
        Some(frame)
    }

    fn parse_binary(&self, data: &[u8], ts: Ts, recv_instant: Instant) -> Option<Self::Out> {
        let decompressed = decompress_payload(data)?;
        let text = match std::str::from_utf8(&decompressed) {
            Ok(s) => s,
            Err(err) => {
                log_parse_drop_bytes("digifinex_parser", "utf8", &err, &decompressed);
                return None;
            }
        };
        let mut frame = DigifinexFrame::from_text(text, ts, recv_instant);
        frame.preparse_text(text);
        Some(frame)
    }

    fn app_heartbeat_interval(&self) -> Option<u64> {
        Some(30)
    }

    fn build_app_heartbeat(&self) -> Option<HeartbeatPayload> {
        let id = self
            .ping_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(HeartbeatPayload::Text(format!(
            r#"{{"event":"server.ping","id":{id}}}"#
        )))
    }

    fn sequence_key_text(&self, text: &str) -> Option<(u64, u64)> {
        if !text.contains("depth.update") {
            return None;
        }
        let ts = find_json_u64(text, "timestamp")?;
        Some((0x4449_4749, ts))
    }

    fn sequence_key_binary(&self, data: &[u8]) -> Option<(u64, u64)> {
        let decompressed = decompress_payload(data)?;
        let text = std::str::from_utf8(&decompressed).ok()?;
        self.sequence_key_text(text)
    }

    fn label(&self) -> String {
        format!("digifinex:{}", self.instrument_id)
    }
}

impl DigifinexFrame {
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
        if text.contains("\"event\":\"server.ping\"") || text.contains("\"event\":\"server.time\"") {
            return;
        }
        if self.json_cache.is_none() {
            self.json_cache = match serde_json::from_str::<Value>(text) {
                Ok(val) => Some(val),
                Err(err) => {
                    log_parse_drop("digifinex_parser", "json", &err, text);
                    None
                }
            };
        }
        if let Some(event) = self
            .json_cache
            .as_ref()
            .and_then(|v| v.get("event"))
            .and_then(|v| v.as_str())
        {
            self.event_cache = Some(event.to_string());
            if event == "depth.update" && self.depth_cache.is_none() {
                self.depth_cache = match serde_json::from_str::<DigifinexDepthMsg>(text) {
                    Ok(val) => Some(val),
                    Err(err) => {
                        log_parse_drop("digifinex_parser", "depth", &err, text);
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
        self.event_cache.as_deref().or_else(|| {
            self.json_cache
                .as_ref()
                .and_then(|v| v.get("event"))
                .and_then(|v| v.as_str())
        })
    }

    pub fn json(&mut self) -> Option<&Value> {
        if self.json_cache.is_none() {
            self.json_cache = match serde_json::from_slice(&self.raw) {
                Ok(val) => Some(val),
                Err(err) => {
                    log_parse_drop_bytes("digifinex_parser", "json", &err, &self.raw);
                    None
                }
            };
        }
        self.json_cache.as_ref()
    }

    pub fn depth_msg(&mut self) -> Option<&DigifinexDepthMsg> {
        if self.depth_cache.is_none() {
            self.depth_cache = match serde_json::from_slice(&self.raw) {
                Ok(val) => Some(val),
                Err(err) => {
                    log_parse_drop_bytes("digifinex_parser", "depth", &err, &self.raw);
                    None
                }
            };
        }
        self.depth_cache.as_ref()
    }
}

fn decompress_payload(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    if ZlibDecoder::new(data).read_to_end(&mut out).is_ok() && !out.is_empty() {
        return Some(out);
    }
    out.clear();
    if DeflateDecoder::new(data).read_to_end(&mut out).is_ok() && !out.is_empty() {
        return Some(out);
    }
    None
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
