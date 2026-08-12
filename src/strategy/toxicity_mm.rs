#![allow(dead_code)]

use std::collections::HashMap;
use std::time::Instant;

use serde::Deserialize;

use crate::base_classes::reference::ReferenceEvent;
use crate::base_classes::state::state;
use crate::base_classes::types::Side;
use crate::execution::{ClientOrderId, ExecutionReport};
use crate::strategy::microstructure::{
    AvellanedaQuote, BookTop, KyleLambda, VpinEstimator, avellaneda_stoikov_quote, best_level_ofi,
};
use crate::strategy::simple_quote::{QuoteConfig, SimpleQuoteStrategy};
use crate::strategy::{FillContext, QuotePlan, QuoteStateMetrics};

fn default_gamma() -> f64 {
    0.08
}
fn default_k() -> f64 {
    1.5
}
fn default_tau_secs() -> f64 {
    8.0
}
fn default_sigma_floor() -> f64 {
    1e-4
}
fn default_ofi_gain() -> f64 {
    0.15
}
fn default_toxicity_gain() -> f64 {
    2.0
}
fn default_vpin_bucket_usd() -> f64 {
    25_000.0
}
fn default_vpin_buckets() -> usize {
    50
}
fn default_inventory_limit() -> f64 {
    0.0
}
fn default_kyle_gain() -> f64 {
    0.5
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ToxicityMmConfig {
    #[serde(default = "default_gamma")]
    pub gamma: f64,
    #[serde(default = "default_k")]
    pub k: f64,
    #[serde(default = "default_tau_secs")]
    pub tau_secs: f64,
    #[serde(default = "default_sigma_floor")]
    pub sigma_floor: f64,
    #[serde(default = "default_ofi_gain")]
    pub ofi_gain: f64,
    #[serde(default = "default_toxicity_gain")]
    pub toxicity_gain: f64,
    #[serde(default = "default_vpin_bucket_usd")]
    pub vpin_bucket_usd: f64,
    #[serde(default = "default_vpin_buckets")]
    pub vpin_buckets: usize,
    /// Hard inventory cap in base units. 0 disables the cap.
    #[serde(default = "default_inventory_limit")]
    pub inventory_limit: f64,
    /// Extra half-spread in price units per unit Kyle λ.
    #[serde(default = "default_kyle_gain")]
    pub kyle_gain: f64,
}

pub struct ToxicityMmStrategy {
    inner: SimpleQuoteStrategy,
    cfg: ToxicityMmConfig,
    quote: QuoteConfig,
    inventory: f64,
    last_top: Option<BookTop>,
    ofi_ewma: Option<f64>,
    vpin: VpinEstimator,
    kyle: KyleLambda,
    last_trade_seq: u64,
    last_quote: Option<AvellanedaQuote>,
    order_sides: HashMap<ClientOrderId, Side>,
    filled_qty: HashMap<ClientOrderId, f64>,
}

impl ToxicityMmStrategy {
    pub fn new(cfg: ToxicityMmConfig, mut quote: QuoteConfig, base_size: f64) -> Self {
        quote.use_reference_bbo = true;
        quote.quote_at_reference_bbo = true;
        let vpin = VpinEstimator::new(cfg.vpin_bucket_usd, cfg.vpin_buckets);
        Self {
            inner: SimpleQuoteStrategy::new(quote.clone(), base_size),
            cfg,
            quote,
            inventory: 0.0,
            last_top: None,
            ofi_ewma: None,
            vpin,
            kyle: KyleLambda::new(0.2),
            last_trade_seq: 0,
            last_quote: None,
            order_sides: HashMap::new(),
            filled_qty: HashMap::new(),
        }
    }

    pub fn latest_price(&self) -> Option<f64> {
        self.inner.latest_price()
    }

    pub fn fill_context(&self, order_id: &ClientOrderId, now: Instant) -> FillContext {
        self.inner.fill_context(order_id, now)
    }

    pub fn idle_reason(&self) -> Option<String> {
        if self.last_quote.is_none() {
            return Some("waiting for valid avellaneda quote".to_string());
        }
        self.inner.idle_reason()
    }

    pub fn on_market_update(&mut self, reference: &ReferenceEvent) -> Vec<ClientOrderId> {
        self.ingest_state();
        let Some(quote) = self.compute_quote(reference) else {
            return self.inner.on_market_update(reference);
        };
        self.last_quote = Some(quote);
        let synthetic = ReferenceEvent {
            price: quote.reservation,
            best_bid: Some(quote.bid),
            best_ask: Some(quote.ask),
            ts_ns: reference.ts_ns,
            source: format!("toxicity_mm:{}", reference.source),
            received_at: reference.received_at,
        };
        self.inner.on_market_update(&synthetic)
    }

    pub fn plan_quotes(&mut self, now: Instant) -> Option<QuotePlan> {
        self.inner.plan_quotes(now)
    }

    pub fn commit_plan(&mut self, plan: &QuotePlan) {
        for intent in &plan.intents {
            self.order_sides
                .insert(intent.client_order_id.clone(), intent.side);
        }
        self.inner.commit_plan(plan);
    }

    pub fn rollback_plan(&mut self, plan: &QuotePlan) {
        self.inner.rollback_plan(plan);
    }

    pub fn state_metrics(&self) -> QuoteStateMetrics {
        self.inner.state_metrics()
    }

    pub fn handle_report(&mut self, report: &ExecutionReport) {
        if report.filled_qty.is_finite() && report.filled_qty > 0.0 {
            if let Some(side) = self.order_sides.get(&report.client_order_id).copied() {
                let prev = self
                    .filled_qty
                    .get(&report.client_order_id)
                    .copied()
                    .unwrap_or(0.0);
                let delta = report.filled_qty - prev;
                if delta.is_finite() && delta > 0.0 {
                    let signed = match side {
                        Side::Bid => delta,
                        Side::Ask => -delta,
                    };
                    self.inventory += signed;
                }
                self.filled_qty
                    .insert(report.client_order_id.clone(), report.filled_qty);
            } else {
                eprintln!(
                    "WARN: toxicity_mm fill without tracked side: order={} qty={}",
                    report.client_order_id, report.filled_qty
                );
            }
        }
        match report.status {
            crate::execution::OrderStatus::Filled
            | crate::execution::OrderStatus::Canceled
            | crate::execution::OrderStatus::Rejected => {
                self.order_sides.remove(&report.client_order_id);
                self.filled_qty.remove(&report.client_order_id);
            }
            _ => {}
        }
        self.inner.handle_report(report);
    }

    fn ingest_state(&mut self) {
        let st = match state().lock() {
            Ok(st) => st,
            Err(poisoned) => {
                eprintln!("FATAL: state lock poisoned in ToxicityMmStrategy: {poisoned}");
                panic!("State lock poisoned - cannot continue safely");
            }
        };
        let snap = if st.binance.bbo.seq > 0 {
            &st.binance
        } else if st.gate.bbo.seq > 0 {
            &st.gate
        } else {
            return;
        };
        if let (Some(bid), Some(ask)) = (snap.bbo.bid_levels[0], snap.bbo.ask_levels[0]) {
            let top = BookTop {
                bid_px: bid.0,
                bid_qty: bid.1,
                ask_px: ask.0,
                ask_qty: ask.1,
            };
            if let Some(prev) = self.last_top {
                let ofi = best_level_ofi(prev, top);
                let alpha = 0.2;
                self.ofi_ewma = Some(match self.ofi_ewma {
                    Some(cur) => alpha * ofi + (1.0 - alpha) * cur,
                    None => ofi,
                });
            }
            self.last_top = Some(top);
        }
        if snap.trade.seq != self.last_trade_seq {
            if let (Some(px), Some(qty), Some(dir)) =
                (snap.trade.price, snap.trade.size, snap.trade.direction)
            {
                let signed = match dir {
                    crate::base_classes::state::TradeDirection::Buy => px * qty,
                    crate::base_classes::state::TradeDirection::Sell => -px * qty,
                };
                self.vpin.on_trade(signed);
                if let Some(mid) = self.last_top.and_then(|t| t.mid()) {
                    let signed_qty = match dir {
                        crate::base_classes::state::TradeDirection::Buy => qty,
                        crate::base_classes::state::TradeDirection::Sell => -qty,
                    };
                    self.kyle.on_trade(mid, signed_qty);
                }
            }
            self.last_trade_seq = snap.trade.seq;
        }
    }

    fn compute_quote(&self, reference: &ReferenceEvent) -> Option<AvellanedaQuote> {
        if self.cfg.inventory_limit > 0.0 && self.inventory.abs() >= self.cfg.inventory_limit {
            eprintln!(
                "WARN: toxicity_mm inventory cap hit: inventory={} limit={}",
                self.inventory, self.cfg.inventory_limit
            );
            return None;
        }
        let top = self.last_top.or_else(|| {
            let bid = reference.best_bid?;
            let ask = reference.best_ask?;
            Some(BookTop {
                bid_px: bid,
                bid_qty: 1.0,
                ask_px: ask,
                ask_qty: 1.0,
            })
        })?;
        let micro = top.microprice().or_else(|| top.mid())?;
        let ofi = self.ofi_ewma.unwrap_or(0.0);
        let depth = (top.bid_qty + top.ask_qty).max(1e-9);
        let ofi_adj = self.cfg.ofi_gain * (ofi / depth) * micro;
        let reference_px = micro + ofi_adj;
        let toxicity = self.vpin.value().unwrap_or(0.0).clamp(0.0, 1.0);
        let gamma = self.cfg.gamma * (1.0 + self.cfg.toxicity_gain * toxicity);
        let sigma = self
            .cfg
            .sigma_floor
            .max(self.quote.min_half_spread_bps / 10_000.0);
        let min_half = micro * self.quote.min_half_spread_bps.max(0.0) / 10_000.0;
        let kyle_extra = self.cfg.kyle_gain * self.kyle.value().unwrap_or(0.0);
        avellaneda_stoikov_quote(
            reference_px,
            self.inventory,
            gamma,
            sigma,
            self.cfg.k,
            self.cfg.tau_secs,
            (min_half + kyle_extra).max(self.quote.min_tick),
        )
    }
}
