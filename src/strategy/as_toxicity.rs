//! Inventory-aware Avellaneda–Stoikov market making with order-flow toxicity skew.
//!
//! Combines:
//! - classical AS reservation price / optimal spread
//! - Cont–Kukanov–Stoikov style OFI / microprice toxicity adjustments
//! - short-horizon signed-volume (VPIN-lite) wideners
//!
//! Execution still goes through the shared QuotePlan / post-only path.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::base_classes::reference::ReferenceEvent;
use crate::base_classes::state::state;
use crate::base_classes::types::Side;
use crate::execution::{
    ClientOrderId, ExecutionReport, OrderStatus, QuoteIntent, TimeInForce, Venue,
};
use crate::strategy::simple_quote::{QuoteConfig, QuotePlan, QuoteStateMetrics, ReferenceMeta};
use crate::strategy::FillContext;

const DEFAULT_GAMMA: f64 = 0.1;
const DEFAULT_K: f64 = 1.5;
const DEFAULT_TAU_SECS: f64 = 30.0;
const DEFAULT_INVENTORY_LIMIT: f64 = 5.0;
const DEFAULT_TOXICITY_WINDOW: usize = 32;
const DEFAULT_OFI_SKEW_BPS: f64 = 2.0;
const DEFAULT_SIGNED_VOL_WIDEN_BPS: f64 = 3.0;
const DEFAULT_MICROPRICE_BLEND: f64 = 0.5;

fn default_gamma() -> f64 {
    DEFAULT_GAMMA
}
fn default_k() -> f64 {
    DEFAULT_K
}
fn default_tau_secs() -> f64 {
    DEFAULT_TAU_SECS
}
fn default_inventory_limit() -> f64 {
    DEFAULT_INVENTORY_LIMIT
}
fn default_toxicity_window() -> usize {
    DEFAULT_TOXICITY_WINDOW
}
fn default_ofi_skew_bps() -> f64 {
    DEFAULT_OFI_SKEW_BPS
}
fn default_signed_vol_widen_bps() -> f64 {
    DEFAULT_SIGNED_VOL_WIDEN_BPS
}
fn default_microprice_blend() -> f64 {
    DEFAULT_MICROPRICE_BLEND
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct AsToxicityConfig {
    #[serde(default = "default_gamma")]
    pub gamma: f64,
    #[serde(default = "default_k")]
    pub k: f64,
    #[serde(default = "default_tau_secs")]
    pub tau_secs: f64,
    #[serde(default = "default_inventory_limit")]
    pub inventory_limit: f64,
    #[serde(default = "default_toxicity_window")]
    pub toxicity_window: usize,
    #[serde(default = "default_ofi_skew_bps")]
    pub ofi_skew_bps: f64,
    #[serde(default = "default_signed_vol_widen_bps")]
    pub signed_vol_widen_bps: f64,
    #[serde(default = "default_microprice_blend")]
    pub microprice_blend: f64,
}

impl Default for AsToxicityConfig {
    fn default() -> Self {
        Self {
            gamma: DEFAULT_GAMMA,
            k: DEFAULT_K,
            tau_secs: DEFAULT_TAU_SECS,
            inventory_limit: DEFAULT_INVENTORY_LIMIT,
            toxicity_window: DEFAULT_TOXICITY_WINDOW,
            ofi_skew_bps: DEFAULT_OFI_SKEW_BPS,
            signed_vol_widen_bps: DEFAULT_SIGNED_VOL_WIDEN_BPS,
            microprice_blend: DEFAULT_MICROPRICE_BLEND,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BookTouch {
    bid: f64,
    ask: f64,
    bid_sz: f64,
    ask_sz: f64,
}

#[derive(Debug, Clone)]
struct ActiveQuote {
    side: Side,
    price: f64,
    placed_at: Instant,
}

pub struct AsToxicityStrategy {
    quote: QuoteConfig,
    as_cfg: AsToxicityConfig,
    base_size: f64,
    inventory: f64,
    next_id: u64,
    active_orders: Vec<ClientOrderId>,
    active_quotes: HashMap<ClientOrderId, ActiveQuote>,
    pending_cancels: HashSet<ClientOrderId>,
    latest_price: Option<f64>,
    latest_best_bid: Option<f64>,
    latest_best_ask: Option<f64>,
    latest_meta: Option<ReferenceMeta>,
    needs_requote: bool,
    last_mid: Option<f64>,
    ewma_var: Option<f64>,
    prev_touch: Option<BookTouch>,
    ofi_hist: VecDeque<f64>,
    signed_vol_hist: VecDeque<f64>,
    last_plan_at: Option<Instant>,
}

impl AsToxicityStrategy {
    pub fn new(quote: QuoteConfig, as_cfg: AsToxicityConfig, base_size: f64) -> Self {
        let window = as_cfg.toxicity_window.max(1);
        Self {
            quote,
            as_cfg,
            base_size,
            inventory: 0.0,
            next_id: 0,
            active_orders: Vec::new(),
            active_quotes: HashMap::new(),
            pending_cancels: HashSet::new(),
            latest_price: None,
            latest_best_bid: None,
            latest_best_ask: None,
            latest_meta: None,
            needs_requote: true,
            last_mid: None,
            ewma_var: None,
            prev_touch: None,
            ofi_hist: VecDeque::with_capacity(window),
            signed_vol_hist: VecDeque::with_capacity(window),
            last_plan_at: None,
        }
    }

    pub fn latest_price(&self) -> Option<f64> {
        self.latest_price
    }

    pub fn idle_reason(&self) -> Option<String> {
        if !self.needs_requote {
            return None;
        }
        if self.latest_price.is_none() {
            return Some("waiting for first reference price update".to_string());
        }
        None
    }

    pub fn fill_context(&self, order_id: &ClientOrderId, now: Instant) -> FillContext {
        let order_age_ms = self
            .active_quotes
            .get(order_id)
            .map(|o| now.saturating_duration_since(o.placed_at).as_millis() as u64);
        FillContext {
            client_order_id: order_id.clone(),
            fair_mid: self.latest_price,
            lighter_mid: None,
            entry_reference_price: None,
            entry_move_bps: None,
            order_age_ms,
        }
    }

    pub fn state_metrics(&self) -> QuoteStateMetrics {
        QuoteStateMetrics {
            active_orders: self.active_orders.len(),
            pending_cancels: self.pending_cancels.len(),
            needs_requote: self.needs_requote,
        }
    }

    pub fn on_market_update(&mut self, reference: &ReferenceEvent) -> Vec<ClientOrderId> {
        let price = reference.price;
        if !price.is_finite() || price <= 0.0 {
            return Vec::new();
        }
        self.latest_price = Some(price);
        if let Some(b) = reference.best_bid.filter(|v| v.is_finite() && *v > 0.0) {
            self.latest_best_bid = Some(b);
        }
        if let Some(a) = reference.best_ask.filter(|v| v.is_finite() && *v > 0.0) {
            self.latest_best_ask = Some(a);
        }
        self.latest_meta = Some(ReferenceMeta {
            source: reference.source.clone(),
            ts_ns: reference.ts_ns,
            received_at: reference.received_at,
        });
        self.update_vol(price);
        self.update_toxicity_from_state();
        self.needs_requote = true;

        // Cancel immediately if inventory hard-capped against that side or quotes are crossed vs touch.
        let mut cancels = Vec::new();
        let inv_limit = self.as_cfg.inventory_limit;
        for id in self.active_orders.clone() {
            if self.pending_cancels.contains(&id) {
                continue;
            }
            let Some(q) = self.active_quotes.get(&id) else {
                continue;
            };
            let should = match q.side {
                Side::Bid => self.inventory >= inv_limit,
                Side::Ask => self.inventory <= -inv_limit,
            };
            if should {
                self.pending_cancels.insert(id.clone());
                cancels.push(id);
            }
        }
        cancels
    }

    pub fn plan_quotes(&mut self, now: Instant) -> Option<QuotePlan> {
        if !self.needs_requote {
            return None;
        }
        let mid = self.latest_price?;
        if let Some(prev) = self.last_plan_at {
            if now.saturating_duration_since(prev)
                < Duration::from_millis(self.quote.quote_interval_ms)
            {
                return None;
            }
        }

        let sigma = self.sigma();
        let tau = self.as_cfg.tau_secs.max(1e-6);
        let gamma = self.as_cfg.gamma.max(0.0);
        let k = self.as_cfg.k.max(1e-9);
        let inv = self
            .inventory
            .clamp(-self.as_cfg.inventory_limit, self.as_cfg.inventory_limit);

        let reference = self.reference_price(mid);
        let reservation = reference - inv * gamma * sigma * sigma * tau;
        let as_half = if gamma == 0.0 {
            1.0 / k
        } else {
            0.5 * (gamma * sigma * sigma * tau + (2.0 / gamma) * (1.0 + gamma / k).ln())
        };
        let fee_half = mid * (self.quote.fee_bps + self.quote.venue_buffer_bps) * 1e-4;
        let toxicity_widen = self.signed_vol_toxicity().abs()
            * mid
            * self.as_cfg.signed_vol_widen_bps
            * 1e-4;
        let min_half = mid * self.quote.min_half_spread_bps * 1e-4;
        let half = as_half.max(fee_half).max(min_half) + toxicity_widen;
        let ofi_skew = self.ofi_signal() * mid * self.as_cfg.ofi_skew_bps * 1e-4;

        let mut bid = reservation - half - ofi_skew;
        let mut ask = reservation + half - ofi_skew;
        let tick = self.quote.min_tick.max(1e-12);
        bid = (bid / tick).floor() * tick;
        ask = (ask / tick).ceil() * tick;
        if ask <= bid {
            ask = bid + tick;
        }
        if let (Some(bb), Some(ba)) = (self.latest_best_bid, self.latest_best_ask) {
            bid = bid.min(ba - tick);
            ask = ask.max(bb + tick);
        }

        let mut cancels = Vec::new();
        for id in &self.active_orders {
            if !self.pending_cancels.contains(id) {
                cancels.push(id.clone());
                self.pending_cancels.insert(id.clone());
            }
        }

        let mut intents = Vec::new();
        if inv < self.as_cfg.inventory_limit && bid.is_finite() && bid > 0.0 {
            intents.push(self.make_intent(Side::Bid, bid));
        }
        if inv > -self.as_cfg.inventory_limit && ask.is_finite() && ask > 0.0 {
            intents.push(self.make_intent(Side::Ask, ask));
        }
        if intents.is_empty() && cancels.is_empty() {
            return None;
        }

        Some(QuotePlan {
            reference_price: reference,
            entry_move_bps: Some(self.ofi_signal() * self.as_cfg.ofi_skew_bps),
            reference_best_bid: self.latest_best_bid,
            reference_best_ask: self.latest_best_ask,
            cancels,
            intents,
            planned_at: now,
            reference_meta: self.latest_meta.clone(),
            prior_submit_at: self.last_plan_at,
        })
    }

    pub fn commit_plan(&mut self, plan: &QuotePlan) {
        for id in &plan.cancels {
            self.active_orders.retain(|x| x != id);
            self.active_quotes.remove(id);
            self.pending_cancels.remove(id);
        }
        for intent in &plan.intents {
            self.active_orders.push(intent.client_order_id.clone());
            self.active_quotes.insert(
                intent.client_order_id.clone(),
                ActiveQuote {
                    side: intent.side,
                    price: intent.price,
                    placed_at: plan.planned_at,
                },
            );
        }
        self.needs_requote = false;
        self.last_plan_at = Some(plan.planned_at);
    }

    pub fn rollback_plan(&mut self, plan: &QuotePlan) {
        for id in &plan.cancels {
            self.pending_cancels.remove(id);
        }
        self.needs_requote = true;
    }

    pub fn handle_report(&mut self, report: &ExecutionReport) {
        match report.status {
            OrderStatus::Filled | OrderStatus::Canceled | OrderStatus::Rejected => {
                self.active_orders
                    .retain(|id| id != &report.client_order_id);
                if let Some(q) = self.active_quotes.remove(&report.client_order_id) {
                    if matches!(report.status, OrderStatus::Filled)
                        && report.filled_qty.is_finite()
                        && report.filled_qty > 0.0
                    {
                        match q.side {
                            Side::Bid => self.inventory += report.filled_qty,
                            Side::Ask => self.inventory -= report.filled_qty,
                        }
                    }
                }
                self.pending_cancels.remove(&report.client_order_id);
                self.needs_requote = true;
            }
            OrderStatus::PartiallyFilled => {
                if report.filled_qty.is_finite() && report.filled_qty > 0.0 {
                    if let Some(q) = self.active_quotes.get(&report.client_order_id) {
                        match q.side {
                            Side::Bid => self.inventory += report.filled_qty,
                            Side::Ask => self.inventory -= report.filled_qty,
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn make_intent(&mut self, side: Side, price: f64) -> QuoteIntent {
        self.next_id = self.next_id.wrapping_add(1);
        let id = ClientOrderId::new(format!(
            "astox-{}-{}-{}",
            self.quote.symbol,
            match side {
                Side::Bid => "b",
                Side::Ask => "a",
            },
            self.next_id
        ));
        QuoteIntent::new(
            self.quote.venue,
            self.quote.symbol.clone(),
            side,
            price,
            self.base_size,
            TimeInForce::PostOnly,
            id,
        )
    }

    fn reference_price(&self, mid: f64) -> f64 {
        let blend = self.as_cfg.microprice_blend.clamp(0.0, 1.0);
        if blend <= 0.0 {
            return mid;
        }
        let Some(micro) = self.microprice() else {
            return mid;
        };
        (1.0 - blend) * mid + blend * micro
    }

    fn microprice(&self) -> Option<f64> {
        let bid = self.latest_best_bid?;
        let ask = self.latest_best_ask?;
        let touch = self.current_touch()?;
        let den = touch.bid_sz + touch.ask_sz;
        if den <= 0.0 {
            return Some(0.5 * (bid + ask));
        }
        Some((ask * touch.bid_sz + bid * touch.ask_sz) / den)
    }

    fn current_touch(&self) -> Option<BookTouch> {
        let guard = state().lock().ok()?;
        let snap = match self.quote.venue {
            Venue::Gate => &guard.gate.bbo,
            Venue::Lighter => &guard.lighter.bbo,
            Venue::Digifinex => &guard.digifinex.bbo,
        };
        let bid = snap.bid_levels[0]?;
        let ask = snap.ask_levels[0]?;
        if bid.0 <= 0.0 || ask.0 <= 0.0 {
            return None;
        }
        Some(BookTouch {
            bid: bid.0,
            ask: ask.0,
            bid_sz: bid.1.max(0.0),
            ask_sz: ask.1.max(0.0),
        })
    }

    fn update_toxicity_from_state(&mut self) {
        if let Some(touch) = self.current_touch() {
            if let Some(prev) = self.prev_touch {
                let ofi = cont_ofi(&prev, &touch);
                let window = self.as_cfg.toxicity_window;
                Self::push_hist(&mut self.ofi_hist, ofi, window);
            }
            self.prev_touch = Some(touch);
        }

        // Signed volume from venue trade snap if available.
        let signed = {
            let Ok(guard) = state().lock() else {
                return;
            };
            let trade = match self.quote.venue {
                Venue::Gate => &guard.gate.trade,
                Venue::Lighter => &guard.lighter.trade,
                Venue::Digifinex => &guard.digifinex.trade,
            };
            match (trade.price, trade.size, trade.direction) {
                (Some(px), Some(sz), Some(dir)) if px.is_finite() && sz.is_finite() && sz > 0.0 => {
                    let sign = match dir {
                        crate::base_classes::state::TradeDirection::Buy => 1.0,
                        crate::base_classes::state::TradeDirection::Sell => -1.0,
                    };
                    Some(sign * sz)
                }
                _ => None,
            }
        };
        if let Some(sv) = signed {
            let window = self.as_cfg.toxicity_window;
            Self::push_hist(&mut self.signed_vol_hist, sv, window);
        }
    }

    fn push_hist(hist: &mut VecDeque<f64>, value: f64, window: usize) {
        if !value.is_finite() {
            return;
        }
        hist.push_back(value);
        while hist.len() > window.max(1) {
            hist.pop_front();
        }
    }

    fn ofi_signal(&self) -> f64 {
        if self.ofi_hist.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.ofi_hist.iter().sum();
        let mean = sum / self.ofi_hist.len() as f64;
        mean.tanh()
    }

    fn signed_vol_toxicity(&self) -> f64 {
        if self.signed_vol_hist.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.signed_vol_hist.iter().sum();
        let abs_sum: f64 = self.signed_vol_hist.iter().map(|v| v.abs()).sum();
        if abs_sum <= 1e-12 {
            return 0.0;
        }
        (sum / abs_sum).clamp(-1.0, 1.0)
    }

    fn update_vol(&mut self, mid: f64) {
        if let Some(prev) = self.last_mid {
            if prev > 0.0 && mid > 0.0 {
                let ret = (mid / prev).ln();
                let alpha = self.quote.volatility_ewma_alpha.clamp(1e-4, 1.0);
                self.ewma_var = Some(match self.ewma_var {
                    Some(v) => (1.0 - alpha) * v + alpha * ret * ret,
                    None => ret * ret,
                });
            }
        }
        self.last_mid = Some(mid);
    }

    fn sigma(&self) -> f64 {
        self.ewma_var
            .map(|v| v.max(0.0).sqrt())
            .unwrap_or(1e-4)
            .max(1e-6)
    }
}

/// Cont–Kukanov–Stoikov best-level OFI contribution for one touch update.
fn cont_ofi(prev: &BookTouch, cur: &BookTouch) -> f64 {
    let bid_contrib = if cur.bid > prev.bid {
        cur.bid_sz
    } else if cur.bid < prev.bid {
        -prev.bid_sz
    } else {
        cur.bid_sz - prev.bid_sz
    };
    let ask_contrib = if cur.ask < prev.ask {
        cur.ask_sz
    } else if cur.ask > prev.ask {
        -prev.ask_sz
    } else {
        cur.ask_sz - prev.ask_sz
    };
    bid_contrib - ask_contrib
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ofi_increases_on_bid_size_add() {
        let prev = BookTouch {
            bid: 100.0,
            ask: 100.1,
            bid_sz: 1.0,
            ask_sz: 1.0,
        };
        let cur = BookTouch {
            bid: 100.0,
            ask: 100.1,
            bid_sz: 2.0,
            ask_sz: 1.0,
        };
        assert!(cont_ofi(&prev, &cur) > 0.0);
    }
}
