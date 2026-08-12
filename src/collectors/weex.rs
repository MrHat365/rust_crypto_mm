use crate::base_classes::bbo_store::BboStore;
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
    if frame.event() == "depth" {
        if let Some(msg) = frame.depth_msg() {
            if book.apply_depth_update(msg) {
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
        Value::Number(n) => n.as_f64().filter(|v| v.is_finite()),
        Value::String(s) => match s.parse::<f64>() {
            Ok(v) if v.is_finite() => Some(v),
            Ok(_) => {
                log_parse_drop("weex_collector", "non_finite", &"non-finite", s);
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
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|v| v as u64)),
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

pub fn update_bbo_from_book(
    symbol: &str,
    store: &mut BboStore,
    bid: (f64, f64),
    ask: (f64, f64),
    ts_ns: u64,
) -> bool {
    if symbol.is_empty() || bid.0 <= 0.0 || ask.0 <= 0.0 {
        return false;
    }
    store.update(symbol, bid.0, bid.1, ask.0, ask.1, ts_ns, Some(ts_ns));
    true
}

pub fn update_trades<const N: usize>(
    frame: &mut WeexFrame,
    trades: &mut FixedTrades<N>,
) -> usize {
    if frame.event() != "trade" {
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
    let system_ts_ns = raw.get("E").and_then(as_u64).map(ms_to_ns);
    let mut inserted = 0usize;
    for trade in entries {
        let price = match trade.get("p").and_then(as_f64) {
            Some(v) if v > 0.0 => v,
            _ => {
                log_parse_drop(
                    "weex_collector",
                    "missing_price",
                    &"missing p",
                    sample.as_str(),
                );
                continue;
            }
        };
        let qty = match trade.get("q").and_then(as_f64) {
            Some(v) if v >= 0.0 => v,
            _ => {
                log_parse_drop(
                    "weex_collector",
                    "missing_qty",
                    &"missing q",
                    sample.as_str(),
                );
                continue;
            }
        };
        // Docs: m = maker was the seller => taker bought => is_buyer_maker = false.
        let maker_was_seller = match trade.get("m") {
            Some(Value::Bool(v)) => *v,
            Some(other) => {
                log_parse_drop(
                    "weex_collector",
                    "side",
                    &"m not bool",
                    &other.to_string(),
                );
                continue;
            }
            None => {
                log_parse_drop(
                    "weex_collector",
                    "missing_side",
                    &"missing m",
                    sample.as_str(),
                );
                continue;
            }
        };
        let is_buyer_maker = !maker_was_seller;
        let ts_ms = match trade.get("T").and_then(as_u64).or_else(|| raw.get("E").and_then(as_u64))
        {
            Some(ts) if ts > 0 => ts,
            _ => {
                log_parse_drop(
                    "weex_collector",
                    "missing_ts",
                    &"missing T",
                    sample.as_str(),
                );
                continue;
            }
        };
        let seq = trade
            .get("t")
            .and_then(as_u64)
            .or_else(|| {
                trade
                    .get("t")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(ts_ms);
        trades.push(Trade::new(
            (price * PRICE_SCALE).round() as Price,
            (qty * QTY_SCALE).round() as Qty,
            ms_to_ns(ts_ms),
            seq as Seq,
            is_buyer_maker,
            system_ts_ns,
        ));
        inserted += 1;
    }
    inserted
}

pub fn update_tickers(
    frame: &mut WeexFrame,
    store: &mut TickerStore,
) -> Option<(String, TickerSnapshot)> {
    if frame.event() != "ticker" {
        return None;
    }
    let sample = frame.text().unwrap_or("").to_string();
    let raw = frame.json()?;
    let symbol = raw.get("s").and_then(|v| v.as_str())?;
    let data = raw.get("d").and_then(|v| v.as_array()).and_then(|a| a.first())?;
    let prev = store.get(symbol).copied();
    let mut snapshot = prev.unwrap_or_default();
    if let Some(last_px) = data.get("c").and_then(as_f64) {
        snapshot.ticker.last_px = (last_px * PRICE_SCALE).round() as Price;
    }
    snapshot.mark_px = data.get("m").and_then(as_f64).or(snapshot.mark_px);
    snapshot.index_px = data.get("i").and_then(as_f64).or(snapshot.index_px);
    snapshot.turnover_24h = data.get("q").and_then(as_f64).or(snapshot.turnover_24h);
    let ts_ns = match raw.get("E").and_then(as_u64) {
        Some(ts_ms) if ts_ms > 0 => ms_to_ns(ts_ms),
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
    if let Some(prev) = prev {
        if prev.ticker.ts != 0 && ts_ns < prev.ticker.ts {
            log_parse_drop(
                "weex_collector",
                "stale_ts",
                &"stale ticker ts",
                sample.as_str(),
            );
            return None;
        }
    }
    snapshot.ticker.ts = ts_ns;
    snapshot.ticker.seq = prev.map(|s| s.ticker.seq.wrapping_add(1)).unwrap_or(1);
    let stored = store.update(symbol.to_string(), snapshot);
    Some((symbol.to_string(), stored))
}
