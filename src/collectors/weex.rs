use crate::base_classes::bbo_store::BboStore;
use crate::base_classes::orderbook_trait::OrderBookOps;
use crate::base_classes::tickers::{TickerSnapshot, TickerStore};
use crate::base_classes::trades::{FixedTrades, Trade};
use crate::base_classes::types::{Price, Qty, Seq};
use crate::exchanges::weex::{WeexBook, WeexFrame};
use crate::utils::parsing::log_parse_drop;
use crate::utils::time::ms_to_ns;
use serde_json::Value;

pub const PRICE_SCALE: f64 = WeexBook::<1>::PRICE_SCALE;
pub const QTY_SCALE: f64 = WeexBook::<1>::QTY_SCALE;

pub fn events_for<const N: usize>(
    frame: &mut WeexFrame,
    book: &mut WeexBook<N>,
) -> Vec<(&'static str, f64)> {
    let mut out = Vec::with_capacity(1);
    if matches!(frame.channel(), Some("depth")) {
        if let Some(msg) = frame.depth_msg() {
            if book.apply(msg) {
                if let Some(mid) = book.mid_price_f64() {
                    out.push(("orderbook", mid));
                }
            }
        }
    }
    out
}

fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => {
            let v = n.as_f64()?;
            if v.is_finite() {
                Some(v)
            } else {
                log_parse_drop(
                    "weex_collector",
                    "non_finite",
                    &"non-finite number",
                    &n.to_string(),
                );
                None
            }
        }
        Value::String(s) => match s.parse::<f64>() {
            Ok(v) if v.is_finite() => Some(v),
            Ok(_) => {
                log_parse_drop("weex_collector", "non_finite", &"non-finite number", s);
                None
            }
            Err(err) => {
                log_parse_drop("weex_collector", "f64", &err, s);
                None
            }
        },
        _ => None,
    }
}

fn as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => {
            let v = n.as_u64();
            if v.is_none() {
                log_parse_drop("weex_collector", "u64", &"non-u64 number", &n.to_string());
            }
            v
        }
        Value::String(s) => match s.parse::<u64>() {
            Ok(v) => Some(v),
            Err(err) => {
                log_parse_drop("weex_collector", "u64", &err, s);
                None
            }
        },
        _ => None,
    }
}

/// Update BBO from the maintained depth book (WEEX ticker channel lacks reliable bid/ask).
pub fn update_bbo_from_book<const N: usize>(
    book: &WeexBook<N>,
    store: &mut BboStore,
    symbol: &str,
) -> bool {
    if !book.is_initialized() {
        return false;
    }
    let (bid_px, bid_qty) = match book.best_bid_f64() {
        Some(v) => v,
        None => return false,
    };
    let (ask_px, ask_qty) = match book.best_ask_f64() {
        Some(v) => v,
        None => return false,
    };
    if bid_px <= 0.0 || ask_px <= 0.0 {
        return false;
    }
    let ts_ns = book.last_ts();
    let system_ts_ns = book.last_system_ts_ns();
    store.update(symbol, bid_px, bid_qty, ask_px, ask_qty, ts_ns, system_ts_ns);
    true
}

pub fn update_trades<const N: usize>(
    frame: &mut WeexFrame,
    trades: &mut FixedTrades<N>,
    qty_multiplier: f64,
) -> usize {
    if !matches!(frame.channel(), Some("trade")) {
        return 0;
    }
    let sample = frame.text().unwrap_or("").to_string();
    let raw = match frame.json() {
        Some(v) => v,
        None => {
            log_parse_drop(
                "weex_collector",
                "missing_json",
                &"missing json",
                sample.as_str(),
            );
            return 0;
        }
    };

    let entries = match raw.get("d").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return 0,
    };

    let system_ts_ns = raw
        .get("E")
        .and_then(as_u64)
        .map(ms_to_ns);

    let mut inserted = 0usize;
    for trade in entries {
        let price = trade.get("p").and_then(as_f64).or_else(|| {
            log_parse_drop(
                "weex_collector",
                "missing_price",
                &"missing price",
                sample.as_str(),
            );
            None
        });
        let qty = trade.get("q").and_then(as_f64).or_else(|| {
            log_parse_drop(
                "weex_collector",
                "missing_qty",
                &"missing qty",
                sample.as_str(),
            );
            None
        });
        let ts_ms = trade
            .get("T")
            .and_then(as_u64)
            .or_else(|| raw.get("E").and_then(as_u64))
            .or_else(|| {
                log_parse_drop(
                    "weex_collector",
                    "missing_ts",
                    &"missing ts",
                    sample.as_str(),
                );
                None
            });
        let seq = trade.get("t").and_then(as_u64).or_else(|| {
            log_parse_drop(
                "weex_collector",
                "missing_seq",
                &"missing seq",
                sample.as_str(),
            );
            None
        });
        let is_buyer_maker = trade.get("m").and_then(|v| v.as_bool());

        if price.is_none() || qty.is_none() || ts_ms.is_none() || seq.is_none() {
            continue;
        }
        if is_buyer_maker.is_none() {
            log_parse_drop(
                "weex_collector",
                "missing_side",
                &"missing m (maker side)",
                sample.as_str(),
            );
            continue;
        }

        let px_i = (price.unwrap() * PRICE_SCALE).round() as Price;
        let qty_i = (qty.unwrap() * qty_multiplier * QTY_SCALE).round() as Qty;
        let record = Trade::new(
            px_i,
            qty_i,
            ms_to_ns(ts_ms.unwrap()),
            seq.unwrap() as Seq,
            is_buyer_maker.unwrap(),
            system_ts_ns,
        );
        trades.push(record);
        inserted += 1;
    }
    inserted
}

pub fn update_tickers(
    frame: &mut WeexFrame,
    store: &mut TickerStore,
    qty_multiplier: f64,
) -> Option<(String, TickerSnapshot)> {
    if !matches!(frame.channel(), Some("ticker")) {
        return None;
    }
    let sample = frame.text().unwrap_or("").to_string();
    let raw = frame.json()?;
    let symbol = raw
        .get("s")
        .and_then(|v| v.as_str())
        .or_else(|| {
            log_parse_drop(
                "weex_collector",
                "missing_symbol",
                &"missing symbol",
                sample.as_str(),
            );
            None
        })?
        .to_string();

    let data = match raw.get("d").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return None,
    };
    let payload = match data.first() {
        Some(v) if v.is_object() => v,
        _ => return None,
    };

    let mut snapshot = store.get(&symbol).copied().unwrap_or_default();

    if let Some(last_px) = payload.get("c").and_then(as_f64) {
        snapshot.ticker.last_px = (last_px * PRICE_SCALE).round() as Price;
    }
    if let Some(mark_px) = payload.get("m").and_then(as_f64) {
        snapshot.mark_px = Some(mark_px);
    }
    if let Some(index_px) = payload.get("i").and_then(as_f64) {
        snapshot.index_px = Some(index_px);
    }
    if let Some(turnover) = payload.get("q").and_then(as_f64) {
        snapshot.turnover_24h = Some(turnover);
    }

    let ts_ms = match raw.get("E").and_then(as_u64) {
        Some(ts_ms) if ts_ms > 0 => ts_ms,
        _ => {
            log_parse_drop(
                "weex_collector",
                "missing_ts",
                &"missing E",
                sample.as_str(),
            );
            return None;
        }
    };
    snapshot.ticker.ts = ms_to_ns(ts_ms);
    snapshot.ticker.seq = ts_ms;

    let stored = store.update(symbol.clone(), snapshot);
    let _ = qty_multiplier;
    Some((symbol, stored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const SAMPLE_TRADE: &str = r#"{
        "e": "trade",
        "E": 1773295739001,
        "s": "BTCUSDT",
        "d": [
            {
                "T": 1773295739001,
                "t": 7423916138,
                "p": "69382.20",
                "q": "0.014",
                "v": "971.3508",
                "m": false
            }
        ]
    }"#;

    fn frame_from_str(json: &str) -> WeexFrame {
        let mut frame = WeexFrame::from_text(json, 0, Instant::now());
        frame.preparse_text(json);
        frame
    }

    #[test]
    fn test_update_trades_aggressor_side() {
        let mut frame = frame_from_str(SAMPLE_TRADE);
        let mut trades = FixedTrades::<8>::default();
        let inserted = update_trades(&mut frame, &mut trades, 1.0);
        assert_eq!(inserted, 1);
        let trade = trades.last().expect("missing trade");
        assert!(!trade.is_buyer_maker);
    }

    #[test]
    fn test_update_trades_aggressive_sell() {
        let json = r#"{
            "e": "trade",
            "E": 1773295739001,
            "s": "BTCUSDT",
            "d": [{"T": 1, "t": 99, "p": "100", "q": "1", "v": "100", "m": true}]
        }"#;
        let mut frame = frame_from_str(json);
        let mut trades = FixedTrades::<8>::default();
        update_trades(&mut frame, &mut trades, 1.0);
        let trade = trades.last().expect("missing trade");
        assert!(trade.is_buyer_maker);
    }
}
