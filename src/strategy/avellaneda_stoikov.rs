//! Avellaneda-Stoikov optimal market making with inventory skew and toxicity widening.
//!
//! Reference: https://github.com/Felooo8/hft-market-making-avellaneda-stoikov

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::base_classes::reference::ReferenceEvent;
use crate::base_classes::types::Side;
use crate::execution::{
    ClientOrderId, ExecutionReport, OrderStatus, QuoteIntent, TimeInForce, Venue,
};
use crate::strategy::orderflow_toxicity::OrderflowToxicityTracker;
use crate::strategy::{FillContext, QuotePlan, QuoteStateMetrics, ReferenceMeta};

fn default_gamma() -> f64 {
    0.1
}
fn default_k() -> f64 {
    1.5
}
fn default_horizon_secs() -> f64 {
    60.0
}
fn default_inventory_limit() -> f64 {
    5.0
}
fn default_min_half_spread_bps() -> f64 {
    5.0
}
fn default_max_widen_bps() -> f64 {
    15.0
}
fn default_quote_interval_ms() -> u64 {
    200
}
fn default_vol_ewma_alpha() -> f64 {
    0.2
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct AvellanedaStoikovConfig {
    #[serde(default = "default_gamma")]
    pub gamma: f64,
    #[serde(default = "default_k")]
    pub k: f64,
    #[serde(default = "default_horizon_secs")]
    pub horizon_secs: f64,
    #[serde(default = "default_inventory_limit")]
    pub inventory_limit: f64,
    #[serde(default = "default_min_half_spread_bps")]
    pub min_half_spread_bps: f64,
    #[serde(default = "default_max_widen_bps")]
    pub max_toxicity_widen_bps: f64,
    #[serde(default = "default_quote_interval_ms")]
    pub quote_interval_ms: u64,
    #[serde(default = "default_vol_ewma_alpha")]
    pub volatility_ewma_alpha: f64,
    #[serde(default)]
    pub use_microprice: bool,
    #[serde(default)]
    pub toxicity_window: Option<usize>,
    #[serde(default)]
    pub toxicity_volume_bucket: Option<f64>,
}

#[derive(Debug, Clone)]
struct ActiveQuote {
    side: Side,
    price: f64,
    placed_at: Instant,
}

pub struct AvellanedaStoikovStrategy {
    venue: Venue,
    symbol: String,
    size: f64,
    min_tick: f64,
    config: AvellanedaStoikovConfig,
    next_id: u64,
    inventory: f64,
    last_mid: Option<f64>,
    vol_ewma: f64,
    toxicity: OrderflowToxicityTracker,
    active_orders: Vec<ClientOrderId>,
    active_quotes: HashMap<ClientOrderId, ActiveQuote>,
    pending_cancels: HashSet<ClientOrderId>,
    needs_quote: bool,
    last_quote_at: Option<Instant>,
    latest_meta: Option<ReferenceMeta>,
    latest_best_bid: Option<f64>,
    latest_best_ask: Option<f64>,
}

impl AvellanedaStoikovStrategy {
    pub fn new(
        config: AvellanedaStoikovConfig,
        venue: Venue,
        symbol: String,
        min_tick: f64,
        size: f64,
    ) -> Self {
        let window = config.toxicity_window.unwrap_or(32);
        let bucket = config.toxicity_volume_bucket.unwrap_or(10_000.0);
        Self {
            venue,
            symbol,
            size,
            min_tick,
            config,
            next_id: 0,
            inventory: 0.0,
            last_mid: None,
            vol_ewma: 0.0,
            toxicity: OrderflowToxicityTracker::new(window, bucket),
            active_orders: Vec::new(),
            active_quotes: HashMap::new(),
            pending_cancels: HashSet::new(),
            needs_quote: true,
            last_quote_at: None,
            latest_meta: None,
            latest_best_bid: None,
            latest_best_ask: None,
        }
    }

    fn next_client_id(&mut self, tag: &str) -> ClientOrderId {
        self.next_id += 1;
        ClientOrderId::new(format!("as-{tag}-{}", self.next_id))
    }

    fn reservation_price(&self, reference: f64, sigma: f64) -> f64 {
        let tau = self.config.horizon_secs;
        reference - self.inventory * self.config.gamma * sigma * sigma * tau
    }

    fn optimal_half_spread(&self, sigma: f64, mid: f64) -> f64 {
        let tau = self.config.horizon_secs;
        let gamma = self.config.gamma;
        let k = self.config.k.max(1e-6);
        let inventory_term = gamma * sigma * sigma * tau;
        let flow_term = if gamma > 0.0 {
            (2.0 / gamma) * (1.0 + gamma / k).ln()
        } else {
            2.0 / k
        };
        let half = 0.5 * (inventory_term + flow_term);
        let min_half = self.config.min_half_spread_bps / 10_000.0 * mid;
        half.max(min_half)
    }

    fn round_to_tick(&self, px: f64) -> f64 {
        let tick = self.min_tick.max(1e-8);
        (px / tick).round() * tick
    }

    pub fn on_market_update(&mut self, reference: &ReferenceEvent) -> Vec<ClientOrderId> {
        let mid = reference.price;
        self.latest_best_bid = reference.best_bid;
        self.latest_best_ask = reference.best_ask;
        if let (Some(bid), Some(ask)) = (reference.best_bid, reference.best_ask) {
            self.toxicity.update_book(bid, 1.0, ask, 1.0);
        }
        if let Some(prev) = self.last_mid {
            let ret = ((mid / prev) - 1.0).abs();
            let alpha = self.config.volatility_ewma_alpha;
            self.vol_ewma = alpha * ret + (1.0 - alpha) * self.vol_ewma;
        }
        self.last_mid = Some(mid);
        self.latest_meta = Some(ReferenceMeta {
            source: reference.source.clone(),
            ts_ns: reference.ts_ns,
            received_at: reference.received_at,
        });
        self.needs_quote = true;
        Vec::new()
    }

    pub fn plan_quotes(&mut self, now: Instant) -> Option<QuotePlan> {
        if !self.needs_quote {
            return None;
        }
        if let Some(last) = self.last_quote_at {
            if now.duration_since(last) < Duration::from_millis(self.config.quote_interval_ms) {
                return None;
            }
        }
        let mid = self.last_mid?;
        let sigma = self.vol_ewma.max(1e-6);
        let reference = if self.config.use_microprice {
            match (self.latest_best_bid, self.latest_best_ask) {
                (Some(b), Some(a)) => (b + a) * 0.5,
                _ => mid,
            }
        } else {
            mid
        };
        let reservation = self.reservation_price(reference, sigma);
        let mut half_spread = self.optimal_half_spread(sigma, mid);
        let widen = self
            .toxicity
            .spread_widen_bps(self.config.min_half_spread_bps, self.config.max_toxicity_widen_bps);
        half_spread += (widen / 10_000.0) * mid;

        let inv_ratio = (self.inventory / self.config.inventory_limit).clamp(-1.0, 1.0);
        let skew = inv_ratio * half_spread * 0.5;
        let bid_px = self.round_to_tick(reservation - half_spread - skew);
        let ask_px = self.round_to_tick(reservation + half_spread - skew);
        if ask_px <= bid_px {
            return None;
        }

        let mut intents = Vec::new();
        if self.inventory < self.config.inventory_limit {
            intents.push(QuoteIntent::new(
                self.venue,
                self.symbol.clone(),
                Side::Bid,
                bid_px,
                self.size,
                TimeInForce::PostOnly,
                self.next_client_id("B"),
            ));
        }
        if self.inventory > -self.config.inventory_limit {
            intents.push(QuoteIntent::new(
                self.venue,
                self.symbol.clone(),
                Side::Ask,
                ask_px,
                self.size,
                TimeInForce::PostOnly,
                self.next_client_id("S"),
            ));
        }
        if intents.is_empty() {
            return None;
        }
        let cancels = self.active_orders.clone();
        self.last_quote_at = Some(now);
        Some(QuotePlan {
            reference_price: mid,
            entry_move_bps: None,
            reference_best_bid: self.latest_best_bid,
            reference_best_ask: self.latest_best_ask,
            cancels,
            intents,
            planned_at: now,
            reference_meta: self.latest_meta.clone(),
            prior_submit_at: self.last_quote_at,
        })
    }

    pub fn commit_plan(&mut self, plan: &QuotePlan) {
        for id in &plan.cancels {
            self.pending_cancels.insert(id.clone());
        }
        for intent in &plan.intents {
            self.active_orders.push(intent.client_order_id.clone());
            self.active_quotes.insert(
                intent.client_order_id.clone(),
                ActiveQuote {
                    side: intent.side,
                    price: intent.price,
                    placed_at: Instant::now(),
                },
            );
        }
        self.needs_quote = false;
    }

    pub fn rollback_plan(&mut self, _plan: &QuotePlan) {
        self.needs_quote = true;
    }

    pub fn handle_report(&mut self, report: &ExecutionReport) {
        if report.filled_qty > 0.0 {
            if let Some(px) = report.avg_fill_price {
                let is_sell = self
                    .active_quotes
                    .get(&report.client_order_id)
                    .map(|q| q.side == Side::Ask)
                    .unwrap_or(false);
                self.toxicity.record_trade(px, report.filled_qty, is_sell);
                let signed = if is_sell {
                    -report.filled_qty
                } else {
                    report.filled_qty
                };
                self.inventory += signed;
            }
        }
        if matches!(
            report.status,
            OrderStatus::Filled | OrderStatus::Canceled | OrderStatus::Rejected
        ) {
            self.active_orders
                .retain(|id| id != &report.client_order_id);
            self.active_quotes.remove(&report.client_order_id);
            self.pending_cancels.remove(&report.client_order_id);
        }
    }

    pub fn state_metrics(&self) -> QuoteStateMetrics {
        QuoteStateMetrics {
            active_orders: self.active_orders.len(),
            pending_cancels: self.pending_cancels.len(),
            needs_requote: self.needs_quote,
        }
    }

    pub fn latest_price(&self) -> Option<f64> {
        self.last_mid
    }

    pub fn fill_context(&self, order_id: &ClientOrderId, now: Instant) -> FillContext {
        let order_age_ms = self
            .active_quotes
            .get(order_id)
            .map(|q| now.duration_since(q.placed_at).as_millis() as u64);
        FillContext {
            client_order_id: order_id.clone(),
            fair_mid: self.last_mid,
            lighter_mid: None,
            entry_reference_price: self.last_mid,
            entry_move_bps: None,
            order_age_ms,
        }
    }

    pub fn idle_reason(&self) -> Option<String> {
        if self.last_mid.is_none() {
            Some("waiting for reference".into())
        } else if !self.needs_quote {
            Some("quotes up to date".into())
        } else {
            None
        }
    }
}
