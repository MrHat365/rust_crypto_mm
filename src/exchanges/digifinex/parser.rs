#![allow(dead_code)]

use crate::base_classes::types::Ts;
use crate::base_classes::ws::{AppHeartbeat, ExchangeHandler, HeartbeatPayload};
use crate::exchanges::digifinex::orderbook::DigifinexDepthMsg;
use crate::exchanges::digifinex::rest::normalize_instrument_id;
use crate::exchanges::endpoints::DigifinexWs;
use crate::utils::parsing::{log_parse_drop, log_parse_drop_bytes};
use flate2::read::{DeflateDecoder, ZlibDecoder};
use serde_json::{self, Value};
use std::io::Read;
use std::time::Instant;

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
}

impl DigifinexHandler {
    pub fn new<S: Into<String>>(symbol: S) -> Self {
        let instrument_id = normalize_instrument_id(&symbol.into());
        let subs = vec![
            DigifinexWs::subscribe_depth(&instrument_id, 20, 1),
            DigifinexWs::subscribe(DigifinexWs::TICKER, &instrument_id, 2),
            DigifinexWs::subscribe(DigifinexWs::TRADES, &instrument_id, 3),
        ];
        Self {
            instrument_id,
            subs,
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
        let text = match inflate_to_string(data) {
            Some(text) => text,
            None => {
                log_parse_drop_bytes(
                    "digifinex_parser",
                    "inflate",
                    &"zlib/deflate decode failed",
                    data,
                );
                return None;
            }
        };
        let mut frame = DigifinexFrame::from_text(&text, ts, recv_instant);
        frame.preparse_text(&text);
        Some(frame)
    }

    fn app_heartbeat(&self) -> Option<AppHeartbeat> {
        Some(AppHeartbeat {
            interval_secs: 20,
            payload: HeartbeatPayload::Text(DigifinexWs::ping(1)),
        })
    }

    fn sequence_key_text(&self, text: &str) -> Option<(u64, u64)> {
        let event = find_json_string(text, "event")?;
        if event != "depth.update" {
            return None;
        }
        let ts = find_nested_u64(text, "timestamp")?;
        let mut key = fnv1a64(self.instrument_id.as_bytes());
        key ^= 0x4446_4450_5448; // DFDPTH
        Some((key, ts))
    }

    fn sequence_key_binary(&self, data: &[u8]) -> Option<(u64, u64)> {
        let text = inflate_to_string(data)?;
        self.sequence_key_text(&text)
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
        if self.event_cache.is_none() {
            self.event_cache = find_json_string(text, "event").map(|s| s.to_string());
        }
        let event = self.event_cache.as_deref().unwrap_or("");
        let needs_depth = event == "depth.update";
        let needs_json = needs_depth || event == "ticker.update" || event == "trades.update";
        if needs_json && self.json_cache.is_none() {
            self.json_cache = match serde_json::from_str::<Value>(text) {
                Ok(val) => Some(val),
                Err(err) => {
                    log_parse_drop("digifinex_parser", "json", &err, text);
                    None
                }
            };
        }
        if needs_depth && self.depth_cache.is_none() {
            self.depth_cache = match serde_json::from_str::<DigifinexDepthMsg>(text) {
                Ok(val) => Some(val),
                Err(err) => {
                    log_parse_drop("digifinex_parser", "depth", &err, text);
                    None
                }
            };
        }
    }

    pub fn preparse_binary(&mut self) {
        let text = match core::str::from_utf8(&self.raw) {
            Ok(text) => text.to_string(),
            Err(err) => {
                log_parse_drop_bytes("digifinex_parser", "utf8", &err, &self.raw);
                return;
            }
        };
        self.preparse_text(&text);
    }

    #[inline(always)]
    pub fn text(&self) -> Option<&str> {
        match core::str::from_utf8(&self.raw) {
            Ok(text) => Some(text),
            Err(err) => {
                log_parse_drop_bytes("digifinex_parser", "utf8", &err, &self.raw);
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
                    log_parse_drop_bytes("digifinex_parser", "json", &err, &self.raw);
                    None
                }
            };
        }
        self.json_cache.as_ref()
    }

    #[inline(always)]
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

pub fn inflate_to_string(data: &[u8]) -> Option<String> {
    let try_zlib = || {
        let mut decoder = ZlibDecoder::new(data);
        let mut out = String::new();
        decoder.read_to_string(&mut out).ok()?;
        if out.is_empty() { None } else { Some(out) }
    };
    let try_deflate = || {
        let mut decoder = DeflateDecoder::new(data);
        let mut out = String::new();
        decoder.read_to_string(&mut out).ok()?;
        if out.is_empty() { None } else { Some(out) }
    };
    try_zlib()
        .or_else(try_deflate)
        .or_else(|| core::str::from_utf8(data).ok().map(|s| s.to_string()))
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

fn find_nested_u64(s: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\"");
    let pos = s.find(&needle)?;
    let rest = &s[pos + needle.len()..];
    let colon = rest.find(':')?;
    let rest = &rest[colon + 1..];
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

#[cfg(test)]
mod tests {
    use super::{find_json_string, inflate_to_string};
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    #[test]
    fn inflates_zlib_payload() {
        let payload = r#"{"event":"ticker.update","data":{"instrument_id":"BTCUSDTPERP"}}"#;
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(payload.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        let decoded = inflate_to_string(&compressed).expect("inflate");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn extracts_event_name() {
        let text = r#"{"event":"depth.update","data":{"timestamp":1}}"#;
        assert_eq!(find_json_string(text, "event"), Some("depth.update"));
    }
}
