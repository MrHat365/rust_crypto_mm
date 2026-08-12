#![allow(dead_code)]

use crate::base_classes::order_book::ArrayOrderBook;
use crate::base_classes::orderbook_trait::OrderBookOps;
use crate::base_classes::types::*;
use crate::utils::parsing::log_parse_drop;
use crate::utils::time::ms_to_ns;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct DigiFinexDepthData {
    pub instrument_id: String,
    #[serde(default)]
    pub level: Option<u32>,
    pub timestamp: u64,
    #[serde(default)]
    pub asks: Vec<Vec<Value>>,
    #[serde(default)]
    pub bids: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DigiFinexDepthMsg {
    pub event: String,
    pub data: DigiFinexDepthData,
}

pub struct DigiFinexBook<const N: usize> {
    pub instrument_id: String,
    book: ArrayOrderBook<N>,
    price_scale: f64,
    qty_scale: f64,
    qty_multiplier: f64,
    last_seq: u64,
    initialized: bool,
    last_system_ts_ns: Option<Ts>,
}

impl<const N: usize> DigiFinexBook<N> {
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
            last_seq: 0,
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

    fn parse_level(entry: &[Value], source: &str) -> Option<(f64, f64)> {
        let px_val = entry.first()?;
        let qty_val = entry.get(1)?;
        let px = parse_finite_f64(px_val, source, "px")?;
        let qty = parse_finite_f64(qty_val, source, "qty")?;
        Some((px, qty))
    }

    #[inline(always)]
    fn convert_levels(&self, levels: &[Vec<Value>], source: &str) -> Vec<(Price, Qty)> {
        levels
            .iter()
            .filter_map(|entry| {
                let (px, qty) = Self::parse_level(entry, source)?;
                Some(self.conv(px, qty))
            })
            .collect()
    }

    /// Apply DigiFinex depth update.
    ///
    /// Live feed is incremental: first message with both sides bootstraps via refresh;
    /// later messages upsert/delete levels (`qty=0` deletes).
    pub fn apply(&mut self, msg: &DigiFinexDepthMsg) -> bool {
        if msg.event != "depth.update" {
            return false;
        }
        if msg.data.timestamp == 0 {
            log_parse_drop(
                "digifinex_orderbook",
                "missing_ts",
                &"missing timestamp",
                "",
            );
            return false;
        }
        let seq_val = msg.data.timestamp;
        if self.initialized && seq_val <= self.last_seq {
            return false;
        }

        let ts = ms_to_ns(seq_val);
        self.last_system_ts_ns = Some(ts);
        let seq: Seq = seq_val as Seq;

        let bids = self.convert_levels(&msg.data.bids, "digifinex_orderbook");
        let asks = self.convert_levels(&msg.data.asks, "digifinex_orderbook");

        if !self.initialized {
            // Bootstrap only from a two-sided payload (typically the first push).
            let non_zero_bids: Vec<_> = bids.iter().copied().filter(|(_, q)| *q > 0).collect();
            let non_zero_asks: Vec<_> = asks.iter().copied().filter(|(_, q)| *q > 0).collect();
            if non_zero_bids.is_empty() || non_zero_asks.is_empty() {
                return false;
            }
            self.book
                .refresh_from_levels(&non_zero_asks, &non_zero_bids, ts, seq);
            self.last_seq = seq_val;
            self.initialized = self.book.is_warmed_up();
            return self.initialized;
        }

        if bids.is_empty() && asks.is_empty() {
            self.book.ts = ts;
            self.book.seq = seq;
            self.last_seq = seq_val;
            return true;
        }
        if !bids.is_empty() && !asks.is_empty() {
            self.book.update_full_batch(&asks, &bids, ts, seq);
        } else if !bids.is_empty() {
            self.book.update_bids_batch(&bids, ts, seq);
        } else {
            self.book.update_asks_batch(&asks, ts, seq);
        }
        self.last_seq = seq_val;
        true
    }

    pub fn apply_rest_snapshot(
        &mut self,
        timestamp: u64,
        asks: &[(f64, f64)],
        bids: &[(f64, f64)],
    ) -> bool {
        if timestamp == 0 {
            log_parse_drop(
                "digifinex_orderbook",
                "missing_ts",
                &"missing timestamp",
                "",
            );
            return false;
        }
        if self.initialized && timestamp <= self.last_seq {
            return false;
        }
        if bids.is_empty() || asks.is_empty() {
            return false;
        }

        let ts = ms_to_ns(timestamp);
        self.last_system_ts_ns = Some(ts);
        let seq = timestamp as Seq;
        let conv_asks: Vec<(Price, Qty)> = asks
            .iter()
            .filter(|(_, q)| *q != 0.0)
            .map(|(px, qty)| self.conv(*px, *qty))
            .collect();
        let conv_bids: Vec<(Price, Qty)> = bids
            .iter()
            .filter(|(_, q)| *q != 0.0)
            .map(|(px, qty)| self.conv(*px, *qty))
            .collect();
        if conv_bids.is_empty() || conv_asks.is_empty() {
            return false;
        }

        self.book
            .refresh_from_levels(&conv_asks, &conv_bids, ts, seq);
        self.last_seq = timestamp;
        self.initialized = self.book.is_warmed_up();
        self.initialized
    }

    #[inline(always)]
    pub fn last_ts(&self) -> Ts {
        self.book.ts
    }

    #[inline(always)]
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    #[inline(always)]
    pub fn mid_price_f64(&self) -> Option<f64> {
        OrderBookOps::mid_price_f64(self)
    }

    #[inline(always)]
    pub fn top_levels_f64(&self, depth: usize) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        OrderBookOps::top_levels_f64(self, depth)
    }

    #[inline(always)]
    pub fn last_system_ts_ns(&self) -> Option<Ts> {
        self.last_system_ts_ns
    }
}

fn parse_finite_f64(value: &Value, source: &str, kind: &str) -> Option<f64> {
    match value {
        Value::Number(n) => {
            let v = n.as_f64()?;
            if v.is_finite() {
                Some(v)
            } else {
                log_parse_drop(source, kind, &"non-finite number", &n.to_string());
                None
            }
        }
        Value::String(s) => match s.parse::<f64>() {
            Ok(v) if v.is_finite() => Some(v),
            Ok(_) => {
                log_parse_drop(source, kind, &"non-finite number", s);
                None
            }
            Err(err) => {
                log_parse_drop(source, kind, &err, s);
                None
            }
        },
        _ => None,
    }
}

impl<const N: usize> OrderBookOps for DigiFinexBook<N> {
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
        self.initialized && self.book.is_warmed_up()
    }

    fn is_empty(&self) -> bool {
        self.book.is_empty()
    }

    fn best_bid_f64(&self) -> Option<(f64, f64)> {
        self.book.best_bid().map(|lvl| {
            (
                (lvl.px as f64) / self.price_scale,
                (lvl.qty as f64) / self.qty_scale,
            )
        })
    }

    fn best_ask_f64(&self) -> Option<(f64, f64)> {
        self.book.best_ask().map(|lvl| {
            (
                (lvl.px as f64) / self.price_scale,
                (lvl.qty as f64) / self.qty_scale,
            )
        })
    }

    fn clear(&mut self) {
        self.book.clear();
        self.last_seq = 0;
        self.initialized = false;
        self.last_system_ts_ns = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{DigiFinexBook, DigiFinexDepthMsg};

    #[test]
    fn depth_update_replaces_top_levels_and_ignores_zero_qty() {
        let msg: DigiFinexDepthMsg = serde_json::from_str(
            r#"{
                "event":"depth.update",
                "data":{
                    "instrument_id":"BTCUSDTPERP",
                    "level":10,
                    "timestamp":1662173255498,
                    "asks":[["19962.75",0],["19964.25",561]],
                    "bids":[["19928.54",1001]]
                }
            }"#,
        )
        .expect("depth parsed");

        let mut book = DigiFinexBook::<16>::new(
            "BTCUSDTPERP",
            DigiFinexBook::<16>::PRICE_SCALE,
            DigiFinexBook::<16>::QTY_SCALE,
            1.0,
        );
        assert!(book.apply(&msg));
        assert_eq!(book.last_seq(), 1_662_173_255_498);
        let (bids, asks) = book.top_levels_f64(5);
        assert_eq!(bids.len(), 1);
        assert_eq!(asks.len(), 1);
        assert!((bids[0].0 - 19_928.54).abs() < 1e-6);
        assert!((asks[0].0 - 19_964.25).abs() < 1e-6);
    }
}
