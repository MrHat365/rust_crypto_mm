#![allow(dead_code)]

use crate::base_classes::order_book::ArrayOrderBook;
use crate::base_classes::orderbook_trait::OrderBookOps;
use crate::base_classes::types::*;
use crate::utils::parsing::log_parse_drop;
use crate::utils::time::ms_to_ns;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct WeexDepthMsg {
    pub e: String,
    #[serde(rename = "E")]
    pub event_time: u64,
    pub s: String,
    #[serde(rename = "U")]
    pub first_update_id: u64,
    pub u: u64,
    pub l: u64,
    /// `SNAPSHOT` on initial push, `CHANGED` on incremental updates.
    pub d: String,
    #[serde(default)]
    pub b: Vec<Vec<String>>,
    #[serde(default)]
    pub a: Vec<Vec<String>>,
}

pub struct WeexBook<const N: usize> {
    pub symbol: String,
    book: ArrayOrderBook<N>,
    price_scale: f64,
    qty_scale: f64,
    qty_multiplier: f64,
    last_u: u64,
    initialized: bool,
    last_system_ts_ns: Option<Ts>,
}

impl<const N: usize> WeexBook<N> {
    pub const PRICE_SCALE: f64 = 100_000.0;
    pub const QTY_SCALE: f64 = 1_000_000.0;

    pub fn new(symbol: &str, price_scale: f64, qty_scale: f64, qty_multiplier: f64) -> Self {
        Self {
            symbol: symbol.to_string(),
            book: ArrayOrderBook::new(),
            price_scale,
            qty_scale,
            qty_multiplier,
            last_u: 0,
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

    #[inline(always)]
    fn parse_level(&self, entry: &[String]) -> Option<(Price, Qty)> {
        let px_str = entry.get(0)?;
        let qty_str = entry.get(1)?;
        let px = match px_str.parse::<f64>() {
            Ok(v) if v.is_finite() => v,
            Ok(_) => {
                log_parse_drop("weex_orderbook", "non_finite_px", &"non-finite px", px_str);
                return None;
            }
            Err(err) => {
                log_parse_drop("weex_orderbook", "px", &err, px_str);
                return None;
            }
        };
        let qty = match qty_str.parse::<f64>() {
            Ok(v) if v.is_finite() => v,
            Ok(_) => {
                log_parse_drop("weex_orderbook", "non_finite_qty", &"non-finite qty", qty_str);
                return None;
            }
            Err(err) => {
                log_parse_drop("weex_orderbook", "qty", &err, qty_str);
                return None;
            }
        };
        Some(self.conv(px, qty))
    }

    #[inline(always)]
    fn convert_levels(&self, levels: &[Vec<String>]) -> Vec<(Price, Qty)> {
        levels
            .iter()
            .filter_map(|entry| self.parse_level(entry))
            .collect()
    }

    #[inline(always)]
    fn clear_on_gap(&mut self, reason: &str, msg: &WeexDepthMsg) {
        eprintln!(
            "weex depth gap [{}]: symbol={}, last_u={}, event_U={}, event_u={}, depth_type={}",
            reason,
            self.symbol,
            self.last_u,
            msg.first_update_id,
            msg.u,
            msg.d
        );
        self.book.clear();
        self.last_u = 0;
        self.initialized = false;
        self.last_system_ts_ns = None;
    }

    /// Apply a WEEX depth message. Returns true when the local book changed.
    pub fn apply(&mut self, msg: &WeexDepthMsg) -> bool {
        if msg.e != "depth" {
            return false;
        }
        if msg.u == 0 {
            return false;
        }

        let ts = ms_to_ns(msg.event_time);
        self.last_system_ts_ns = Some(ts);
        let seq: Seq = msg.u as Seq;
        let bids = self.convert_levels(&msg.b);
        let asks = self.convert_levels(&msg.a);
        let is_snapshot = msg.d.eq_ignore_ascii_case("SNAPSHOT");

        if !self.initialized {
            if is_snapshot || msg.d.eq_ignore_ascii_case("CHANGED") {
                if bids.is_empty() || asks.is_empty() {
                    return false;
                }
                self.book.refresh_from_levels(&asks, &bids, ts, seq);
                self.last_u = msg.u;
                self.initialized = true;
                return true;
            }
            return false;
        }

        if is_snapshot {
            if bids.is_empty() || asks.is_empty() {
                return false;
            }
            self.book.refresh_from_levels(&asks, &bids, ts, seq);
            self.last_u = msg.u;
            return true;
        }

        if !msg.d.eq_ignore_ascii_case("CHANGED") {
            log_parse_drop(
                "weex_orderbook",
                "depth_type",
                &format!("unknown depth type: {}", msg.d),
                "",
            );
            return false;
        }

        if msg.u <= self.last_u {
            return false;
        }

        // WEEX contract depth IDs are not always strictly contiguous across merges.
        // Contiguous/overlap: apply incremental. Hard gap: refresh if both sides present,
        // otherwise clear and wait for a usable snapshot-like update.
        let expected = self.last_u.saturating_add(1);
        if msg.first_update_id > expected {
            if !bids.is_empty() && !asks.is_empty() {
                eprintln!(
                    "WARN: weex depth gap recovered via refresh: symbol={} last_u={} U={} u={}",
                    self.symbol, self.last_u, msg.first_update_id, msg.u
                );
                self.book.refresh_from_levels(&asks, &bids, ts, seq);
                self.last_u = msg.u;
                return true;
            }
            self.clear_on_gap("U/u sequence gap", msg);
            return false;
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

        self.last_u = msg.u;
        true
    }

    #[inline(always)]
    pub fn last_ts(&self) -> Ts {
        self.book.ts
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

    #[inline(always)]
    pub fn is_initialized(&self) -> bool {
        self.initialized && self.book.is_warmed_up()
    }
}

impl<const N: usize> OrderBookOps for WeexBook<N> {
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
        self.last_u = 0;
        self.initialized = false;
        self.last_system_ts_ns = None;
    }
}
