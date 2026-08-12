#![allow(dead_code)]

use crate::base_classes::order_book::ArrayOrderBook;
use crate::base_classes::orderbook_trait::OrderBookOps;
use crate::base_classes::types::*;
use crate::utils::time::ms_to_ns;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct WeexDepthMsg {
    #[serde(rename = "e")]
    pub event: String,
    #[serde(rename = "E")]
    pub event_time: u64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "U")]
    pub first_update_id: u64,
    #[serde(rename = "u")]
    pub last_update_id: u64,
    #[serde(rename = "l", default)]
    pub level: u32,
    #[serde(rename = "d", default)]
    pub depth_type: Option<String>,
    #[serde(rename = "b", default)]
    pub bids: Vec<[String; 2]>,
    #[serde(rename = "a", default)]
    pub asks: Vec<[String; 2]>,
}

pub const PRICE_SCALE: f64 = 100_000.0;
pub const QTY_SCALE: f64 = 1_000_000.0;

pub struct WeexBook<const N: usize> {
    pub symbol: String,
    book: ArrayOrderBook<N>,
    price_scale: f64,
    qty_scale: f64,
    last_seq: u64,
    initialized: bool,
    last_system_ts_ns: Option<Ts>,
}

impl<const N: usize> WeexBook<N> {
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

    pub fn apply(&mut self, msg: &WeexDepthMsg) -> bool {
        if msg.event != "depth" {
            return false;
        }
        let seq_val = msg.last_update_id;
        if seq_val == 0 {
            return false;
        }
        let ts = ms_to_ns(msg.event_time);
        self.last_system_ts_ns = Some(ts);
        let seq: Seq = seq_val as Seq;

        let bids: Vec<(Price, Qty)> = msg
            .bids
            .iter()
            .filter_map(|p| {
                let px = p[0].parse::<f64>().ok()?;
                let qty = p[1].parse::<f64>().ok()?;
                Some(self.conv(px, qty))
            })
            .collect();
        let asks: Vec<(Price, Qty)> = msg
            .asks
            .iter()
            .filter_map(|p| {
                let px = p[0].parse::<f64>().ok()?;
                let qty = p[1].parse::<f64>().ok()?;
                Some(self.conv(px, qty))
            })
            .collect();

        let is_snapshot = msg
            .depth_type
            .as_deref()
            .map(|d| d.eq_ignore_ascii_case("SNAPSHOT"))
            .unwrap_or(false);

        if !self.initialized || is_snapshot {
            if bids.is_empty() || asks.is_empty() {
                return false;
            }
            self.book.refresh_from_levels(&asks, &bids, ts, seq);
            self.last_seq = seq_val;
            self.initialized = true;
            return true;
        }

        if seq_val <= self.last_seq {
            return false;
        }

        if !bids.is_empty() && !asks.is_empty() {
            self.book.update_full_batch(&asks, &bids, ts, seq);
        } else if !bids.is_empty() {
            self.book.update_bids_batch(&bids, ts, seq);
        } else if !asks.is_empty() {
            self.book.update_asks_batch(&asks, ts, seq);
        }
        self.last_seq = seq_val;
        true
    }

    pub fn apply_bbo(
        &mut self,
        bid_px: f64,
        bid_sz: f64,
        ask_px: f64,
        ask_sz: f64,
        seq: u64,
        ts_ms: u64,
    ) -> bool {
        if !self.initialized {
            let ts = ms_to_ns(ts_ms);
            let seqn: Seq = seq as Seq;
            let (bpx, bqty) = self.conv(bid_px, bid_sz);
            let (apx, aqty) = self.conv(ask_px, ask_sz);
            self.book.refresh_from_levels(
                &[(apx, aqty)],
                &[(bpx, bqty)],
                ts,
                seqn,
            );
            self.last_seq = seq;
            self.initialized = true;
            self.last_system_ts_ns = Some(ts);
            return true;
        }
        if seq <= self.last_seq {
            return false;
        }
        let ts = ms_to_ns(ts_ms);
        self.last_system_ts_ns = Some(ts);
        let seqn: Seq = seq as Seq;
        let (bpx, bqty) = self.conv(bid_px, bid_sz);
        let (apx, aqty) = self.conv(ask_px, ask_sz);
        if let Some(best_b) = self.book.best_bid() {
            if bpx == best_b.px {
                self.book.upsert_bid(bpx, bqty, ts, seqn);
            } else if bpx > best_b.px {
                self.book.upsert_bid(bpx, bqty, ts, seqn);
                self.book.trim_asks_at_or_below(bpx);
            }
        } else {
            self.book.upsert_bid(bpx, bqty, ts, seqn);
        }
        if let Some(best_a) = self.book.best_ask() {
            if apx == best_a.px {
                self.book.upsert_ask(apx, aqty, ts, seqn);
            } else if apx < best_a.px {
                self.book.upsert_ask(apx, aqty, ts, seqn);
                self.book.trim_bids_at_or_above(apx);
            }
        } else {
            self.book.upsert_ask(apx, aqty, ts, seqn);
        }
        self.last_seq = seq;
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

impl<const N: usize> OrderBookOps for WeexBook<N> {
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
