//! Event-driven Binance leader -> DigiFinex lagger signal.
//!
//! The strategy emits an intent only after fees, spread and configured impact
//! are covered. Execution and position reconciliation remain venue concerns.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeadLagConfig {
    pub symbol: String,
    pub quantity: f64,
    pub entry_threshold_bps: f64,
    pub exit_hysteresis_bps: f64,
    pub taker_fee_bps: f64,
    pub impact_buffer_bps: f64,
    pub max_signal_bps: f64,
    pub max_position: f64,
    pub max_feed_age_ms: u64,
    pub cooldown_ms: u64,
    pub signal_halflife_ms: f64,
    pub calibration_alpha: f64,
    pub initial_beta: f64,
    pub min_calibration_samples: u64,
}

impl LeadLagConfig {
    pub fn validate(&self) -> Result<()> {
        if self.symbol.trim().is_empty() {
            bail!("lead-lag symbol must be non-empty");
        }
        finite_gt("quantity", self.quantity, 0.0)?;
        finite_gt("entry_threshold_bps", self.entry_threshold_bps, 0.0)?;
        finite_ge("exit_hysteresis_bps", self.exit_hysteresis_bps, 0.0)?;
        if self.exit_hysteresis_bps >= self.entry_threshold_bps {
            bail!("exit_hysteresis_bps must be smaller than entry_threshold_bps");
        }
        finite_ge("taker_fee_bps", self.taker_fee_bps, 0.0)?;
        finite_ge("impact_buffer_bps", self.impact_buffer_bps, 0.0)?;
        finite_gt(
            "max_signal_bps",
            self.max_signal_bps,
            self.entry_threshold_bps,
        )?;
        finite_gt("max_position", self.max_position, 0.0)?;
        if self.quantity > self.max_position {
            bail!("quantity cannot exceed max_position");
        }
        if self.max_feed_age_ms == 0 {
            bail!("max_feed_age_ms must be > 0");
        }
        finite_gt("signal_halflife_ms", self.signal_halflife_ms, 0.0)?;
        if !self.calibration_alpha.is_finite()
            || self.calibration_alpha <= 0.0
            || self.calibration_alpha > 1.0
        {
            bail!("calibration_alpha must be in (0, 1]");
        }
        finite_gt("initial_beta", self.initial_beta, 0.0)?;
        if self.min_calibration_samples == 0 {
            bail!("min_calibration_samples must be > 0");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VenueBbo {
    pub bid: f64,
    pub ask: f64,
    pub exchange_ts_ms: u64,
    pub received_at: Instant,
}

impl VenueBbo {
    fn validate(self, venue: &str) -> Result<()> {
        finite_gt(&format!("{venue}.bid"), self.bid, 0.0)?;
        finite_gt(&format!("{venue}.ask"), self.ask, 0.0)?;
        if self.ask <= self.bid {
            bail!(
                "{venue} BBO is crossed or locked: bid={} ask={}",
                self.bid,
                self.ask
            );
        }
        if self.exchange_ts_ms == 0 {
            bail!("{venue} BBO exchange_ts_ms must be non-zero");
        }
        Ok(())
    }

    fn mid(self) -> f64 {
        0.5 * (self.bid + self.ask)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeadLagSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct LeadLagDecision {
    pub side: LeadLagSide,
    pub quantity: f64,
    /// Marketable limit guard; execution must not pay beyond this price.
    pub limit_price: f64,
    pub expected_move_bps: f64,
    pub expected_edge_bps: f64,
    pub beta: f64,
    pub calibration_samples: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalState {
    Flat,
    Long,
    Short,
}

pub struct BinanceDigiFinexLeadLag {
    config: LeadLagConfig,
    binance: Option<VenueBbo>,
    digifinex: Option<VenueBbo>,
    previous_binance_mid: Option<f64>,
    previous_digifinex_mid: Option<f64>,
    leader_move_since_lagger: f64,
    pending_signal_log: f64,
    covariance_ewma: f64,
    leader_variance_ewma: f64,
    beta: f64,
    calibration_samples: u64,
    position: f64,
    signal_state: SignalState,
    last_signal_update: Option<Instant>,
    last_decision_at: Option<Instant>,
}

impl BinanceDigiFinexLeadLag {
    pub fn new(config: LeadLagConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            beta: config.initial_beta,
            config,
            binance: None,
            digifinex: None,
            previous_binance_mid: None,
            previous_digifinex_mid: None,
            leader_move_since_lagger: 0.0,
            pending_signal_log: 0.0,
            covariance_ewma: 0.0,
            leader_variance_ewma: 0.0,
            calibration_samples: 0,
            position: 0.0,
            signal_state: SignalState::Flat,
            last_signal_update: None,
            last_decision_at: None,
        })
    }

    pub fn on_binance_bbo(&mut self, update: VenueBbo) -> Result<()> {
        update.validate("binance")?;
        if let Some(previous) = self.binance {
            ensure_monotonic_timestamp("binance", previous.exchange_ts_ms, update.exchange_ts_ms)?;
        }
        let mid = update.mid();
        self.decay_signal(update.received_at);
        if let Some(previous_mid) = self.previous_binance_mid {
            let leader_return = (mid / previous_mid).ln();
            if !leader_return.is_finite() {
                bail!("non-finite Binance log return");
            }
            self.leader_move_since_lagger += leader_return;
            self.pending_signal_log += self.beta * leader_return;
            let max_log = self.config.max_signal_bps * 1e-4;
            self.pending_signal_log = self.pending_signal_log.clamp(-max_log, max_log);
        }
        self.previous_binance_mid = Some(mid);
        self.binance = Some(update);
        Ok(())
    }

    pub fn on_digifinex_bbo(&mut self, update: VenueBbo) -> Result<()> {
        update.validate("digifinex")?;
        if let Some(previous) = self.digifinex {
            ensure_monotonic_timestamp(
                "digifinex",
                previous.exchange_ts_ms,
                update.exchange_ts_ms,
            )?;
        }
        let mid = update.mid();
        self.decay_signal(update.received_at);
        if let Some(previous_mid) = self.previous_digifinex_mid {
            let lagger_return = (mid / previous_mid).ln();
            if !lagger_return.is_finite() {
                bail!("non-finite DigiFinex log return");
            }
            self.update_beta(self.leader_move_since_lagger, lagger_return);
            // Remove the portion already incorporated by the lagger.
            self.pending_signal_log -= lagger_return;
            let max_log = self.config.max_signal_bps * 1e-4;
            self.pending_signal_log = self.pending_signal_log.clamp(-max_log, max_log);
        }
        self.leader_move_since_lagger = 0.0;
        self.previous_digifinex_mid = Some(mid);
        self.digifinex = Some(update);
        Ok(())
    }

    pub fn set_position(&mut self, position: f64) -> Result<()> {
        if !position.is_finite() {
            bail!("lead-lag position must be finite");
        }
        if position.abs() > self.config.max_position {
            bail!(
                "lead-lag position {} exceeds max_position {}",
                position,
                self.config.max_position
            );
        }
        self.position = position;
        Ok(())
    }

    pub fn evaluate(&mut self, now: Instant) -> Result<Option<LeadLagDecision>> {
        self.decay_signal(now);
        let binance = self
            .binance
            .ok_or_else(|| anyhow::anyhow!("lead-lag waiting for Binance BBO"))?;
        let digifinex = self
            .digifinex
            .ok_or_else(|| anyhow::anyhow!("lead-lag waiting for DigiFinex BBO"))?;
        let max_age = Duration::from_millis(self.config.max_feed_age_ms);
        let binance_age = now.saturating_duration_since(binance.received_at);
        let digifinex_age = now.saturating_duration_since(digifinex.received_at);
        if binance_age > max_age || digifinex_age > max_age {
            bail!(
                "lead-lag feed stale: binance_age_ms={} digifinex_age_ms={} max_ms={}",
                binance_age.as_millis(),
                digifinex_age.as_millis(),
                self.config.max_feed_age_ms
            );
        }
        if let Some(last) = self.last_decision_at
            && now.saturating_duration_since(last) < Duration::from_millis(self.config.cooldown_ms)
        {
            return Ok(None);
        }

        let expected_move_bps = self.pending_signal_log * 10_000.0;
        let mid = digifinex.mid();
        let crossing_bps = if expected_move_bps > 0.0 {
            (digifinex.ask / mid).ln() * 10_000.0
        } else {
            (mid / digifinex.bid).ln() * 10_000.0
        };
        let expected_edge_bps = expected_move_bps.abs()
            - crossing_bps
            - self.config.taker_fee_bps
            - self.config.impact_buffer_bps;

        let threshold = match self.signal_state {
            SignalState::Flat => self.config.entry_threshold_bps,
            SignalState::Long if expected_move_bps > 0.0 => self.config.exit_hysteresis_bps,
            SignalState::Short if expected_move_bps < 0.0 => self.config.exit_hysteresis_bps,
            SignalState::Long | SignalState::Short => self.config.entry_threshold_bps,
        };
        if expected_edge_bps < threshold {
            if expected_edge_bps <= self.config.exit_hysteresis_bps {
                self.signal_state = SignalState::Flat;
            }
            return Ok(None);
        }

        let (side, limit_price, resulting_position, state) = if expected_move_bps > 0.0 {
            (
                LeadLagSide::Buy,
                digifinex.ask,
                self.position + self.config.quantity,
                SignalState::Long,
            )
        } else {
            (
                LeadLagSide::Sell,
                digifinex.bid,
                self.position - self.config.quantity,
                SignalState::Short,
            )
        };
        if resulting_position.abs() > self.config.max_position {
            return Ok(None);
        }
        self.signal_state = state;
        self.last_decision_at = Some(now);
        Ok(Some(LeadLagDecision {
            side,
            quantity: self.config.quantity,
            limit_price,
            expected_move_bps,
            expected_edge_bps,
            beta: self.beta,
            calibration_samples: self.calibration_samples,
        }))
    }

    pub fn beta(&self) -> f64 {
        self.beta
    }

    fn update_beta(&mut self, leader_return: f64, lagger_return: f64) {
        if leader_return == 0.0 {
            return;
        }
        let alpha = self.config.calibration_alpha;
        self.covariance_ewma = ewma(self.covariance_ewma, leader_return * lagger_return, alpha);
        self.leader_variance_ewma = ewma(
            self.leader_variance_ewma,
            leader_return * leader_return,
            alpha,
        );
        self.calibration_samples = self.calibration_samples.saturating_add(1);
        if self.calibration_samples >= self.config.min_calibration_samples
            && self.leader_variance_ewma > f64::EPSILON
        {
            self.beta = (self.covariance_ewma / self.leader_variance_ewma).clamp(0.05, 3.0);
        }
    }

    fn decay_signal(&mut self, now: Instant) {
        if let Some(previous) = self.last_signal_update {
            let elapsed_ms = now.saturating_duration_since(previous).as_secs_f64() * 1000.0;
            let decay =
                (-elapsed_ms * std::f64::consts::LN_2 / self.config.signal_halflife_ms).exp();
            self.pending_signal_log *= decay;
        }
        self.last_signal_update = Some(now);
    }
}

fn ensure_monotonic_timestamp(venue: &str, previous: u64, current: u64) -> Result<()> {
    if current < previous {
        bail!("{venue} exchange timestamp moved backwards: previous={previous} current={current}");
    }
    Ok(())
}

fn ewma(previous: f64, sample: f64, alpha: f64) -> f64 {
    alpha * sample + (1.0 - alpha) * previous
}

fn finite_ge(name: &str, value: f64, minimum: f64) -> Result<()> {
    if !value.is_finite() || value < minimum {
        bail!("{name} must be finite and >= {minimum}, got {value}");
    }
    Ok(())
}

fn finite_gt(name: &str, value: f64, minimum: f64) -> Result<()> {
    if !value.is_finite() || value <= minimum {
        bail!("{name} must be finite and > {minimum}, got {value}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> LeadLagConfig {
        LeadLagConfig {
            symbol: "BTC_USDT".to_string(),
            quantity: 0.01,
            entry_threshold_bps: 1.0,
            exit_hysteresis_bps: 0.25,
            taker_fee_bps: 0.1,
            impact_buffer_bps: 0.1,
            max_signal_bps: 50.0,
            max_position: 0.1,
            max_feed_age_ms: 100,
            cooldown_ms: 10,
            signal_halflife_ms: 500.0,
            calibration_alpha: 0.2,
            initial_beta: 1.0,
            min_calibration_samples: 2,
        }
    }

    fn bbo(mid: f64, timestamp: u64, now: Instant) -> VenueBbo {
        VenueBbo {
            bid: mid - 0.005,
            ask: mid + 0.005,
            exchange_ts_ms: timestamp,
            received_at: now,
        }
    }

    #[test]
    fn emits_buy_after_binance_leads_up() {
        let now = Instant::now();
        let mut strategy = BinanceDigiFinexLeadLag::new(config()).expect("valid strategy");
        strategy
            .on_binance_bbo(bbo(100.0, 1, now))
            .expect("leader warmup");
        strategy
            .on_digifinex_bbo(bbo(100.0, 1, now))
            .expect("lagger warmup");
        strategy
            .on_binance_bbo(bbo(100.05, 2, now + Duration::from_millis(1)))
            .expect("leader move");
        let decision = strategy
            .evaluate(now + Duration::from_millis(2))
            .expect("fresh feeds")
            .expect("profitable signal");
        assert_eq!(decision.side, LeadLagSide::Buy);
        assert!(decision.expected_edge_bps > 1.0);
    }

    #[test]
    fn lagger_catchup_removes_signal() {
        let now = Instant::now();
        let mut strategy = BinanceDigiFinexLeadLag::new(config()).expect("valid strategy");
        strategy
            .on_binance_bbo(bbo(100.0, 1, now))
            .expect("leader warmup");
        strategy
            .on_digifinex_bbo(bbo(100.0, 1, now))
            .expect("lagger warmup");
        strategy
            .on_binance_bbo(bbo(100.05, 2, now + Duration::from_millis(1)))
            .expect("leader move");
        strategy
            .on_digifinex_bbo(bbo(100.05, 2, now + Duration::from_millis(2)))
            .expect("lagger catches up");
        assert!(
            strategy
                .evaluate(now + Duration::from_millis(3))
                .expect("fresh")
                .is_none()
        );
    }

    #[test]
    fn stale_feed_fails_loudly() {
        let now = Instant::now();
        let mut strategy = BinanceDigiFinexLeadLag::new(config()).expect("valid strategy");
        strategy.on_binance_bbo(bbo(100.0, 1, now)).expect("leader");
        strategy
            .on_digifinex_bbo(bbo(100.0, 1, now))
            .expect("lagger");
        let err = strategy
            .evaluate(now + Duration::from_millis(101))
            .expect_err("stale feed must error");
        assert!(err.to_string().contains("stale"));
    }
}
