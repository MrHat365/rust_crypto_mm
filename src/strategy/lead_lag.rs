#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::base_classes::reference::ReferenceEvent;
use crate::base_classes::state::state;
use crate::base_classes::types::Side;
use crate::execution::{
    ClientOrderId, ExecutionReport, OrderStatus, QuoteIntent, TimeInForce,
};
use crate::strategy::simple_quote::QuoteConfig;
use crate::strategy::{FillContext, QuotePlan, QuoteStateMetrics, ReferenceMeta};

fn default_min_entry_spread_bps() -> f64 {
    3.0
}
fn default_exit_spread_bps() -> f64 {
    0.5
}
fn default_min_persist_ms() -> u64 {
    80
}
fn default_max_quote_skew_ms() -> u64 {
    250
}
fn default_max_quote_age_ms() -> u64 {
    400
}
fn default_min_leader_depth_usd() -> f64 {
    25_000.0
}
fn default_cooldown_ms() -> u64 {
    1_500
}
fn default_max_hold_ms() -> u64 {
    5_000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeadLagDirection {
    LongLagger,
    ShortLagger,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct LeadLagConfig {
    #[serde(default = "default_min_entry_spread_bps")]
    pub min_entry_spread_bps: f64,
    #[serde(default = "default_exit_spread_bps")]
    pub exit_spread_bps: f64,
    #[serde(default = "default_min_persist_ms")]
    pub min_persist_ms: u64,
    #[serde(default = "default_max_quote_skew_ms")]
    pub max_quote_skew_ms: u64,
    #[serde(default = "default_max_quote_age_ms")]
    pub max_quote_age_ms: u64,
    #[serde(default = "default_min_leader_depth_usd")]
    pub min_leader_depth_usd: f64,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
    #[serde(default = "default_max_hold_ms")]
    pub max_hold_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct Bbo {
    bid: f64,
    ask: f64,
    bid_qty: f64,
    ask_qty: f64,
    recv: Instant,
    ts_ns: Option<u64>,
}

struct ActiveQuote {
    side: Side,
    price: f64,
    placed_at: Instant,
}

pub struct LeadLagStrategy {
    cfg: LeadLagConfig,
    quote: QuoteConfig,
    base_size: f64,
    next_id: u64,
    latest_price: Option<f64>,
    latest_meta: Option<ReferenceMeta>,
    active_orders: Vec<ClientOrderId>,
    active_quotes: HashMap<ClientOrderId, ActiveQuote>,
    pending_cancels: HashSet<ClientOrderId>,
    needs_requote: bool,
    persist_dir: Option<LeadLagDirection>,
    persist_since: Option<Instant>,
    cooldown_until: Option<Instant>,
    position_opened_at: Option<Instant>,
    last_spread_bps: Option<f64>,
}

impl LeadLagStrategy {
    pub fn new(cfg: LeadLagConfig, quote: QuoteConfig, base_size: f64) -> Self {
        Self {
            cfg,
            quote,
            base_size,
            next_id: 0,
            latest_price: None,
            latest_meta: None,
            active_orders: Vec::new(),
            active_quotes: HashMap::new(),
            pending_cancels: HashSet::new(),
            needs_requote: true,
            persist_dir: None,
            persist_since: None,
            cooldown_until: None,
            position_opened_at: None,
            last_spread_bps: None,
        }
    }

    pub fn latest_price(&self) -> Option<f64> {
        self.latest_price
    }

    pub fn fill_context(&self, order_id: &ClientOrderId, now: Instant) -> FillContext {
        let order_age_ms = self
            .active_quotes
            .get(order_id)
            .map(|order| now.saturating_duration_since(order.placed_at).as_millis() as u64);
        FillContext {
            client_order_id: order_id.clone(),
            fair_mid: self.latest_price,
            lighter_mid: None,
            entry_reference_price: None,
            entry_move_bps: self.last_spread_bps,
            order_age_ms,
        }
    }

    pub fn idle_reason(&self) -> Option<String> {
        if self.cooldown_until.is_some() {
            return Some("lead_lag cooldown".to_string());
        }
        if self.last_spread_bps.is_none() {
            return Some("waiting for binance/digifinex bbo pair".to_string());
        }
        None
    }

    pub fn on_market_update(&mut self, reference: &ReferenceEvent) -> Vec<ClientOrderId> {
        self.latest_meta = Some(ReferenceMeta {
            source: reference.source.clone(),
            ts_ns: reference.ts_ns,
            received_at: reference.received_at,
        });
        let now = reference.received_at;
        let signal = self.evaluate(now);
        self.latest_price = signal
            .as_ref()
            .map(|(_, lagger)| 0.5 * (lagger.bid + lagger.ask))
            .or(self.latest_price);

        let mut cancels = Vec::new();
        let want_side = signal.map(|(dir, _)| match dir {
            LeadLagDirection::LongLagger => Side::Bid,
            LeadLagDirection::ShortLagger => Side::Ask,
        });
        let exit = self.should_exit(now, signal.as_ref().map(|(d, _)| *d));
        for (id, quote) in self.active_quotes.iter() {
            if self.pending_cancels.contains(id) {
                continue;
            }
            let side_mismatch = want_side.map(|side| side != quote.side).unwrap_or(true);
            if exit || side_mismatch {
                cancels.push(id.clone());
            }
        }
        if !cancels.is_empty() || want_side.is_some() {
            self.needs_requote = true;
        }
        for id in &cancels {
            self.pending_cancels.insert(id.clone());
        }
        cancels
    }

    pub fn plan_quotes(&mut self, now: Instant) -> Option<QuotePlan> {
        if !self.needs_requote {
            return None;
        }
        let signal = self.evaluate(now)?;
        if self.should_exit(now, Some(signal.0)) {
            return None;
        }
        let (dir, lagger) = signal;
        let (side, price) = match dir {
            LeadLagDirection::LongLagger => (Side::Bid, lagger.ask),
            LeadLagDirection::ShortLagger => (Side::Ask, lagger.bid),
        };
        if !price.is_finite() || price <= 0.0 {
            return None;
        }
        let already = self.active_quotes.values().any(|q| q.side == side);
        let mut intents = Vec::new();
        if !already {
            intents.push(QuoteIntent::new(
                self.quote.venue,
                self.quote.symbol.clone(),
                side,
                price,
                self.base_size,
                TimeInForce::Ioc,
                self.next_client_id(match side {
                    Side::Bid => "LLB",
                    Side::Ask => "LLA",
                }),
            ));
        }
        let cancels = self
            .active_orders
            .iter()
            .filter(|id| self.pending_cancels.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        if intents.is_empty() && cancels.is_empty() {
            return None;
        }
        Some(QuotePlan {
            reference_price: 0.5 * (lagger.bid + lagger.ask),
            entry_move_bps: self.last_spread_bps,
            reference_best_bid: Some(lagger.bid),
            reference_best_ask: Some(lagger.ask),
            cancels,
            intents,
            planned_at: now,
            reference_meta: self.latest_meta.clone(),
            prior_submit_at: None,
        })
    }

    pub fn commit_plan(&mut self, plan: &QuotePlan) {
        for id in &plan.cancels {
            self.pending_cancels.insert(id.clone());
        }
        for intent in &plan.intents {
            if !self
                .active_orders
                .iter()
                .any(|id| id == &intent.client_order_id)
            {
                self.active_orders.push(intent.client_order_id.clone());
            }
            self.active_quotes.insert(
                intent.client_order_id.clone(),
                ActiveQuote {
                    side: intent.side,
                    price: intent.price,
                    placed_at: plan.planned_at,
                },
            );
            self.position_opened_at = Some(plan.planned_at);
        }
        self.needs_requote = false;
    }

    pub fn rollback_plan(&mut self, plan: &QuotePlan) {
        for intent in &plan.intents {
            self.active_orders
                .retain(|id| id != &intent.client_order_id);
            self.active_quotes.remove(&intent.client_order_id);
        }
        self.needs_requote = true;
    }

    pub fn state_metrics(&self) -> QuoteStateMetrics {
        QuoteStateMetrics {
            active_orders: self.active_orders.len(),
            pending_cancels: self.pending_cancels.len(),
            needs_requote: self.needs_requote,
        }
    }

    pub fn handle_report(&mut self, report: &ExecutionReport) {
        match report.status {
            OrderStatus::Filled | OrderStatus::Canceled | OrderStatus::Rejected => {
                self.pending_cancels.remove(&report.client_order_id);
                self.active_orders
                    .retain(|id| id != &report.client_order_id);
                self.active_quotes.remove(&report.client_order_id);
                self.needs_requote = true;
                if matches!(report.status, OrderStatus::Rejected) {
                    self.cooldown_until = Some(Instant::now() + Duration::from_millis(self.cfg.cooldown_ms));
                }
                if self.active_quotes.is_empty() {
                    self.position_opened_at = None;
                }
            }
            OrderStatus::PartiallyFilled | OrderStatus::New | OrderStatus::Unknown => {
                self.pending_cancels.remove(&report.client_order_id);
            }
        }
    }

    fn evaluate(&mut self, now: Instant) -> Option<(LeadLagDirection, Bbo)> {
        if let Some(until) = self.cooldown_until {
            if now < until {
                return None;
            }
            self.cooldown_until = None;
        }
        let st = match state().lock() {
            Ok(st) => st,
            Err(poisoned) => {
                eprintln!("FATAL: state lock poisoned in LeadLagStrategy: {poisoned}");
                panic!("State lock poisoned - cannot continue safely");
            }
        };
        let leader = bbo_from_snap(&st.binance.bbo)?;
        let lagger = bbo_from_snap(&st.digifinex.bbo)?;
        drop(st);

        let skew = if leader.recv > lagger.recv {
            leader.recv.saturating_duration_since(lagger.recv)
        } else {
            lagger.recv.saturating_duration_since(leader.recv)
        };
        if skew > Duration::from_millis(self.cfg.max_quote_skew_ms) {
            return None;
        }
        let max_age = Duration::from_millis(self.cfg.max_quote_age_ms);
        if now.saturating_duration_since(leader.recv) > max_age
            || now.saturating_duration_since(lagger.recv) > max_age
        {
            return None;
        }
        if leader.bid <= 0.0 || leader.ask <= leader.bid || lagger.bid <= 0.0 || lagger.ask <= lagger.bid
        {
            return None;
        }

        let (long_bps, short_bps) = cross_spread_bps(leader, lagger)?;
        let (dir, spread_bps, leader_depth) = if long_bps >= short_bps {
            (
                LeadLagDirection::LongLagger,
                long_bps,
                leader.bid * leader.bid_qty,
            )
        } else {
            (
                LeadLagDirection::ShortLagger,
                short_bps,
                leader.ask * leader.ask_qty,
            )
        };
        self.last_spread_bps = Some(spread_bps);
        if spread_bps < self.cfg.min_entry_spread_bps {
            self.persist_dir = None;
            self.persist_since = None;
            return None;
        }
        if leader_depth < self.cfg.min_leader_depth_usd {
            return None;
        }
        match (self.persist_dir, self.persist_since) {
            (Some(prev), Some(since)) if prev == dir => {
                if now.saturating_duration_since(since)
                    < Duration::from_millis(self.cfg.min_persist_ms)
                {
                    return None;
                }
            }
            _ => {
                self.persist_dir = Some(dir);
                self.persist_since = Some(now);
                return None;
            }
        }
        Some((dir, lagger))
    }

    fn should_exit(&self, now: Instant, dir: Option<LeadLagDirection>) -> bool {
        if let Some(opened) = self.position_opened_at {
            if now.saturating_duration_since(opened) >= Duration::from_millis(self.cfg.max_hold_ms)
            {
                return true;
            }
        }
        if dir.is_none() {
            return !self.active_quotes.is_empty();
        }
        if let Some(spread) = self.last_spread_bps {
            if spread <= self.cfg.exit_spread_bps {
                return true;
            }
        }
        false
    }

    fn next_client_id(&mut self, prefix: &str) -> ClientOrderId {
        self.next_id = self.next_id.saturating_add(1);
        ClientOrderId::new(format!("{prefix}{}", self.next_id))
    }
}

fn cross_spread_bps(leader: Bbo, lagger: Bbo) -> Option<(f64, f64)> {
    let mid = 0.5 * (leader.bid + lagger.ask);
    if !(mid.is_finite() && mid > 0.0) {
        return None;
    }
    let delta_long = leader.bid - lagger.ask;
    let delta_short = lagger.bid - leader.ask;
    Some((delta_long / mid * 10_000.0, delta_short / mid * 10_000.0))
}

fn bbo_from_snap(snap: &crate::base_classes::state::FeedSnap) -> Option<Bbo> {
    let bid = snap.bid_levels[0]?;
    let ask = snap.ask_levels[0]?;
    let recv = snap.received_at?;
    if snap.seq == 0 {
        return None;
    }
    Some(Bbo {
        bid: bid.0,
        ask: ask.0,
        bid_qty: bid.1,
        ask_qty: ask.1,
        recv,
        ts_ns: snap.ts_ns,
    })
}

#[cfg(test)]
mod tests {
    use super::{Bbo, LeadLagDirection, cross_spread_bps};
    use std::time::Instant;

    fn bbo(bid: f64, ask: f64) -> Bbo {
        Bbo {
            bid,
            ask,
            bid_qty: 1.0,
            ask_qty: 1.0,
            recv: Instant::now(),
            ts_ns: Some(1),
        }
    }

    #[test]
    fn direction_labels_are_stable() {
        assert!(matches!(
            LeadLagDirection::LongLagger,
            LeadLagDirection::LongLagger
        ));
    }

    #[test]
    fn long_lagger_when_leader_bid_clears_lagger_ask() {
        let (long_bps, short_bps) = cross_spread_bps(bbo(101.0, 101.2), bbo(99.8, 100.0)).unwrap();
        assert!(long_bps > 0.0);
        assert!(long_bps > short_bps);
    }

    #[test]
    fn short_lagger_when_lagger_bid_clears_leader_ask() {
        let (long_bps, short_bps) = cross_spread_bps(bbo(99.8, 100.0), bbo(101.0, 101.2)).unwrap();
        assert!(short_bps > 0.0);
        assert!(short_bps > long_bps);
    }
}
