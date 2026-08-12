//! Lead-lag arbitrage: Binance (leader) vs Digifinex (follower).
//!
//! Reference: https://github.com/MrHat365/lead-lag-arb
//!            https://github.com/MrHat365/hft-lead-lag

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::base_classes::reference::ReferenceEvent;
use crate::base_classes::state::state;
use crate::base_classes::types::Side;
use crate::execution::{
    ClientOrderId, ExecutionReport, OrderStatus, QuoteIntent, TimeInForce, Venue,
};
use crate::strategy::{FillContext, QuotePlan, QuoteStateMetrics, ReferenceMeta};

fn default_entry_threshold_bps() -> f64 {
    3.0
}
fn default_exit_threshold_bps() -> f64 {
    0.5
}
fn default_max_position() -> f64 {
    3.0
}
fn default_min_interval_ms() -> u64 {
    100
}
fn default_signal_persist_ms() -> u64 {
    150
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LeadLagDirection {
    Long,
    Short,
    Flat,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct LeadLagConfig {
    pub leader_source_prefix: String,
    pub follower_venue: Venue,
    pub follower_symbol: String,
    #[serde(default = "default_entry_threshold_bps")]
    pub entry_threshold_bps: f64,
    #[serde(default = "default_exit_threshold_bps")]
    pub exit_threshold_bps: f64,
    #[serde(default = "default_max_position")]
    pub max_position: f64,
    #[serde(default = "default_min_interval_ms")]
    pub min_interval_ms: u64,
    #[serde(default = "default_signal_persist_ms")]
    pub signal_persist_ms: u64,
    #[serde(default)]
    pub min_tick: Option<f64>,
    #[serde(default)]
    pub order_size: Option<f64>,
}

#[derive(Debug, Clone)]
struct ActiveOrder {
    side: Side,
    price: f64,
    placed_at: Instant,
}

pub struct LeadLagStrategy {
    config: LeadLagConfig,
    size: f64,
    min_tick: f64,
    next_id: u64,
    position: f64,
    leader_bid: Option<f64>,
    leader_ask: Option<f64>,
    follower_bid: Option<f64>,
    follower_ask: Option<f64>,
    signal: LeadLagDirection,
    signal_since: Option<Instant>,
    last_submit_at: Option<Instant>,
    needs_quote: bool,
    active_orders: Vec<ClientOrderId>,
    active_quotes: HashMap<ClientOrderId, ActiveOrder>,
    pending_cancels: HashSet<ClientOrderId>,
    latest_meta: Option<ReferenceMeta>,
}

impl LeadLagStrategy {
    pub fn new(config: LeadLagConfig, base_size: f64, min_tick: f64) -> Self {
        Self {
            size: config.order_size.unwrap_or(base_size),
            min_tick,
            config,
            next_id: 0,
            position: 0.0,
            leader_bid: None,
            leader_ask: None,
            follower_bid: None,
            follower_ask: None,
            signal: LeadLagDirection::Flat,
            signal_since: None,
            last_submit_at: None,
            needs_quote: false,
            active_orders: Vec::new(),
            active_quotes: HashMap::new(),
            pending_cancels: HashSet::new(),
            latest_meta: None,
        }
    }

    fn next_client_id(&mut self, tag: &str) -> ClientOrderId {
        self.next_id += 1;
        ClientOrderId::new(format!("ll-{tag}-{}", self.next_id))
    }

    fn round_to_tick(&self, px: f64) -> f64 {
        let tick = self.min_tick.max(1e-8);
        (px / tick).round() * tick
    }

    fn refresh_follower_bbo(&mut self) {
        let st = match state().lock() {
            Ok(st) => st,
            Err(_) => return,
        };
        let snap = &st.digifinex.bbo;
        self.follower_bid = snap.bid_levels[0].map(|lvl| lvl.0);
        self.follower_ask = snap.ask_levels[0].map(|lvl| lvl.0);
    }

    fn compute_signal(&self) -> LeadLagDirection {
        let (lb, la, fb, fa) = match (
            self.leader_bid,
            self.leader_ask,
            self.follower_bid,
            self.follower_ask,
        ) {
            (Some(lb), Some(la), Some(fb), Some(fa)) => (lb, la, fb, fa),
            _ => return LeadLagDirection::Flat,
        };
        let delta_long = lb - fa;
        let delta_short = fb - la;
        let mid = (lb + fa) * 0.5;
        if mid <= 0.0 {
            return LeadLagDirection::Flat;
        }
        let long_bps = delta_long / mid * 10_000.0;
        let short_bps = delta_short / mid * 10_000.0;
        if long_bps >= self.config.entry_threshold_bps {
            LeadLagDirection::Long
        } else if short_bps >= self.config.entry_threshold_bps {
            LeadLagDirection::Short
        } else if self.position.abs() > 0.0 {
            let exit_bps = if self.position > 0.0 {
                long_bps
            } else {
                short_bps
            };
            if exit_bps <= self.config.exit_threshold_bps {
                LeadLagDirection::Flat
            } else if self.position > 0.0 {
                LeadLagDirection::Long
            } else {
                LeadLagDirection::Short
            }
        } else {
            LeadLagDirection::Flat
        }
    }

    pub fn on_market_update(&mut self, reference: &ReferenceEvent) -> Vec<ClientOrderId> {
        let source = reference.source.as_str();
        if source.starts_with(&self.config.leader_source_prefix)
            || source.contains("binance")
        {
            self.leader_bid = reference.best_bid;
            self.leader_ask = reference.best_ask;
            self.latest_meta = Some(ReferenceMeta {
                source: reference.source.clone(),
                ts_ns: reference.ts_ns,
                received_at: reference.received_at,
            });
        }
        self.refresh_follower_bbo();
        let new_signal = self.compute_signal();
        let now = reference.received_at;
        if new_signal != self.signal {
            self.signal = new_signal;
            self.signal_since = Some(now);
        }
        if self.signal != LeadLagDirection::Flat {
            if let Some(since) = self.signal_since {
                if now.duration_since(since)
                    >= Duration::from_millis(self.config.signal_persist_ms)
                {
                    self.needs_quote = true;
                }
            }
        } else if self.position.abs() > 0.0 {
            self.needs_quote = true;
        }
        Vec::new()
    }

    pub fn plan_quotes(&mut self, now: Instant) -> Option<QuotePlan> {
        if !self.needs_quote {
            return None;
        }
        if let Some(last) = self.last_submit_at {
            if now.duration_since(last) < Duration::from_millis(self.config.min_interval_ms) {
                return None;
            }
        }
        if !self.active_orders.is_empty() {
            return None;
        }

        let intent = match self.signal {
            LeadLagDirection::Long if self.position < self.config.max_position => {
                let ask = self.follower_ask?;
                Some((
                    Side::Bid,
                    self.round_to_tick(ask),
                    self.config.follower_venue,
                    self.config.follower_symbol.clone(),
                ))
            }
            LeadLagDirection::Short if self.position > -self.config.max_position => {
                let bid = self.follower_bid?;
                Some((
                    Side::Ask,
                    self.round_to_tick(bid),
                    self.config.follower_venue,
                    self.config.follower_symbol.clone(),
                ))
            }
            LeadLagDirection::Flat if self.position > 0.0 => {
                let bid = self.follower_bid?;
                Some((
                    Side::Ask,
                    self.round_to_tick(bid),
                    self.config.follower_venue,
                    self.config.follower_symbol.clone(),
                ))
            }
            LeadLagDirection::Flat if self.position < 0.0 => {
                let ask = self.follower_ask?;
                Some((
                    Side::Bid,
                    self.round_to_tick(ask),
                    self.config.follower_venue,
                    self.config.follower_symbol.clone(),
                ))
            }
            _ => None,
        }?;

        let (side, price, venue, symbol) = intent;
        let tag = match side {
            Side::Bid => "B",
            Side::Ask => "S",
        };
        let intents = vec![QuoteIntent::new(
            venue,
            symbol,
            side,
            price,
            self.size,
            TimeInForce::Ioc,
            self.next_client_id(tag),
        )];
        let mid = self.follower_bid.zip(self.follower_ask).map(|(b, a)| (b + a) * 0.5);
        Some(QuotePlan {
            reference_price: mid.unwrap_or(price),
            entry_move_bps: None,
            reference_best_bid: self.follower_bid,
            reference_best_ask: self.follower_ask,
            cancels: Vec::new(),
            intents,
            planned_at: now,
            reference_meta: self.latest_meta.clone(),
            prior_submit_at: self.last_submit_at,
        })
    }

    pub fn commit_plan(&mut self, plan: &QuotePlan) {
        for intent in &plan.intents {
            self.active_orders.push(intent.client_order_id.clone());
            self.active_quotes.insert(
                intent.client_order_id.clone(),
                ActiveOrder {
                    side: intent.side,
                    price: intent.price,
                    placed_at: Instant::now(),
                },
            );
        }
        self.last_submit_at = Some(Instant::now());
        self.needs_quote = false;
    }

    pub fn rollback_plan(&mut self, _plan: &QuotePlan) {
        self.needs_quote = true;
    }

    pub fn handle_report(&mut self, report: &ExecutionReport) {
        if report.filled_qty > 0.0 {
            let is_buy = self
                .active_quotes
                .get(&report.client_order_id)
                .map(|q| q.side == Side::Bid)
                .unwrap_or(false);
            let signed = if is_buy {
                report.filled_qty
            } else {
                -report.filled_qty
            };
            self.position += signed;
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
        match (self.follower_bid, self.follower_ask) {
            (Some(b), Some(a)) => Some((b + a) * 0.5),
            _ => None,
        }
    }

    pub fn fill_context(&self, order_id: &ClientOrderId, now: Instant) -> FillContext {
        let order_age_ms = self
            .active_quotes
            .get(order_id)
            .map(|q| now.duration_since(q.placed_at).as_millis() as u64);
        FillContext {
            client_order_id: order_id.clone(),
            fair_mid: self.latest_price(),
            lighter_mid: None,
            entry_reference_price: self.latest_price(),
            entry_move_bps: None,
            order_age_ms,
        }
    }

    pub fn idle_reason(&self) -> Option<String> {
        if self.leader_bid.is_none() {
            Some("waiting for leader BBO".into())
        } else if self.follower_bid.is_none() {
            Some("waiting for follower BBO".into())
        } else if !self.needs_quote {
            Some("no lead-lag signal".into())
        } else {
            None
        }
    }
}
