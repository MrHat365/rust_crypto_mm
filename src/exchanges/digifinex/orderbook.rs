#![allow(dead_code)]

use crate::base_classes::order_book::ArrayOrderBook;
use crate::base_classes::orderbook_trait::OrderBookOps;
use crate::base_classes::types::*;
use crate::utils::time::ms_to_ns;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct DigifinexDepthData {
    pub instrument_id: String,
    #[serde(default)]
    pub level: u32,
    pub timestamp: u64,
    #[serde(default)]
    pub asks: Vec<[serde_json::Value; 2]>,
    #[serde(default)]
    pub bids: Vec<[serde_json::Value; 2]>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DigifinexDepthMsg {
    pub event: String,
    pub data: DigifinexDepthData,
}

pub const PRICE_SCALE: f64 = 100_000.0;
pub const QTY_SCALE: f64 = 1_000_000.0;

pub struct DigifinexBook<const N: usize> {
    pub symbol: String,
    book: ArrayOrderBook<N>,
    price_scale: f64,
    qty_scale: f64,
    last_seq: u64,
    initialized: bool,
    last_system_ts_ns: Option<Ts>,
}

impl<const N: usize> DigifinexBook<N> {
    pub fn new(symbol: &str, price_scale: f64, qty_scale: f64) -> Self {
        Self {
            symbol: symbol.to_string(),
            book: ArrayOrderBook::new(),
            price_scale,
            qty_scale,
            last_seq: 0,
            initialized: false,
            last_system_ts_ns: None,
        }
    }

    #[inline(always)]
    fn conv(&self, px: f64, qty: f64) -> (Price, Qty) {
        let price = (px * self.price_scale).round() as Price;
        let qty = (qty * self.qty_scale).round() as Qty;
        (price, qty)
    }

    fn parse_level(entry: &[serde_json::Value; 2]) -> Option<(f64, f64)> {
        let px = match &entry[0] {
            serde_json::Value::String(s) => s.parse::<f64>().ok()?,
            serde_json::Value::Number(n) => n.as_f64()?,
            _ => return None,
        };
        let qty = match &entry[1] {
            serde_json::Value::String(s) => s.parse::<f64>().ok()?,
            serde_json::Value::Number(n) => n.as_f64()?,
            _ => return None,
        };
        if px.is_finite() && qty.is_finite() {
            Some((px, qty))
        } else {
            None
        }
    }

    pub fn apply(&mut self, msg: &DigifinexDepthMsg) -> bool {
        let d = &msg.data;
        let seq_val = d.timestamp;
        if seq_val == 0 {
            return false;
        }
        if seq_val <= self.last_seq && self.initialized {
            return false;
        }
        let ts = ms_to_ns(seq_val);
        self.last_system_ts_ns = Some(ts);
        let seq: Seq = seq_val as Seq;

        let bids: Vec<(Price, Qty)> = d
            .bids
            .iter()
            .filter_map(|lvl| Self::parse_level(lvl).map(|(px, q)| self.conv(px, q)))
            .collect();
        let asks: Vec<(Price, Qty)> = d
            .asks
            .iter()
            .filter_map(|lvl| Self::parse_level(lvl).map(|(px, q)| self.conv(px, q)))
            .collect();

        if bids.is_empty() || asks.is_empty() {
            return false;
        }

        self.book.refresh_from_levels(&asks, &bids, ts, seq);
        self.last_seq = seq_val;
        self.initialized = true;
        true
    }

    #[inline(always)]
    pub fn mid_price_f64(&self) -> Option<f64> {
        let b = self.book.best_bid()?;
        let a = self.book.best_ask()?;
        Some(((b.px + a.px) as f64) / (2.0 * self.price_scale))
    }

    #[inline(always)]
    pub fn last_ts(&self) -> Ts {
        self.book.ts
    }

    #[inline(always)]
    pub fn last_system_ts_ns(&self) -> Option<Ts> {
        self.last_system_ts_ns
    }

    pub fn apply_bbo_from_ticker(
        &mut self,
        bid_px: f64,
        bid_sz: f64,
        ask_px: f64,
        ask_sz: f64,
        ts_ms: u64,
    ) -> bool {
        if ts_ms == 0 {
            return false;
        }
        if ts_ms <= self.last_seq && self.initialized {
            return false;
        }
        let ts = ms_to_ns(ts_ms);
        self.last_system_ts_ns = Some(ts);
        let seq: Seq = ts_ms as Seq;
        let (bpx, bqty) = self.conv(bid_px, bid_sz);
        let (apx, aqty) = self.conv(ask_px, ask_sz);
        if !self.initialized {
            self.book
                .refresh_from_levels(&[(apx, aqty)], &[(bpx, bqty)], ts, seq);
            self.initialized = true;
        } else {
            self.book.upsert_bid(bpx, bqty, ts, seq);
            self.book.upsert_ask(apx, aqty, ts, seq);
        }
        self.last_seq = ts_ms;
        true
    }

    #[inline(always)]
    pub fn top_levels_f64(&self, depth: usize) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
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
}

impl<const N: usize> OrderBookOps for DigifinexBook<N> {
    fn mid_price_f64(&self) -> Option<f64> {
        self.mid_price_f64()
    }

    fn top_levels_f64(&self, depth: usize) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        self.top_levels_f64(depth)
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
        self.last_seq = 0;
        self.last_system_ts_ns = None;
    }
}
