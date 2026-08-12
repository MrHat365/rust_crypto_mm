use crate::base_classes::bbo_store::BboStore;
use crate::base_classes::tickers::{TickerSnapshot, TickerStore};
use crate::base_classes::trades::{FixedTrades, Trade};
use crate::base_classes::types::{Price, Qty, Seq};
use crate::exchanges::digifinex::{DigifinexBook, DigifinexFrame};
use crate::utils::parsing::log_parse_drop;
use crate::utils::time::ms_to_ns;
use serde_json::Value;

pub const PRICE_SCALE: f64 = DigifinexBook::<1>::PRICE_SCALE;
pub const QTY_SCALE: f64 = DigifinexBook::<1>::QTY_SCALE;

pub fn events_for<const N: usize>(
    frame: &mut DigifinexFrame,
    book: &mut DigifinexBook<N>,
) -> Vec<(&'static str, f64)> {
    let mut out = Vec::with_capacity(1);
    if frame.event() == "depth.update" {
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
        Value::Number(n) => n.as_f64().filter(|v| v.is_finite()),
        Value::String(s) => match s.parse::<f64>() {
            Ok(v) if v.is_finite() => Some(v),
            Ok(_) => {
                log_parse_drop("digifinex_collector", "non_finite", &"non-finite", s);
                None
            }
            Err(err) => {
                log_parse_drop("digifinex_collector", "f64", &err, s);
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

fn value_to_f64(obj: &Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(v) = obj.get(*key).and_then(as_f64) {
            return Some(v);
        }
    }
    None
}

fn value_to_u64(obj: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(v) = obj.get(*key).and_then(as_u64) {
            return Some(v);
        }
    }
    None
}

fn direction_token(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => n
            .as_i64()
            .map(|v| v.to_string())
            .or_else(|| n.as_u64().map(|v| v.to_string())),
        _ => None,
    }
}

fn trade_entries(raw: &Value) -> Option<&Vec<Value>> {
    match raw.get("data") {
        Some(Value::Array(arr)) => Some(arr),
        Some(Value::Object(map)) => map
            .get("data")
            .or_else(|| map.get("trades"))
            .and_then(|v| v.as_array()),
        _ => None,
    }
}

/// Digifinex trade direction: 1 open long, 2 open short, 3 close long, 4 close short.
/// Buy aggressor = 1 or 4; sell aggressor = 2 or 3.
pub fn is_buyer_maker_from_direction(direction: &str) -> Option<bool> {
    match direction.trim() {
        "1" | "4" => Some(false),
        "2" | "3" => Some(true),
        _ => None,
    }
}

pub fn update_bbo_store(
    frame: &mut DigifinexFrame,
    store: &mut BboStore,
    qty_multiplier: f64,
) -> bool {
    if frame.event() != "ticker.update" {
        return false;
    }
    let sample = frame.text().unwrap_or("").to_string();
    let raw = match frame.json() {
        Some(v) => v,
        None => {
            log_parse_drop(
                "digifinex_collector",
                "missing_json",
                &"missing json",
                sample.as_str(),
            );
            return false;
        }
    };
    let data = match raw.get("data") {
        Some(Value::Object(_)) => raw.get("data").unwrap(),
        _ => return false,
    };
    let inst_id = match data.get("instrument_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return false,
    };
    let bid = match value_to_f64(data, &["best_bid"]) {
        Some(v) if v > 0.0 => v,
        _ => return false,
    };
    let ask = match value_to_f64(data, &["best_ask"]) {
        Some(v) if v > 0.0 => v,
        _ => return false,
    };
    let bid_qty = value_to_f64(data, &["best_bid_size"]).unwrap_or(0.0) * qty_multiplier;
    let ask_qty = value_to_f64(data, &["best_ask_size"]).unwrap_or(0.0) * qty_multiplier;
    let ts_ms = match value_to_u64(data, &["timestamp"]) {
        Some(ts) if ts > 0 => ts,
        _ => {
            log_parse_drop(
                "digifinex_collector",
                "missing_ts",
                &"missing timestamp",
                sample.as_str(),
            );
            return false;
        }
    };
    let ts_ns = ms_to_ns(ts_ms);
    store.update(inst_id, bid, bid_qty, ask, ask_qty, ts_ns, Some(ts_ns));
    true
}

pub fn update_trades<const N: usize>(
    frame: &mut DigifinexFrame,
    trades: &mut FixedTrades<N>,
    qty_multiplier: f64,
) -> usize {
    if frame.event() != "trades.update" {
        return 0;
    }
    let sample = frame.text().unwrap_or("").to_string();
    let raw = match frame.json() {
        Some(v) => v,
        None => {
            log_parse_drop(
                "digifinex_collector",
                "missing_json",
                &"missing json",
                sample.as_str(),
            );
            return 0;
        }
    };
    let entries = match trade_entries(raw) {
        Some(arr) => arr,
        None => return 0,
    };
    let mut inserted = 0usize;
    for trade in entries {
        let price = match trade.get("price").and_then(as_f64) {
            Some(v) if v > 0.0 => v,
            _ => {
                log_parse_drop(
                    "digifinex_collector",
                    "missing_price",
                    &"missing price",
                    sample.as_str(),
                );
                continue;
            }
        };
        let qty = match trade.get("volume").and_then(as_f64) {
            Some(v) if v >= 0.0 => v,
            _ => {
                log_parse_drop(
                    "digifinex_collector",
                    "missing_qty",
                    &"missing volume",
                    sample.as_str(),
                );
                continue;
            }
        };
        let direction = match trade.get("direction").and_then(direction_token) {
            Some(d) => d,
            None => {
                log_parse_drop(
                    "digifinex_collector",
                    "missing_side",
                    &"missing direction",
                    sample.as_str(),
                );
                continue;
            }
        };
        let is_buyer_maker = match is_buyer_maker_from_direction(&direction) {
            Some(v) => v,
            None => {
                log_parse_drop(
                    "digifinex_collector",
                    "direction",
                    &"unknown direction",
                    &direction,
                );
                continue;
            }
        };
        let ts_ms = match trade.get("trade_time").and_then(as_u64) {
            Some(ts) if ts > 0 => ts,
            _ => {
                log_parse_drop(
                    "digifinex_collector",
                    "missing_ts",
                    &"missing trade_time",
                    sample.as_str(),
                );
                continue;
            }
        };
        let seq = trade
            .get("trade_id")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| trade.get("trade_id").and_then(as_u64))
            .unwrap_or(ts_ms);
        let record = Trade::new(
            (price * PRICE_SCALE).round() as Price,
            (qty * qty_multiplier * QTY_SCALE).round() as Qty,
            ms_to_ns(ts_ms),
            seq as Seq,
            is_buyer_maker,
            Some(ms_to_ns(ts_ms)),
        );
        trades.push(record);
        inserted += 1;
    }
    inserted
}

pub fn update_tickers(
    frame: &mut DigifinexFrame,
    store: &mut TickerStore,
    qty_multiplier: f64,
) -> Option<(String, TickerSnapshot)> {
    if frame.event() != "ticker.update" {
        return None;
    }
    let sample = frame.text().unwrap_or("").to_string();
    let raw = frame.json()?;
    let data = raw.get("data")?;
    if !data.is_object() {
        return None;
    }
    let inst_id = data.get("instrument_id").and_then(|v| v.as_str())?;
    let prev = store.get(inst_id).copied();
    let mut snapshot = prev.unwrap_or_default();
    if let Some(last_px) = value_to_f64(data, &["last"]) {
        snapshot.ticker.last_px = (last_px * PRICE_SCALE).round() as Price;
    }
    if let Some(last_sz) = value_to_f64(data, &["last_qty"]) {
        snapshot.ticker.last_qty = (last_sz * qty_multiplier * QTY_SCALE).round() as Qty;
    }
    if let Some(bid_px) = value_to_f64(data, &["best_bid"]) {
        snapshot.ticker.best_bid = (bid_px * PRICE_SCALE).round() as Price;
    }
    if let Some(ask_px) = value_to_f64(data, &["best_ask"]) {
        snapshot.ticker.best_ask = (ask_px * PRICE_SCALE).round() as Price;
    }
    snapshot.turnover_24h = value_to_f64(data, &["volume_token_24h", "volume_24h"])
        .or(snapshot.turnover_24h);
    snapshot.open_interest = value_to_f64(data, &["open_interest"]).or(snapshot.open_interest);
    let ts_ns = match value_to_u64(data, &["timestamp"]) {
        Some(ts_ms) if ts_ms > 0 => ms_to_ns(ts_ms),
        _ => {
            log_parse_drop(
                "digifinex_collector",
                "missing_ts",
                &"missing timestamp",
                sample.as_str(),
            );
            return None;
        }
    };
    if let Some(prev) = prev {
        if prev.ticker.ts != 0 && ts_ns < prev.ticker.ts {
            log_parse_drop(
                "digifinex_collector",
                "stale_ts",
                &"stale ticker ts",
                sample.as_str(),
            );
            return None;
        }
    }
    snapshot.ticker.ts = ts_ns;
    snapshot.ticker.seq = prev
        .map(|s| s.ticker.seq.wrapping_add(1))
        .unwrap_or(1);
    let stored = store.update(inst_id.to_string(), snapshot);
    Some((inst_id.to_string(), stored))
}

#[cfg(test)]
mod tests {
    use super::is_buyer_maker_from_direction;

    #[test]
    fn maps_trade_direction_to_aggressor() {
        assert_eq!(is_buyer_maker_from_direction("1"), Some(false));
        assert_eq!(is_buyer_maker_from_direction("2"), Some(true));
        assert_eq!(is_buyer_maker_from_direction("3"), Some(true));
        assert_eq!(is_buyer_maker_from_direction("4"), Some(false));
        assert_eq!(is_buyer_maker_from_direction("9"), None);
    }

    #[test]
    fn direction_token_accepts_number_or_string() {
        use super::direction_token;
        use serde_json::json;
        assert_eq!(direction_token(&json!(1)).as_deref(), Some("1"));
        assert_eq!(direction_token(&json!("4")).as_deref(), Some("4"));
    }
}
