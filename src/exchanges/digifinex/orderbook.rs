#![allow(dead_code)]

use crate::base_classes::order_book::ArrayOrderBook;
use crate::base_classes::orderbook_trait::OrderBookOps;
use crate::base_classes::types::*;
use crate::utils::parsing::log_parse_drop;
use crate::utils::time::ms_to_ns;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct DigifinexDepthMsg {
    pub event: String,
    pub data: DigifinexDepthData,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DigifinexDepthData {
    pub instrument_id: String,
    #[serde(default)]
    pub level: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_ts_opt")]
    pub timestamp: Option<u64>,
    #[serde(default)]
    pub asks: Vec<Vec<Value>>,
    #[serde(default)]
    pub bids: Vec<Vec<Value>>,
}

fn deserialize_ts_opt<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Value::deserialize(deserializer)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => match s.parse::<u64>() {
            Ok(v) => Some(v),
            Err(err) => {
                log_parse_drop("digifinex_orderbook", "ts", &err, &s);
                None
            }
        },
        Value::Null => None,
        other => {
            log_parse_drop(
                "digifinex_orderbook",
                "ts",
                &"unexpected ts type",
                &other.to_string(),
            );
            None
        }
    })
}

fn json_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64().filter(|v| v.is_finite()),
        Value::String(s) => s.parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    }
}

pub struct DigifinexBook<const N: usize> {
    pub instrument_id: String,
    book: ArrayOrderBook<N>,
    price_scale: f64,
    qty_scale: f64,
    qty_multiplier: f64,
    last_ts_ms: u64,
    initialized: bool,
    last_system_ts_ns: Option<Ts>,
}

impl<const N: usize> DigifinexBook<N> {
    pub const PRICE_SCALE: f64 = 100_000.0;
    pub const QTY_SCALE: f64 = 1_000_000.0;

    pub fn new(
        instrument_id: &str,
        price_scale: f64,
        qty_scale: f64,
        qty_multiplier: f64,
    ) -> Self {
        Self {
            instrument_id: instrument_id.to_string(),
            book: ArrayOrderBook::new(),
            price_scale,
            qty_scale,
            qty_multiplier,
            last_ts_ms: 0,
            initialized: false,
            last_system_ts_ns: None,
        }
    }

    #[inline(always)]
    fn conv(&self, px: f64, qty: f64) -> (Price, Qty) {
        let price = (px * self.price_scale).round() as Price;
        let qty = (qty * self.qty_multiplier * self.qty_scale).round() as Qty;
        (price, qty)
    }

    fn convert_levels(&self, levels: &[Vec<Value>]) -> Vec<(Price, Qty)> {
        levels
            .iter()
            .filter_map(|entry| {
                let px = json_f64(entry.get(0)?)?;
                let qty = json_f64(entry.get(1)?)?;
                if !px.is_finite() || px <= 0.0 {
                    log_parse_drop(
                        "digifinex_orderbook",
                        "px",
                        &"invalid px",
                        &px.to_string(),
                    );
                    return None;
                }
                if !qty.is_finite() || qty < 0.0 {
                    log_parse_drop(
                        "digifinex_orderbook",
                        "qty",
                        &"invalid qty",
                        &qty.to_string(),
                    );
                    return None;
                }
                Some(self.conv(px, qty))
            })
            .collect()
    }

    pub fn apply(&mut self, msg: &DigifinexDepthMsg) -> bool {
        if msg.event != "depth.update" {
            return false;
        }
        let ts_ms = match msg.data.timestamp {
            Some(ts) if ts > 0 => ts,
            _ => {
                log_parse_drop(
                    "digifinex_orderbook",
                    "missing_ts",
                    &"missing timestamp",
                    &msg.data.instrument_id,
                );
                return false;
            }
        };
        if self.initialized && ts_ms < self.last_ts_ms {
            return false;
        }
        let ts = ms_to_ns(ts_ms);
        let seq = ts_ms as Seq;
        self.last_system_ts_ns = Some(ts);
        let bids = self.convert_levels(&msg.data.bids);
        let asks = self.convert_levels(&msg.data.asks);

        if !self.initialized {
            if bids.is_empty() || asks.is_empty() {
                return false;
            }
            self.book.refresh_from_levels(&asks, &bids, ts, seq);
            self.last_ts_ms = ts_ms;
            self.initialized = true;
            return true;
        }

        if !bids.is_empty() && !asks.is_empty() {
            self.book.update_full_batch(&asks, &bids, ts, seq);
        } else if !bids.is_empty() {
            self.book.update_bids_batch(&bids, ts, seq);
        } else if !asks.is_empty() {
            self.book.update_asks_batch(&asks, ts, seq);
        } else {
            self.book.ts = ts;
            self.book.seq = seq;
        }
        self.last_ts_ms = ts_ms;
        true
    }

    #[inline(always)]
    pub fn last_ts(&self) -> Ts {
        self.book.ts
    }

    #[inline(always)]
    pub fn last_system_ts_ns(&self) -> Option<Ts> {
        self.last_system_ts_ns
    }

    #[inline(always)]
    pub fn mid_price_f64(&self) -> Option<f64> {
        OrderBookOps::mid_price_f64(self)
    }

    #[inline(always)]
    pub fn top_levels_f64(&self, depth: usize) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        OrderBookOps::top_levels_f64(self, depth)
    }
}

impl<const N: usize> OrderBookOps for DigifinexBook<N> {
    fn mid_price_f64(&self) -> Option<f64> {
        let bid = self.book.best_bid()?;
        let ask = self.book.best_ask()?;
        Some(((bid.px + ask.px) as f64) / (2.0 * self.price_scale))
    }

    fn top_levels_f64(&self, depth: usize) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        let mut bids = Vec::with_capacity(depth.min(self.book.len_bids()));
        let mut asks = Vec::with_capacity(depth.min(self.book.len_asks()));
        for lvl in self.book.iter_bids().take(depth) {
            bids.push((
                (lvl.px as f64) / self.price_scale,
                (lvl.qty as f64) / self.qty_scale,
            ));
        }
        for lvl in self.book.iter_asks().take(depth) {
            asks.push((
                (lvl.px as f64) / self.price_scale,
                (lvl.qty as f64) / self.qty_scale,
            ));
        }
        (bids, asks)
    }

    fn is_initialized(&self) -> bool {
        self.initialized
    }

    fn is_empty(&self) -> bool {
        self.book.is_empty()
    }

    fn best_bid_f64(&self) -> Option<(f64, f64)> {
        let b = self.book.best_bid()?;
        Some((
            (b.px as f64) / self.price_scale,
            (b.qty as f64) / self.qty_scale,
        ))
    }

    fn best_ask_f64(&self) -> Option<(f64, f64)> {
        let a = self.book.best_ask()?;
        Some((
            (a.px as f64) / self.price_scale,
            (a.qty as f64) / self.qty_scale,
        ))
    }

    fn clear(&mut self) {
        self.book.clear();
        self.initialized = false;
        self.last_ts_ms = 0;
        self.last_system_ts_ns = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{DigifinexBook, DigifinexDepthData, DigifinexDepthMsg};
    use crate::base_classes::orderbook_trait::OrderBookOps;
    use serde_json::json;

    #[test]
    fn applies_snapshot_then_qty_zero_delete() {
        let mut book = DigifinexBook::<16>::new("BTCUSDTPERP", 100_000.0, 1_000_000.0, 1.0);
        let snap = DigifinexDepthMsg {
            event: "depth.update".into(),
            data: DigifinexDepthData {
                instrument_id: "BTCUSDTPERP".into(),
                level: Some(2),
                timestamp: Some(1_000),
                asks: vec![
                    vec![json!("100.0"), json!(2.0)],
                    vec![json!("101.0"), json!(3.0)],
                ],
                bids: vec![
                    vec![json!("99.0"), json!(4.0)],
                    vec![json!("98.0"), json!(5.0)],
                ],
            },
        };
        assert!(book.apply(&snap));
        assert_eq!(book.best_bid_f64().unwrap().0, 99.0);
        let delta = DigifinexDepthMsg {
            event: "depth.update".into(),
            data: DigifinexDepthData {
                instrument_id: "BTCUSDTPERP".into(),
                level: Some(2),
                timestamp: Some(1_001),
                asks: vec![],
                bids: vec![vec![json!("99.0"), json!(0)]],
            },
        };
        assert!(book.apply(&delta));
        assert_eq!(book.best_bid_f64().unwrap().0, 98.0);
    }
}
