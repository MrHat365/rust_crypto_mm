//! Binance (leader) → DigiFinex (lagger) lead-lag taker strategy.
//!
//! Signal follows the cross-venue dislocation used by hft-lead-lag / lead-lag-arb:
//! - Long lagger when `leader.bid - lagger.ask` spread (bps) exceeds entry threshold
//! - Short lagger when `lagger.bid - leader.ask` exceeds threshold
//! Only fire when leader exchange timestamp is not behind the lagger (clock-corrected via
//! local receive ordering + optional exchange ts).

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::base_classes::reference::ReferenceEvent;
use crate::base_classes::state::state;
use crate::base_classes::types::Side;
use crate::execution::{
    ClientOrderId, ExecutionReport, OrderStatus, QuoteIntent, TimeInForce, Venue,
};
use crate::strategy::simple_quote::{QuotePlan, QuoteStateMetrics, ReferenceMeta};
use crate::strategy::FillContext;

fn default_min_entry_spread_bps() -> f64 {
    8.0
}
fn default_exit_spread_bps() -> f64 {
    1.0
}
fn default_max_position_age_ms() -> u64 {
    5_000
}
fn default_max_quote_skew_ms() -> u64 {
    1_000
}
fn default_max_quote_age_ms() -> u64 {
    2_000
}
fn default_cooldown_ms() -> u64 {
    250
}
fn default_leader_feed() -> String {
    "binance".into()
}
fn default_lagger_feed() -> String {
    "digifinex".into()
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
    #[serde(default = "default_max_position_age_ms")]
    pub max_position_age_ms: u64,
    #[serde(default = "default_max_quote_skew_ms")]
    pub max_quote_skew_ms: u64,
    #[serde(default = "default_max_quote_age_ms")]
    pub max_quote_age_ms: u64,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
    #[serde(default = "default_leader_feed")]
    pub leader_feed: String,
    #[serde(default = "default_lagger_feed")]
    pub lagger_feed: String,
}

impl Default for LeadLagConfig {
    fn default() -> Self {
        Self {
            min_entry_spread_bps: default_min_entry_spread_bps(),
            exit_spread_bps: default_exit_spread_bps(),
            max_position_age_ms: default_max_position_age_ms(),
            max_quote_skew_ms: default_max_quote_skew_ms(),
            max_quote_age_ms: default_max_quote_age_ms(),
            cooldown_ms: default_cooldown_ms(),
            leader_feed: default_leader_feed(),
            lagger_feed: default_lagger_feed(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BookQuote {
    bid: f64,
    ask: f64,
    exchange_ts_ns: Option<u64>,
    local_recv: Instant,
}

#[derive(Debug, Clone)]
struct OpenPosition {
    direction: LeadLagDirection,
    entry_spread_bps: f64,
    opened_at: Instant,
    size: f64,
}

#[derive(Debug, Clone)]
struct ActiveOrder {
    side: Side,
    reduce_only: bool,
    placed_at: Instant,
}

pub struct LeadLagStrategy {
    venue: Venue,
    symbol: String,
    min_tick: f64,
    base_size: f64,
    cfg: LeadLagConfig,
    leader: Option<BookQuote>,
    lagger: Option<BookQuote>,
    position: Option<OpenPosition>,
    next_id: u64,
    active_orders: Vec<ClientOrderId>,
    active_meta: HashMap<ClientOrderId, ActiveOrder>,
    pending_cancels: HashSet<ClientOrderId>,
    latest_price: Option<f64>,
    latest_meta: Option<ReferenceMeta>,
    last_entry_at: Option<Instant>,
    needs_action: bool,
}

impl LeadLagStrategy {
    pub fn new(
        cfg: LeadLagConfig,
        venue: Venue,
        symbol: String,
        min_tick: f64,
        base_size: f64,
    ) -> Self {
        Self {
            venue,
            symbol,
            min_tick: min_tick.max(1e-12),
            base_size,
            cfg,
            leader: None,
            lagger: None,
            position: None,
            next_id: 0,
            active_orders: Vec::new(),
            active_meta: HashMap::new(),
            pending_cancels: HashSet::new(),
            latest_price: None,
            latest_meta: None,
            last_entry_at: None,
            needs_action: true,
        }
    }

    pub fn latest_price(&self) -> Option<f64> {
        self.latest_price
    }

    pub fn idle_reason(&self) -> Option<String> {
        if self.leader.is_none() {
            return Some(format!("waiting for {} BBO", self.cfg.leader_feed));
        }
        if self.lagger.is_none() {
            return Some(format!("waiting for {} BBO", self.cfg.lagger_feed));
        }
        None
    }

    pub fn fill_context(&self, order_id: &ClientOrderId, now: Instant) -> FillContext {
        let order_age_ms = self
            .active_meta
            .get(order_id)
            .map(|o| now.saturating_duration_since(o.placed_at).as_millis() as u64);
        FillContext {
            client_order_id: order_id.clone(),
            fair_mid: self.latest_price,
            lighter_mid: None,
            entry_reference_price: None,
            entry_move_bps: self.position.as_ref().map(|p| p.entry_spread_bps),
            order_age_ms,
        }
    }

    pub fn state_metrics(&self) -> QuoteStateMetrics {
        QuoteStateMetrics {
            active_orders: self.active_orders.len(),
            pending_cancels: self.pending_cancels.len(),
            needs_requote: self.needs_action,
        }
    }

    pub fn on_market_update(&mut self, reference: &ReferenceEvent) -> Vec<ClientOrderId> {
        self.refresh_books_from_state();
        self.ingest_reference(reference);
        self.needs_action = true;
        // Lead-lag is taker-style; cancel any stale working orders on each update.
        let mut cancels = Vec::new();
        for id in self.active_orders.clone() {
            if !self.pending_cancels.contains(&id) {
                self.pending_cancels.insert(id.clone());
                cancels.push(id);
            }
        }
        cancels
    }

    pub fn plan_quotes(&mut self, now: Instant) -> Option<QuotePlan> {
        self.refresh_books_from_state();
        let leader = self.leader?;
        let lagger = self.lagger?;
        self.latest_price = Some(0.5 * (lagger.bid + lagger.ask));

        if !self.quotes_fresh(now, &leader, &lagger) {
            return None;
        }
        if !self.leader_is_leading(&leader, &lagger) {
            return None;
        }

        // Exit path
        if let Some(pos) = self.position.clone() {
            let (bid_ask_bps, ask_bid_bps) = directional_spreads(&leader, &lagger);
            let current = match pos.direction {
                LeadLagDirection::LongLagger => bid_ask_bps,
                LeadLagDirection::ShortLagger => ask_bid_bps,
            };
            let aged = now.saturating_duration_since(pos.opened_at)
                >= Duration::from_millis(self.cfg.max_position_age_ms);
            let mean_reverted = current <= self.cfg.exit_spread_bps;
            if aged || mean_reverted {
                let (side, px) = match pos.direction {
                    LeadLagDirection::LongLagger => (Side::Ask, lagger.bid),
                    LeadLagDirection::ShortLagger => (Side::Bid, lagger.ask),
                };
                let intent = self.make_intent(side, px, true, "exit");
                return Some(QuotePlan {
                    reference_price: 0.5 * (lagger.bid + lagger.ask),
                    entry_move_bps: Some(current),
                    reference_best_bid: Some(lagger.bid),
                    reference_best_ask: Some(lagger.ask),
                    cancels: Vec::new(),
                    intents: vec![intent],
                    planned_at: now,
                    reference_meta: self.latest_meta.clone(),
                    prior_submit_at: self.last_entry_at,
                });
            }
            return None;
        }

        if !self.active_orders.is_empty() {
            return None;
        }
        if let Some(last) = self.last_entry_at {
            if now.saturating_duration_since(last) < Duration::from_millis(self.cfg.cooldown_ms) {
                return None;
            }
        }

        let (bid_ask_bps, ask_bid_bps) = directional_spreads(&leader, &lagger);
        let (spread_bps, direction) = if bid_ask_bps >= ask_bid_bps {
            (bid_ask_bps, LeadLagDirection::LongLagger)
        } else {
            (ask_bid_bps, LeadLagDirection::ShortLagger)
        };
        if spread_bps < self.cfg.min_entry_spread_bps {
            return None;
        }

        let (side, px) = match direction {
            LeadLagDirection::LongLagger => (Side::Bid, lagger.ask),
            LeadLagDirection::ShortLagger => (Side::Ask, lagger.bid),
        };
        let intent = self.make_intent(side, px, false, "entry");
        Some(QuotePlan {
            reference_price: 0.5 * (lagger.bid + lagger.ask),
            entry_move_bps: Some(spread_bps),
            reference_best_bid: Some(lagger.bid),
            reference_best_ask: Some(lagger.ask),
            cancels: Vec::new(),
            intents: vec![intent],
            planned_at: now,
            reference_meta: self.latest_meta.clone(),
            prior_submit_at: self.last_entry_at,
        })
    }

    pub fn commit_plan(&mut self, plan: &QuotePlan) {
        for intent in &plan.intents {
            let reduce_only = intent.client_order_id.0.contains("exit");
            self.active_orders.push(intent.client_order_id.clone());
            self.active_meta.insert(
                intent.client_order_id.clone(),
                ActiveOrder {
                    side: intent.side,
                    reduce_only,
                    placed_at: plan.planned_at,
                },
            );
            if !reduce_only {
                let direction = match intent.side {
                    Side::Bid => LeadLagDirection::LongLagger,
                    Side::Ask => LeadLagDirection::ShortLagger,
                };
                self.position = Some(OpenPosition {
                    direction,
                    entry_spread_bps: plan.entry_move_bps.unwrap_or(0.0),
                    opened_at: plan.planned_at,
                    size: intent.size,
                });
                self.last_entry_at = Some(plan.planned_at);
            }
        }
        self.needs_action = false;
    }

    pub fn rollback_plan(&mut self, _plan: &QuotePlan) {
        self.needs_action = true;
    }

    pub fn handle_report(&mut self, report: &ExecutionReport) {
        match report.status {
            OrderStatus::Filled => {
                let meta = self.active_meta.remove(&report.client_order_id);
                self.active_orders
                    .retain(|id| id != &report.client_order_id);
                self.pending_cancels.remove(&report.client_order_id);
                if let Some(meta) = meta {
                    if meta.reduce_only {
                        self.position = None;
                    }
                }
                self.needs_action = true;
            }
            OrderStatus::Canceled | OrderStatus::Rejected => {
                let meta = self.active_meta.remove(&report.client_order_id);
                self.active_orders
                    .retain(|id| id != &report.client_order_id);
                self.pending_cancels.remove(&report.client_order_id);
                if let Some(meta) = meta {
                    if !meta.reduce_only {
                        // Entry failed/canceled before fill confirmation — drop optimistic position.
                        self.position = None;
                    }
                }
                self.needs_action = true;
            }
            _ => {}
        }
    }

    fn make_intent(
        &mut self,
        side: Side,
        price: f64,
        _reduce_only: bool,
        tag: &str,
    ) -> QuoteIntent {
        self.next_id = self.next_id.wrapping_add(1);
        let tick = self.min_tick;
        let px = match side {
            Side::Bid => (price / tick).ceil() * tick,
            Side::Ask => (price / tick).floor() * tick,
        };
        let id = ClientOrderId::new(format!(
            "ll-{}-{}-{}-{}",
            tag,
            self.symbol,
            match side {
                Side::Bid => "b",
                Side::Ask => "a",
            },
            self.next_id
        ));
        QuoteIntent::new(
            self.venue,
            self.symbol.clone(),
            side,
            px,
            self.base_size,
            TimeInForce::Ioc,
            id,
        )
    }

    fn ingest_reference(&mut self, reference: &ReferenceEvent) {
        let src = reference.source.to_ascii_lowercase();
        let bid = reference.best_bid.filter(|v| v.is_finite() && *v > 0.0);
        let ask = reference.best_ask.filter(|v| v.is_finite() && *v > 0.0);
        let (Some(bid), Some(ask)) = (bid, ask) else {
            return;
        };
        if ask < bid {
            eprintln!(
                "WARN: lead_lag crossed book from {}: bid={} ask={}",
                reference.source, bid, ask
            );
            return;
        }
        let q = BookQuote {
            bid,
            ask,
            exchange_ts_ns: reference.ts_ns,
            local_recv: reference.received_at,
        };
        if src.contains(&self.cfg.leader_feed) {
            self.leader = Some(q);
        } else if src.contains(&self.cfg.lagger_feed) {
            self.lagger = Some(q);
            self.latest_price = Some(0.5 * (bid + ask));
        }
        self.latest_meta = Some(ReferenceMeta {
            source: reference.source.clone(),
            ts_ns: reference.ts_ns,
            received_at: reference.received_at,
        });
    }

    fn refresh_books_from_state(&mut self) {
        let Ok(guard) = state().lock() else {
            eprintln!("WARN: lead_lag state lock poisoned");
            return;
        };
        let now = Instant::now();
        if let Some(q) = snap_to_quote(&guard.binance.bbo, now) {
            self.leader = Some(q);
        }
        if let Some(q) = snap_to_quote(&guard.digifinex.bbo, now) {
            self.lagger = Some(q);
            self.latest_price = Some(0.5 * (q.bid + q.ask));
        }
    }

    fn quotes_fresh(&self, now: Instant, leader: &BookQuote, lagger: &BookQuote) -> bool {
        let max_age = Duration::from_millis(self.cfg.max_quote_age_ms);
        if now.saturating_duration_since(leader.local_recv) > max_age
            || now.saturating_duration_since(lagger.local_recv) > max_age
        {
            return false;
        }
        let skew = if leader.local_recv >= lagger.local_recv {
            leader.local_recv.saturating_duration_since(lagger.local_recv)
        } else {
            lagger.local_recv.saturating_duration_since(leader.local_recv)
        };
        skew <= Duration::from_millis(self.cfg.max_quote_skew_ms)
    }

    fn leader_is_leading(&self, leader: &BookQuote, lagger: &BookQuote) -> bool {
        match (leader.exchange_ts_ns, lagger.exchange_ts_ns) {
            (Some(l), Some(g)) => l >= g,
            _ => leader.local_recv >= lagger.local_recv,
        }
    }
}

fn snap_to_quote(snap: &crate::base_classes::state::FeedSnap, fallback_now: Instant) -> Option<BookQuote> {
    let bid = snap.bid_levels[0]?.0;
    let ask = snap.ask_levels[0]?.0;
    if !(bid.is_finite() && ask.is_finite() && bid > 0.0 && ask > 0.0 && ask >= bid) {
        return None;
    }
    Some(BookQuote {
        bid,
        ask,
        exchange_ts_ns: snap.source_engine_ts_ns.or(snap.ts_ns),
        local_recv: snap.received_at.unwrap_or(fallback_now),
    })
}

fn directional_spreads(leader: &BookQuote, lagger: &BookQuote) -> (f64, f64) {
    let bid_ask = spread_bps(leader.bid, lagger.ask);
    let ask_bid = spread_bps(lagger.bid, leader.ask);
    (bid_ask, ask_bid)
}

fn spread_bps(top: f64, bottom: f64) -> f64 {
    if !(bottom.is_finite() && bottom > 0.0 && top.is_finite()) {
        return 0.0;
    }
    ((top - bottom) / bottom) * 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_leg_positive_when_leader_bid_above_lagger_ask() {
        let leader = BookQuote {
            bid: 110.0,
            ask: 111.0,
            exchange_ts_ns: Some(200),
            local_recv: Instant::now(),
        };
        let lagger = BookQuote {
            bid: 100.0,
            ask: 101.0,
            exchange_ts_ns: Some(100),
            local_recv: Instant::now(),
        };
        let (bid_ask, _) = directional_spreads(&leader, &lagger);
        assert!(bid_ask > 50.0);
    }
}
