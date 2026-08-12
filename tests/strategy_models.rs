#![cfg(feature = "gate_exec")]

use std::time::{Duration, Instant};

use rust_test::exchanges::digifinex::DigiFinexPositionAction;
use rust_test::strategy::{
    AdaptiveMarketMaker, AdaptiveMarketMakerConfig, BinanceDigiFinexLeadLag, LeadLagConfig,
    LeadLagSide, MarketSnapshot, VenueBbo,
};

#[test]
fn adaptive_market_maker_reacts_to_order_flow() {
    let mut model = AdaptiveMarketMaker::new(AdaptiveMarketMakerConfig {
        tick_size: 0.01,
        base_order_size: 1.0,
        fee_bps: 0.5,
        min_half_spread_bps: 1.0,
        max_half_spread_bps: 30.0,
        volatility_alpha: 0.2,
        flow_alpha: 0.5,
        microprice_weight: 0.5,
        ofi_weight_bps: 2.0,
        trade_flow_weight_bps: 2.0,
        risk_aversion: 0.01,
        arrival_rate: 1_000.0,
        horizon_seconds: 0.5,
        inventory_limit: 10.0,
        toxicity_spread_multiplier: 8.0,
        toxicity_size_reduction: 0.8,
        toxicity_halt_threshold: 0.9,
        stale_after_ms: 250,
    })
    .expect("valid model");

    let neutral = model
        .update(&snapshot(1.0, 1.0, 1.0, 1.0))
        .expect("neutral quote");
    let pressure = model
        .update(&snapshot(5.0, 0.5, 10.0, 0.0))
        .expect("pressure quote");

    assert!(pressure.fair_price > neutral.fair_price);
    assert!(pressure.bid_size < neutral.bid_size);
}

#[test]
fn lead_lag_emits_only_after_executable_costs() {
    let now = Instant::now();
    let mut strategy = BinanceDigiFinexLeadLag::new(LeadLagConfig {
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
    })
    .expect("valid strategy");

    strategy
        .on_binance_bbo(bbo(100.0, 1, now))
        .expect("leader warmup");
    strategy
        .on_digifinex_bbo(bbo(100.0, 1, now))
        .expect("lagger warmup");
    strategy
        .on_binance_bbo(bbo(100.05, 2, now + Duration::from_millis(1)))
        .expect("leader impulse");

    let decision = strategy
        .evaluate(now + Duration::from_millis(2))
        .expect("fresh feeds")
        .expect("edge clears costs");
    assert_eq!(decision.side, LeadLagSide::Buy);
    assert!(decision.expected_edge_bps >= 1.0);
    let plan = decision
        .digifinex_execution_plan("BTCUSDTPERP", -0.005)
        .expect("position-aware execution plan");
    assert_eq!(plan.len(), 2);
    assert!(matches!(
        plan[0].action,
        DigiFinexPositionAction::CloseShort
    ));
    assert!(matches!(plan[1].action, DigiFinexPositionAction::OpenLong));
}

fn snapshot(bid_qty: f64, ask_qty: f64, buys: f64, sells: f64) -> MarketSnapshot {
    MarketSnapshot {
        bid_levels: vec![(99.99, bid_qty), (99.98, 2.0)],
        ask_levels: vec![(100.01, ask_qty), (100.02, 2.0)],
        aggressive_buy_volume: buys,
        aggressive_sell_volume: sells,
        inventory: 0.0,
        age_ms: 5,
    }
}

fn bbo(mid: f64, timestamp: u64, received_at: Instant) -> VenueBbo {
    VenueBbo {
        bid: mid - 0.005,
        ask: mid + 0.005,
        exchange_ts_ms: timestamp,
        received_at,
    }
}
