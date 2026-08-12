//! Order-flow-aware market-making quote model.
//!
//! This module is deliberately independent from venue execution. It consumes a
//! normalized L2/trade snapshot and returns prices/sizes that an execution
//! strategy can submit after venue tick/lot rounding.

use anyhow::{Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdaptiveMarketMakerConfig {
    pub tick_size: f64,
    pub base_order_size: f64,
    pub fee_bps: f64,
    pub min_half_spread_bps: f64,
    pub max_half_spread_bps: f64,
    pub volatility_alpha: f64,
    pub flow_alpha: f64,
    pub microprice_weight: f64,
    pub ofi_weight_bps: f64,
    pub trade_flow_weight_bps: f64,
    pub risk_aversion: f64,
    pub arrival_rate: f64,
    pub horizon_seconds: f64,
    pub inventory_limit: f64,
    pub toxicity_spread_multiplier: f64,
    pub toxicity_size_reduction: f64,
    pub toxicity_halt_threshold: f64,
    pub stale_after_ms: u64,
}

impl AdaptiveMarketMakerConfig {
    pub fn validate(&self) -> Result<()> {
        finite_gt("tick_size", self.tick_size, 0.0)?;
        finite_gt("base_order_size", self.base_order_size, 0.0)?;
        finite_ge("fee_bps", self.fee_bps, 0.0)?;
        finite_ge("min_half_spread_bps", self.min_half_spread_bps, 0.0)?;
        finite_ge(
            "max_half_spread_bps",
            self.max_half_spread_bps,
            self.min_half_spread_bps,
        )?;
        unit_interval("volatility_alpha", self.volatility_alpha, false)?;
        unit_interval("flow_alpha", self.flow_alpha, false)?;
        finite_ge("microprice_weight", self.microprice_weight, 0.0)?;
        finite_ge("ofi_weight_bps", self.ofi_weight_bps, 0.0)?;
        finite_ge("trade_flow_weight_bps", self.trade_flow_weight_bps, 0.0)?;
        finite_gt("risk_aversion", self.risk_aversion, 0.0)?;
        finite_gt("arrival_rate", self.arrival_rate, 0.0)?;
        finite_gt("horizon_seconds", self.horizon_seconds, 0.0)?;
        finite_gt("inventory_limit", self.inventory_limit, 0.0)?;
        finite_ge(
            "toxicity_spread_multiplier",
            self.toxicity_spread_multiplier,
            0.0,
        )?;
        unit_interval(
            "toxicity_size_reduction",
            self.toxicity_size_reduction,
            true,
        )?;
        unit_interval(
            "toxicity_halt_threshold",
            self.toxicity_halt_threshold,
            false,
        )?;
        if self.stale_after_ms == 0 {
            bail!("stale_after_ms must be > 0");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    pub bid_levels: Vec<(f64, f64)>,
    pub ask_levels: Vec<(f64, f64)>,
    pub aggressive_buy_volume: f64,
    pub aggressive_sell_volume: f64,
    pub inventory: f64,
    pub age_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketRegime {
    Normal,
    Toxic,
    Halted,
}

#[derive(Debug, Clone)]
pub struct AdaptiveQuote {
    pub fair_price: f64,
    pub reservation_price: f64,
    pub bid_price: Option<f64>,
    pub ask_price: Option<f64>,
    pub bid_size: f64,
    pub ask_size: f64,
    pub half_spread_bps: f64,
    pub ofi: f64,
    pub trade_flow_imbalance: f64,
    pub toxicity: f64,
    pub volatility_bps: f64,
    pub regime: MarketRegime,
}

#[derive(Debug, Clone, Copy)]
struct TopOfBook {
    bid: f64,
    bid_qty: f64,
    ask: f64,
    ask_qty: f64,
}

pub struct AdaptiveMarketMaker {
    config: AdaptiveMarketMakerConfig,
    previous_top: Option<TopOfBook>,
    previous_mid: Option<f64>,
    variance_ewma: f64,
    ofi_ewma: f64,
    signed_flow_ewma: f64,
    total_flow_ewma: f64,
}

impl AdaptiveMarketMaker {
    pub fn new(config: AdaptiveMarketMakerConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            previous_top: None,
            previous_mid: None,
            variance_ewma: 0.0,
            ofi_ewma: 0.0,
            signed_flow_ewma: 0.0,
            total_flow_ewma: 0.0,
        })
    }

    pub fn update(&mut self, snapshot: &MarketSnapshot) -> Result<AdaptiveQuote> {
        let top = validate_snapshot(snapshot, &self.config)?;
        let mid = 0.5 * (top.bid + top.ask);
        let microprice =
            (top.ask * top.bid_qty + top.bid * top.ask_qty) / (top.bid_qty + top.ask_qty);

        if let Some(previous_mid) = self.previous_mid {
            let log_return = (mid / previous_mid).ln();
            if !log_return.is_finite() {
                bail!("non-finite log return mid={mid} previous_mid={previous_mid}");
            }
            self.variance_ewma = ewma(
                self.variance_ewma,
                log_return * log_return,
                self.config.volatility_alpha,
            );
        }

        let raw_ofi = self
            .previous_top
            .map(|previous| normalized_ofi(previous, top))
            .unwrap_or(0.0);
        self.ofi_ewma = ewma(self.ofi_ewma, raw_ofi, self.config.flow_alpha).clamp(-1.0, 1.0);

        let signed_flow = snapshot.aggressive_buy_volume - snapshot.aggressive_sell_volume;
        let total_flow = snapshot.aggressive_buy_volume + snapshot.aggressive_sell_volume;
        self.signed_flow_ewma = ewma(self.signed_flow_ewma, signed_flow, self.config.flow_alpha);
        self.total_flow_ewma = ewma(self.total_flow_ewma, total_flow, self.config.flow_alpha);
        let trade_flow_imbalance = if self.total_flow_ewma > 0.0 {
            (self.signed_flow_ewma / self.total_flow_ewma).clamp(-1.0, 1.0)
        } else {
            0.0
        };

        // A VPIN-like online proxy: one-sided aggressive flow plus L2 OFI
        // disagreement. It is bounded and does not pretend to be bucketed VPIN.
        let flow_toxicity = trade_flow_imbalance.abs();
        let book_toxicity = self.ofi_ewma.abs();
        let disagreement = if trade_flow_imbalance.signum() != self.ofi_ewma.signum() {
            (trade_flow_imbalance.abs() + self.ofi_ewma.abs()) * 0.25
        } else {
            0.0
        };
        let toxicity = (0.55 * flow_toxicity + 0.45 * book_toxicity + disagreement).clamp(0.0, 1.0);

        let volatility = self.variance_ewma.max(0.0).sqrt();
        let volatility_bps = volatility * 10_000.0;
        let microprice_move_bps = ((microprice / mid).ln() * 10_000.0).clamp(-50.0, 50.0);
        let alpha_bps = self.config.microprice_weight * microprice_move_bps
            + self.config.ofi_weight_bps * self.ofi_ewma
            + self.config.trade_flow_weight_bps * trade_flow_imbalance;
        let fair_price = mid * (alpha_bps * 1e-4).exp();

        let normalized_inventory =
            (snapshot.inventory / self.config.inventory_limit).clamp(-1.0, 1.0);
        let inventory_shift_log = self.config.risk_aversion
            * normalized_inventory
            * self.variance_ewma
            * self.config.horizon_seconds;
        let reservation_price = fair_price * (-inventory_shift_log).exp();

        // Infinite-horizon A-S arrival term plus finite-horizon inventory risk.
        let gamma = self.config.risk_aversion;
        let arrival_component_log = (1.0 + gamma / self.config.arrival_rate).ln() / gamma;
        let risk_component_log = 0.5 * gamma * self.variance_ewma * self.config.horizon_seconds;
        let model_half_spread_bps = (arrival_component_log + risk_component_log) * 10_000.0;
        let half_spread_bps = (self.config.fee_bps
            + model_half_spread_bps
            + volatility_bps
            + toxicity * self.config.toxicity_spread_multiplier)
            .clamp(
                self.config.min_half_spread_bps,
                self.config.max_half_spread_bps,
            );

        let inventory_abs = normalized_inventory.abs();
        let size_factor =
            (1.0 - self.config.toxicity_size_reduction * toxicity - 0.5 * inventory_abs)
                .clamp(0.0, 1.0);
        let mut bid_size = self.config.base_order_size * size_factor;
        let mut ask_size = self.config.base_order_size * size_factor;
        if normalized_inventory > 0.0 {
            bid_size *= 1.0 - normalized_inventory;
        } else {
            ask_size *= 1.0 + normalized_inventory;
        }

        let regime = if snapshot.age_ms > self.config.stale_after_ms
            || snapshot.inventory.abs() >= self.config.inventory_limit
        {
            MarketRegime::Halted
        } else if toxicity >= self.config.toxicity_halt_threshold {
            MarketRegime::Toxic
        } else {
            MarketRegime::Normal
        };

        let (bid_price, ask_price) = match regime {
            MarketRegime::Halted => (None, None),
            MarketRegime::Normal | MarketRegime::Toxic => {
                let distance = half_spread_bps * 1e-4;
                let bid = round_down(reservation_price * (-distance).exp(), self.config.tick_size);
                let ask = round_up(reservation_price * distance.exp(), self.config.tick_size);
                if bid <= 0.0 || ask <= bid {
                    bail!(
                        "invalid generated quote bid={bid} ask={ask} reservation={reservation_price}"
                    );
                }
                (Some(bid), Some(ask))
            }
        };

        self.previous_top = Some(top);
        self.previous_mid = Some(mid);

        Ok(AdaptiveQuote {
            fair_price,
            reservation_price,
            bid_price,
            ask_price,
            bid_size,
            ask_size,
            half_spread_bps,
            ofi: self.ofi_ewma,
            trade_flow_imbalance,
            toxicity,
            volatility_bps,
            regime,
        })
    }
}

fn validate_snapshot(
    snapshot: &MarketSnapshot,
    config: &AdaptiveMarketMakerConfig,
) -> Result<TopOfBook> {
    let (bid, bid_qty) = snapshot
        .bid_levels
        .first()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("market snapshot has no bid levels"))?;
    let (ask, ask_qty) = snapshot
        .ask_levels
        .first()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("market snapshot has no ask levels"))?;
    for (name, value) in [
        ("best_bid", bid),
        ("best_bid_qty", bid_qty),
        ("best_ask", ask),
        ("best_ask_qty", ask_qty),
    ] {
        finite_gt(name, value, 0.0)?;
    }
    if ask <= bid {
        bail!("crossed/locked snapshot best_bid={bid} best_ask={ask}");
    }
    for (side, levels) in [("bid", &snapshot.bid_levels), ("ask", &snapshot.ask_levels)] {
        for (index, (price, quantity)) in levels.iter().enumerate() {
            finite_gt(&format!("{side}[{index}].price"), *price, 0.0)?;
            finite_gt(&format!("{side}[{index}].quantity"), *quantity, 0.0)?;
        }
    }
    finite_ge("aggressive_buy_volume", snapshot.aggressive_buy_volume, 0.0)?;
    finite_ge(
        "aggressive_sell_volume",
        snapshot.aggressive_sell_volume,
        0.0,
    )?;
    if !snapshot.inventory.is_finite() {
        bail!("inventory must be finite");
    }
    if snapshot.age_ms > config.stale_after_ms.saturating_mul(100) {
        bail!(
            "market snapshot is implausibly stale: age_ms={} hard_limit_ms={}",
            snapshot.age_ms,
            config.stale_after_ms.saturating_mul(100)
        );
    }
    Ok(TopOfBook {
        bid,
        bid_qty,
        ask,
        ask_qty,
    })
}

fn normalized_ofi(previous: TopOfBook, current: TopOfBook) -> f64 {
    let bid_flow = if current.bid > previous.bid {
        current.bid_qty
    } else if current.bid < previous.bid {
        -previous.bid_qty
    } else {
        current.bid_qty - previous.bid_qty
    };
    let ask_flow = if current.ask < previous.ask {
        current.ask_qty
    } else if current.ask > previous.ask {
        -previous.ask_qty
    } else {
        current.ask_qty - previous.ask_qty
    };
    let depth = 0.5
        * (previous.bid_qty + previous.ask_qty + current.bid_qty + current.ask_qty)
            .max(f64::EPSILON);
    ((bid_flow - ask_flow) / depth).clamp(-1.0, 1.0)
}

fn ewma(previous: f64, sample: f64, alpha: f64) -> f64 {
    alpha * sample + (1.0 - alpha) * previous
}

fn round_down(value: f64, tick: f64) -> f64 {
    (value / tick).floor() * tick
}

fn round_up(value: f64, tick: f64) -> f64 {
    (value / tick).ceil() * tick
}

fn unit_interval(name: &str, value: f64, allow_zero: bool) -> Result<()> {
    if !value.is_finite()
        || value > 1.0
        || (allow_zero && value < 0.0)
        || (!allow_zero && value <= 0.0)
    {
        bail!(
            "{name} must be finite and in {}",
            if allow_zero { "[0, 1]" } else { "(0, 1]" }
        );
    }
    Ok(())
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

    fn config() -> AdaptiveMarketMakerConfig {
        AdaptiveMarketMakerConfig {
            tick_size: 0.1,
            base_order_size: 1.0,
            fee_bps: 1.0,
            min_half_spread_bps: 2.0,
            max_half_spread_bps: 50.0,
            volatility_alpha: 0.2,
            flow_alpha: 0.5,
            microprice_weight: 0.5,
            ofi_weight_bps: 2.0,
            trade_flow_weight_bps: 2.0,
            risk_aversion: 0.01,
            arrival_rate: 100.0,
            horizon_seconds: 1.0,
            inventory_limit: 10.0,
            toxicity_spread_multiplier: 10.0,
            toxicity_size_reduction: 0.8,
            toxicity_halt_threshold: 0.8,
            stale_after_ms: 500,
        }
    }

    fn snapshot(bid_qty: f64, ask_qty: f64, buys: f64, sells: f64) -> MarketSnapshot {
        MarketSnapshot {
            bid_levels: vec![(99.9, bid_qty), (99.8, 2.0)],
            ask_levels: vec![(100.1, ask_qty), (100.2, 2.0)],
            aggressive_buy_volume: buys,
            aggressive_sell_volume: sells,
            inventory: 0.0,
            age_ms: 10,
        }
    }

    #[test]
    fn buy_pressure_skews_fair_up_and_reduces_size() {
        let mut model = AdaptiveMarketMaker::new(config()).expect("valid model");
        let neutral = model
            .update(&snapshot(1.0, 1.0, 1.0, 1.0))
            .expect("neutral quote");
        let pressured = model
            .update(&snapshot(4.0, 0.5, 10.0, 0.0))
            .expect("pressure quote");
        assert!(pressured.fair_price > neutral.fair_price);
        assert!(pressured.bid_size < neutral.bid_size);
        assert!(pressured.half_spread_bps >= neutral.half_spread_bps);
    }

    #[test]
    fn long_inventory_skews_reservation_down() {
        let mut model = AdaptiveMarketMaker::new(config()).expect("valid model");
        model.update(&snapshot(1.0, 1.0, 1.0, 1.0)).expect("warmup");
        let mut moved = snapshot(1.0, 1.0, 1.0, 1.0);
        moved.bid_levels[0].0 = 100.9;
        moved.ask_levels[0].0 = 101.1;
        moved.inventory = 8.0;
        let quote = model.update(&moved).expect("inventory quote");
        assert!(quote.reservation_price < quote.fair_price);
        assert!(quote.bid_size < quote.ask_size);
    }

    #[test]
    fn stale_snapshot_halts_quotes() {
        let mut model = AdaptiveMarketMaker::new(config()).expect("valid model");
        let mut stale = snapshot(1.0, 1.0, 1.0, 1.0);
        stale.age_ms = 501;
        let quote = model.update(&stale).expect("stale is a controlled halt");
        assert_eq!(quote.regime, MarketRegime::Halted);
        assert!(quote.bid_price.is_none());
        assert!(quote.ask_price.is_none());
    }
}
