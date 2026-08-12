//! Order-flow toxicity metrics inspired by VPIN / order-flow imbalance research.
//!
//! References:
//! - https://github.com/Priyaanshu-Patel/orderflow-toxicity
//! - https://github.com/Felooo8/hft-orderflow-alpha

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, Default)]
pub struct ToxicitySnapshot {
    pub ofi: f64,
    pub signed_volume: f64,
    pub vpin: f64,
    pub spread_bps: f64,
    pub toxicity_score: f64,
}

#[derive(Debug, Clone)]
pub struct OrderflowToxicityTracker {
    window: usize,
    volume_bucket: f64,
    trades: VecDeque<(f64, f64)>, // (signed_qty, notional)
    buckets: VecDeque<f64>,
    last_bid_px: f64,
    last_ask_px: f64,
    last_bid_qty: f64,
    last_ask_qty: f64,
}

impl OrderflowToxicityTracker {
    pub fn new(window: usize, volume_bucket: f64) -> Self {
        Self {
            window: window.max(8),
            volume_bucket: volume_bucket.max(1.0),
            trades: VecDeque::new(),
            buckets: VecDeque::new(),
            last_bid_px: 0.0,
            last_ask_px: 0.0,
            last_bid_qty: 0.0,
            last_ask_qty: 0.0,
        }
    }

    pub fn update_book(&mut self, bid_px: f64, bid_qty: f64, ask_px: f64, ask_qty: f64) -> f64 {
        self.last_bid_px = bid_px;
        self.last_ask_px = ask_px;
        self.last_bid_qty = bid_qty;
        self.last_ask_qty = ask_qty;
        self.ofi()
    }

    pub fn record_trade(&mut self, price: f64, qty: f64, is_buyer_maker: bool) {
        let signed = if is_buyer_maker { -qty } else { qty };
        self.trades.push_back((signed, price * qty.abs()));
        while self.trades.len() > self.window * 4 {
            self.trades.pop_front();
        }
        self.update_vpin_buckets(signed, price);
    }

    fn update_vpin_buckets(&mut self, signed_qty: f64, price: f64) {
        let mut remaining = signed_qty.abs() * price;
        if remaining <= 0.0 {
            return;
        }
        let mut imbalance = signed_qty.signum();
        while remaining > 0.0 {
            let take = remaining.min(self.volume_bucket);
            self.buckets.push_back(imbalance * take);
            remaining -= take;
            while self.buckets.len() > self.window {
                self.buckets.pop_front();
            }
        }
    }

    pub fn ofi(&self) -> f64 {
        if self.last_bid_px <= 0.0 || self.last_ask_px <= 0.0 {
            return 0.0;
        }
        let bid_contrib = self.last_bid_qty;
        let ask_contrib = self.last_ask_qty;
        let denom = bid_contrib + ask_contrib;
        if denom <= 0.0 {
            0.0
        } else {
            (bid_contrib - ask_contrib) / denom
        }
    }

    pub fn vpin(&self) -> f64 {
        if self.buckets.is_empty() {
            return 0.0;
        }
        let sum_abs: f64 = self.buckets.iter().map(|b| b.abs()).sum();
        let net: f64 = self.buckets.iter().sum();
        if sum_abs <= 0.0 {
            0.0
        } else {
            (net.abs() / sum_abs).clamp(0.0, 1.0)
        }
    }

    pub fn spread_bps(&self) -> f64 {
        if self.last_bid_px <= 0.0 || self.last_ask_px <= 0.0 {
            return 0.0;
        }
        let mid = (self.last_bid_px + self.last_ask_px) * 0.5;
        if mid <= 0.0 {
            0.0
        } else {
            (self.last_ask_px - self.last_bid_px) / mid * 10_000.0
        }
    }

    pub fn snapshot(&self) -> ToxicitySnapshot {
        let ofi = self.ofi();
        let vpin = self.vpin();
        let spread_bps = self.spread_bps();
        let signed_volume: f64 = self.trades.iter().map(|(q, _)| q).sum();
        let toxicity_score = (vpin * 0.5 + ofi.abs() * 0.3 + (spread_bps / 100.0).min(1.0) * 0.2)
            .clamp(0.0, 1.0);
        ToxicitySnapshot {
            ofi,
            signed_volume,
            vpin,
            spread_bps,
            toxicity_score,
        }
    }

    /// Widen half-spread in bps when toxicity is elevated.
    pub fn spread_widen_bps(&self, base_half_spread_bps: f64, max_widen_bps: f64) -> f64 {
        let snap = self.snapshot();
        base_half_spread_bps + snap.toxicity_score * max_widen_bps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toxicity_increases_with_imbalance() {
        let mut tracker = OrderflowToxicityTracker::new(16, 1000.0);
        tracker.update_book(100.0, 10.0, 100.1, 1.0);
        let low = tracker.snapshot().toxicity_score;
        tracker.update_book(100.0, 1.0, 100.1, 10.0);
        let high = tracker.snapshot().toxicity_score;
        assert!(high >= low);
    }
}
