#![allow(dead_code)]

use std::time::Instant;

use serde::Deserialize;

use crate::base_classes::reference::ReferenceEvent;
use crate::execution::{ClientOrderId, ExecutionReport};

pub mod lead_lag;
pub mod microstructure;
pub mod momentum_fade;
pub mod simple_quote;
pub mod toxicity_mm;

pub use lead_lag::{LeadLagConfig, LeadLagStrategy};
pub use momentum_fade::{EntryPriceSource, MomentumFadeConfig, MomentumFadeStrategy};
pub use simple_quote::{
    QuoteConfig, QuotePlan, QuoteStateMetrics, ReferenceMeta, SimpleQuoteStrategy, SizeSpec,
};
pub use toxicity_mm::{ToxicityMmConfig, ToxicityMmStrategy};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyKind {
    SimpleQuote,
    MomentumFade,
    ToxicityMm,
    LeadLag,
}

impl Default for StrategyKind {
    fn default() -> Self {
        StrategyKind::SimpleQuote
    }
}

impl StrategyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StrategyKind::SimpleQuote => "simple_quote",
            StrategyKind::MomentumFade => "momentum_fade",
            StrategyKind::ToxicityMm => "toxicity_mm",
            StrategyKind::LeadLag => "lead_lag",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FillContext {
    pub client_order_id: ClientOrderId,
    pub fair_mid: Option<f64>,
    pub lighter_mid: Option<f64>,
    pub entry_reference_price: Option<f64>,
    pub entry_move_bps: Option<f64>,
    pub order_age_ms: Option<u64>,
}

pub enum StrategyEngine {
    Simple(SimpleQuoteStrategy),
    Momentum(MomentumFadeStrategy),
    Toxicity(ToxicityMmStrategy),
    LeadLag(LeadLagStrategy),
}

impl StrategyEngine {
    pub fn on_market_update(&mut self, reference: &ReferenceEvent) -> Vec<ClientOrderId> {
        match self {
            StrategyEngine::Simple(strategy) => {
                if reference.source == "lighter_orderbook" {
                    Vec::new()
                } else {
                    strategy.on_market_update(reference)
                }
            }
            StrategyEngine::Momentum(strategy) => strategy.on_market_update(reference),
            StrategyEngine::Toxicity(strategy) => strategy.on_market_update(reference),
            StrategyEngine::LeadLag(strategy) => strategy.on_market_update(reference),
        }
    }

    pub fn plan_quotes(&mut self, now: Instant) -> Option<QuotePlan> {
        match self {
            StrategyEngine::Simple(strategy) => strategy.plan_quotes(now),
            StrategyEngine::Momentum(strategy) => strategy.plan_quotes(now),
            StrategyEngine::Toxicity(strategy) => strategy.plan_quotes(now),
            StrategyEngine::LeadLag(strategy) => strategy.plan_quotes(now),
        }
    }

    pub fn commit_plan(&mut self, plan: &QuotePlan) {
        match self {
            StrategyEngine::Simple(strategy) => strategy.commit_plan(plan),
            StrategyEngine::Momentum(strategy) => strategy.commit_plan(plan),
            StrategyEngine::Toxicity(strategy) => strategy.commit_plan(plan),
            StrategyEngine::LeadLag(strategy) => strategy.commit_plan(plan),
        }
    }

    pub fn rollback_plan(&mut self, plan: &QuotePlan) {
        match self {
            StrategyEngine::Simple(strategy) => strategy.rollback_plan(plan),
            StrategyEngine::Momentum(strategy) => strategy.rollback_plan(plan),
            StrategyEngine::Toxicity(strategy) => strategy.rollback_plan(plan),
            StrategyEngine::LeadLag(strategy) => strategy.rollback_plan(plan),
        }
    }

    pub fn state_metrics(&self) -> QuoteStateMetrics {
        match self {
            StrategyEngine::Simple(strategy) => strategy.state_metrics(),
            StrategyEngine::Momentum(strategy) => strategy.state_metrics(),
            StrategyEngine::Toxicity(strategy) => strategy.state_metrics(),
            StrategyEngine::LeadLag(strategy) => strategy.state_metrics(),
        }
    }

    pub fn handle_report(&mut self, report: &ExecutionReport) {
        match self {
            StrategyEngine::Simple(strategy) => strategy.handle_report(report),
            StrategyEngine::Momentum(strategy) => strategy.handle_report(report),
            StrategyEngine::Toxicity(strategy) => strategy.handle_report(report),
            StrategyEngine::LeadLag(strategy) => strategy.handle_report(report),
        }
    }

    pub fn latest_price(&self) -> Option<f64> {
        match self {
            StrategyEngine::Simple(strategy) => strategy.latest_price(),
            StrategyEngine::Momentum(strategy) => strategy.latest_price(),
            StrategyEngine::Toxicity(strategy) => strategy.latest_price(),
            StrategyEngine::LeadLag(strategy) => strategy.latest_price(),
        }
    }

    pub fn fill_context(&self, order_id: &ClientOrderId, now: Instant) -> FillContext {
        match self {
            StrategyEngine::Simple(strategy) => strategy.fill_context(order_id, now),
            StrategyEngine::Momentum(strategy) => strategy.fill_context(order_id, now),
            StrategyEngine::Toxicity(strategy) => strategy.fill_context(order_id, now),
            StrategyEngine::LeadLag(strategy) => strategy.fill_context(order_id, now),
        }
    }

    pub fn idle_reason(&self) -> Option<String> {
        match self {
            StrategyEngine::Simple(strategy) => strategy.idle_reason(),
            StrategyEngine::Momentum(strategy) => strategy.idle_reason(),
            StrategyEngine::Toxicity(strategy) => strategy.idle_reason(),
            StrategyEngine::LeadLag(strategy) => strategy.idle_reason(),
        }
    }
}
