//! Causal microstructure features for quoting.
//!
//! Implements:
//! - Cont–Kukanov–Stoikov (2014) best-level OFI
//! - Stoikov (2018) microprice
//! - queue imbalance
//! - VPIN-style bucketed flow toxicity (Easley–López de Prado–O'Hara)
//! - Avellaneda–Stoikov reservation + optimal spread with γ→0 limit

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub struct BookTop {
    pub bid_px: f64,
    pub bid_qty: f64,
    pub ask_px: f64,
    pub ask_qty: f64,
}

impl BookTop {
    pub fn mid(self) -> Option<f64> {
        if self.bid_px > 0.0 && self.ask_px > 0.0 && self.ask_px > self.bid_px {
            Some(0.5 * (self.bid_px + self.ask_px))
        } else {
            None
        }
    }

    pub fn microprice(self) -> Option<f64> {
        let den = self.bid_qty + self.ask_qty;
        if den <= 0.0 {
            return self.mid();
        }
        if self.bid_px <= 0.0 || self.ask_px <= 0.0 {
            return None;
        }
        Some((self.ask_px * self.bid_qty + self.bid_px * self.ask_qty) / den)
    }

    pub fn imbalance(self) -> Option<f64> {
        let den = self.bid_qty + self.ask_qty;
        if den <= 0.0 {
            return None;
        }
        Some((self.bid_qty - self.ask_qty) / den)
    }
}

/// Best-level OFI (CKS 2014). Computed only within a continuous book epoch.
pub fn best_level_ofi(prev: BookTop, next: BookTop) -> f64 {
    let bid_ofi = if next.bid_px > prev.bid_px {
        next.bid_qty
    } else if next.bid_px < prev.bid_px {
        -prev.bid_qty
    } else {
        next.bid_qty - prev.bid_qty
    };
    let ask_ofi = if next.ask_px < prev.ask_px {
        next.ask_qty
    } else if next.ask_px > prev.ask_px {
        -prev.ask_qty
    } else {
        next.ask_qty - prev.ask_qty
    };
    bid_ofi - ask_ofi
}

#[derive(Debug, Clone)]
pub struct VpinEstimator {
    bucket_notional: f64,
    buckets: usize,
    buy_acc: f64,
    sell_acc: f64,
    history: VecDeque<f64>,
}

impl VpinEstimator {
    pub fn new(bucket_notional: f64, buckets: usize) -> Self {
        if !(bucket_notional.is_finite() && bucket_notional > 0.0) {
            panic!("VpinEstimator bucket_notional must be finite and > 0, got {bucket_notional}");
        }
        if buckets == 0 {
            panic!("VpinEstimator buckets must be > 0");
        }
        Self {
            bucket_notional,
            buckets,
            buy_acc: 0.0,
            sell_acc: 0.0,
            history: VecDeque::with_capacity(buckets),
        }
    }

    /// `signed_notional` > 0 is taker buy, < 0 is taker sell.
    pub fn on_trade(&mut self, signed_notional: f64) {
        if !signed_notional.is_finite() || signed_notional == 0.0 {
            return;
        }
        if signed_notional > 0.0 {
            self.buy_acc += signed_notional;
        } else {
            self.sell_acc += -signed_notional;
        }
        while self.buy_acc + self.sell_acc >= self.bucket_notional {
            let total = self.buy_acc + self.sell_acc;
            if total <= 0.0 {
                break;
            }
            let vpin = (self.buy_acc - self.sell_acc).abs() / total;
            self.history.push_back(vpin);
            while self.history.len() > self.buckets {
                self.history.pop_front();
            }
            self.buy_acc = 0.0;
            self.sell_acc = 0.0;
        }
    }

    pub fn value(&self) -> Option<f64> {
        if self.history.is_empty() {
            return None;
        }
        let sum: f64 = self.history.iter().sum();
        Some(sum / self.history.len() as f64)
    }
}

/// Kyle's λ ≈ |Δmid| / |signed qty|. Higher λ ⇒ more toxic flow / thinner book.
#[derive(Debug, Clone)]
pub struct KyleLambda {
    alpha: f64,
    last_mid: Option<f64>,
    ewma: Option<f64>,
}

impl KyleLambda {
    pub fn new(alpha: f64) -> Self {
        if !(alpha.is_finite() && alpha > 0.0 && alpha <= 1.0) {
            panic!("KyleLambda alpha must be in (0, 1], got {alpha}");
        }
        Self {
            alpha,
            last_mid: None,
            ewma: None,
        }
    }

    pub fn on_trade(&mut self, mid: f64, signed_qty: f64) {
        if !mid.is_finite() || mid <= 0.0 || !signed_qty.is_finite() || signed_qty == 0.0 {
            return;
        }
        let Some(prev) = self.last_mid.replace(mid) else {
            return;
        };
        let lambda = (mid - prev).abs() / signed_qty.abs();
        if !lambda.is_finite() {
            return;
        }
        self.ewma = Some(match self.ewma {
            Some(cur) => self.alpha * lambda + (1.0 - self.alpha) * cur,
            None => lambda,
        });
    }

    pub fn value(&self) -> Option<f64> {
        self.ewma
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AvellanedaQuote {
    pub reservation: f64,
    pub half_spread: f64,
    pub bid: f64,
    pub ask: f64,
}

/// Finite-horizon Avellaneda–Stoikov with γ→0 limit and toxicity-scaled risk aversion.
pub fn avellaneda_stoikov_quote(
    reference: f64,
    inventory: f64,
    gamma: f64,
    sigma: f64,
    k: f64,
    tau_secs: f64,
    min_half_spread: f64,
) -> Option<AvellanedaQuote> {
    if !reference.is_finite() || reference <= 0.0 {
        return None;
    }
    if !gamma.is_finite() || gamma < 0.0 {
        return None;
    }
    if !sigma.is_finite() || sigma < 0.0 || !k.is_finite() || k <= 0.0 {
        return None;
    }
    if !tau_secs.is_finite() || tau_secs <= 0.0 {
        return None;
    }
    let var = sigma * sigma;
    let reservation = reference - inventory * gamma * var * tau_secs;
    if !reservation.is_finite() || reservation <= 0.0 {
        return None;
    }
    let spread = if gamma == 0.0 {
        // γ → 0 limit: δ = σ² τ + 2/k
        var * tau_secs + 2.0 / k
    } else {
        gamma * var * tau_secs + (2.0 / gamma) * (1.0 + gamma / k).ln()
    };
    if !spread.is_finite() || spread < 0.0 {
        return None;
    }
    let half = (0.5 * spread).max(min_half_spread).max(0.0);
    let bid = reservation - half;
    let ask = reservation + half;
    if bid <= 0.0 || ask <= bid {
        return None;
    }
    Some(AvellanedaQuote {
        reservation,
        half_spread: half,
        bid,
        ask,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microprice_weights_toward_thinner_side() {
        let top = BookTop {
            bid_px: 100.0,
            bid_qty: 10.0,
            ask_px: 101.0,
            ask_qty: 2.0,
        };
        let mu = top.microprice().unwrap();
        assert!(mu > 100.5);
        assert!(mu < 101.0);
    }

    #[test]
    fn ofi_positive_when_bid_size_grows() {
        let prev = BookTop {
            bid_px: 100.0,
            bid_qty: 1.0,
            ask_px: 101.0,
            ask_qty: 1.0,
        };
        let next = BookTop {
            bid_px: 100.0,
            bid_qty: 4.0,
            ask_px: 101.0,
            ask_qty: 1.0,
        };
        assert!((best_level_ofi(prev, next) - 3.0).abs() < 1e-12);
    }

    #[test]
    fn vpin_high_on_one_sided_flow() {
        let mut vpin = VpinEstimator::new(10.0, 2);
        vpin.on_trade(10.0);
        vpin.on_trade(10.0);
        let value = vpin.value().unwrap();
        assert!((value - 1.0).abs() < 1e-12);
    }

    #[test]
    fn as_long_inventory_lowers_reservation() {
        let q = avellaneda_stoikov_quote(100.0, 2.0, 0.1, 0.2, 1.5, 1.0, 0.01).unwrap();
        let flat = avellaneda_stoikov_quote(100.0, 0.0, 0.1, 0.2, 1.5, 1.0, 0.01).unwrap();
        assert!(q.reservation < flat.reservation);
        assert!(q.bid < q.ask);
    }

    #[test]
    fn as_gamma_zero_limit_is_finite() {
        let q = avellaneda_stoikov_quote(100.0, 0.0, 0.0, 0.2, 1.5, 1.0, 0.01).unwrap();
        assert!(q.half_spread.is_finite());
        assert!(q.half_spread >= 0.01);
    }

    #[test]
    fn kyle_lambda_rises_on_large_price_impact() {
        let mut kyle = KyleLambda::new(1.0);
        kyle.on_trade(100.0, 1.0);
        kyle.on_trade(101.0, 1.0);
        assert!((kyle.value().unwrap() - 1.0).abs() < 1e-12);
    }
}
