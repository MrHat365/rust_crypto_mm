#![allow(dead_code)]

use crate::base_classes::order_book::ArrayOrderBook;
use crate::base_classes::orderbook_trait::OrderBookOps;
use crate::base_classes::types::*;
use crate::exchanges::weex::rest::{WeexSnapshot, fetch_depth_snapshot};
use crate::utils::parsing::log_parse_drop;
use crate::utils::time::ms_to_ns;
use serde::Deserialize;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
pub struct WeexDepthMsg {
    pub e: String,
    #[serde(rename = "E")]
    pub event_time: u64,
    pub s: String,
    #[serde(rename = "U")]
    pub first_update_id: u64,
    pub u: u64,
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
    last_update_id: u64,
    last_u: Option<u64>,
    first_valid_processed: bool,
    last_system_ts_ns: Option<Ts>,
    last_resync: Option<Instant>,
}

impl<const N: usize> WeexBook<N> {
    pub const PRICE_SCALE: f64 = 100_000.0;
    pub const QTY_SCALE: f64 = 1_000_000.0;
    const RESYNC_COOLDOWN: Duration = Duration::from_secs(1);

    pub fn new(symbol: &str, price_scale: f64, qty_scale: f64) -> Self {
        Self {
            symbol: symbol.to_ascii_uppercase(),
            book: ArrayOrderBook::new(),
            price_scale,
            qty_scale,
            last_update_id: 0,
            last_u: None,
            first_valid_processed: false,
            last_system_ts_ns: None,
            last_resync: None,
        }
    }

    pub async fn init_from_rest(
        &mut self,
        limit: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let snap = fetch_depth_snapshot(&self.symbol, limit)
            .await?
            .ok_or_else(|| format!("WEEX depth snapshot missing for {}", self.symbol))?;
        self.apply_snapshot(&snap);
        Ok(())
    }

    pub fn apply_snapshot(&mut self, snap: &WeexSnapshot) {
        let ts = 0;
        let seq = snap.last_update_id as Seq;
        let bids: Vec<(Price, Qty)> = snap
            .bids
            .iter()
            .take(N)
            .map(|(px, qty)| self.conv(*px, *qty))
            .collect();
        let asks: Vec<(Price, Qty)> = snap
            .asks
            .iter()
            .take(N)
            .map(|(px, qty)| self.conv(*px, *qty))
            .collect();
        self.book.refresh_from_levels(&asks, &bids, ts, seq);
        self.last_update_id = snap.last_update_id;
        self.last_u = None;
        self.first_valid_processed = false;
        eprintln!(
            "weex depth snapshot loaded: symbol={}, last_update_id={}, bids={}, asks={}",
            self.symbol,
            snap.last_update_id,
            bids.len(),
            asks.len()
        );
    }

    #[inline(always)]
    fn conv(&self, px: f64, qty: f64) -> (Price, Qty) {
        (
            (px * self.price_scale).round() as Price,
            (qty * self.qty_scale).round() as Qty,
        )
    }

    fn convert_levels(&self, levels: &[Vec<String>]) -> Vec<(Price, Qty)> {
        levels
            .iter()
            .filter_map(|entry| {
                let px_str = entry.get(0)?;
                let qty_str = entry.get(1)?;
                let px = match px_str.parse::<f64>() {
                    Ok(v) if v.is_finite() && v > 0.0 => v,
                    Ok(_) => {
                        log_parse_drop("weex_orderbook", "px", &"invalid px", px_str);
                        return None;
                    }
                    Err(err) => {
                        log_parse_drop("weex_orderbook", "px", &err, px_str);
                        return None;
                    }
                };
                let qty = match qty_str.parse::<f64>() {
                    Ok(v) if v.is_finite() && v >= 0.0 => v,
                    Ok(_) => {
                        log_parse_drop("weex_orderbook", "qty", &"invalid qty", qty_str);
                        return None;
                    }
                    Err(err) => {
                        log_parse_drop("weex_orderbook", "qty", &err, qty_str);
                        return None;
                    }
                };
                Some(self.conv(px, qty))
            })
            .collect()
    }

    pub fn apply_depth_update(&mut self, d: &WeexDepthMsg) -> bool {
        if d.e != "depth" {
            return false;
        }
        if d.u < self.last_update_id {
            return false;
        }
        if !self.first_valid_processed {
            if !(d.first_update_id <= self.last_update_id.saturating_add(1) && d.u >= self.last_update_id)
            {
                self.try_resync("initial diff missing target id", d);
                return false;
            }
            self.first_valid_processed = true;
        } else {
            let prev_u = match self.last_u {
                Some(prev) => prev,
                None => {
                    eprintln!(
                        "weex depth reject [missing prev_u]: symbol={}, last_id={}, U={}, u={}",
                        self.symbol, self.last_update_id, d.first_update_id, d.u
                    );
                    return false;
                }
            };
            if d.first_update_id != prev_u.saturating_add(1) && d.first_update_id != prev_u {
                self.try_resync("incremental U gap", d);
                return false;
            }
        }

        self.last_u = Some(d.u);
        self.last_update_id = d.u;
        let ts = ms_to_ns(d.event_time);
        self.last_system_ts_ns = Some(ts);
        let seq = d.u as Seq;
        let bids = self.convert_levels(&d.b);
        let asks = self.convert_levels(&d.a);
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
        true
    }

    fn try_resync(&mut self, reason: &str, d: &WeexDepthMsg) {
        let now = Instant::now();
        if let Some(last) = self.last_resync {
            if now.duration_since(last) < Self::RESYNC_COOLDOWN {
                return;
            }
        }
        self.last_resync = Some(now);
        eprintln!(
            "weex depth resync: symbol={}, reason={}, last_id={}, U={}, u={}",
            self.symbol, reason, self.last_update_id, d.first_update_id, d.u
        );
        match tokio::runtime::Runtime::new() {
            Ok(rt) => {
                if let Err(err) = rt.block_on(self.init_from_rest(200)) {
                    eprintln!(
                        "weex depth resync failed: symbol={}, err={}",
                        self.symbol, err
                    );
                }
            }
            Err(err) => {
                eprintln!(
                    "weex depth resync runtime init failed: symbol={}, err={}",
                    self.symbol, err
                );
            }
        }
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
        self.first_valid_processed
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
        self.first_valid_processed = false;
        self.last_update_id = 0;
        self.last_u = None;
        self.last_system_ts_ns = None;
    }
}
