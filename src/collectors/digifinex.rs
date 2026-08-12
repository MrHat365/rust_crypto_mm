use crate::base_classes::bbo_store::BboStore;
use crate::base_classes::tickers::{TickerSnapshot, TickerStore};
use crate::base_classes::trades::{FixedTrades, Trade};
use crate::base_classes::types::{Price, Qty};
use crate::exchanges::digifinex::orderbook::{DigifinexBook, PRICE_SCALE, QTY_SCALE};
use crate::utils::parsing::log_parse_drop;
use crate::utils::time::ms_to_ns;
use serde_json::{self, Value};

pub fn events_for<const N: usize>(
    s: &str,
    book: &mut DigifinexBook<N>,
) -> Vec<(&'static str, f64)> {
    let mut out = Vec::with_capacity(1);
    let value: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(err) => {
            log_parse_drop("digifinex_collector", "json", &err, s);
            return out;
        }
    };
    let event = match value.get("event").and_then(|v| v.as_str()) {
        Some(e) => e,
        None => return out,
    };
    if event == "depth.update" {
        match serde_json::from_str::<crate::exchanges::digifinex::orderbook::DigifinexDepthMsg>(s) {
            Ok(msg) => {
                if book.apply(&msg) {
                    if let Some(mid) = book.mid_price_f64() {
                        out.push(("orderbook", mid));
                    }
                }
            }
            Err(err) => log_parse_drop("digifinex_collector", "depth", &err, s),
        }
    } else if event == "ticker.update" {
        if let Some(data) = value.get("data") {
            let bid = parse_f64(data, "best_bid");
            let ask = parse_f64(data, "best_ask");
            let bid_qty = parse_f64(data, "best_bid_size");
            let ask_qty = parse_f64(data, "best_ask_size");
            let ts_ms = data.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
            if let (Some(bid), Some(ask), Some(bid_qty), Some(ask_qty)) =
                (bid, ask, bid_qty, ask_qty)
            {
                if book.apply_bbo_from_ticker(bid, bid_qty, ask, ask_qty, ts_ms) {
                    if let Some(mid) = book.mid_price_f64() {
                        out.push(("bbo", mid));
                    }
                }
            }
        }
    }
    out
}

pub fn update_bbo_store(s: &str, store: &mut BboStore) -> bool {
    let value: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(err) => {
            log_parse_drop("digifinex_collector", "json", &err, s);
            return false;
        }
    };
    if value.get("event").and_then(|v| v.as_str()) != Some("ticker.update") {
        return false;
    }
    let data = match value.get("data") {
        Some(d) => d,
        None => return false,
    };
    let symbol = data
        .get("instrument_id")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN");
    let bid = match parse_f64(data, "best_bid") {
        Some(v) => v,
        None => return false,
    };
    let ask = match parse_f64(data, "best_ask") {
        Some(v) => v,
        None => return false,
    };
    let bid_qty = parse_f64(data, "best_bid_size").unwrap_or(0.0);
    let ask_qty = parse_f64(data, "best_ask_size").unwrap_or(0.0);
    let ts_ms = data.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
    if ts_ms == 0 {
        log_parse_drop("digifinex_collector", "missing_ts", &"missing ts", s);
        return false;
    }
    let ts_ns = ms_to_ns(ts_ms);
    store.update(symbol, bid, bid_qty, ask, ask_qty, ts_ns, Some(ts_ns));
    true
}

pub fn update_trades<const N: usize>(s: &str, trades: &mut FixedTrades<N>) -> usize {
    let value: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(err) => {
            log_parse_drop("digifinex_collector", "json", &err, s);
            return 0;
        }
    };
    if value.get("event").and_then(|v| v.as_str()) != Some("trades.update") {
        return 0;
    }
    let data = match value.get("data").and_then(|d| d.as_array()) {
        Some(arr) => arr,
        None => return 0,
    };
    let mut inserted = 0usize;
    for entry in data {
        let price = match entry.get("price").and_then(|v| v.as_str()) {
            Some(s) => match s.parse::<f64>() {
                Ok(v) if v.is_finite() => v,
                _ => continue,
            },
            None => continue,
        };
        let size = match entry.get("volume").and_then(|v| v.as_str()) {
            Some(s) => match s.parse::<f64>() {
                Ok(v) if v.is_finite() => v,
                _ => continue,
            },
            None => continue,
        };
        let ts_ms = entry
            .get("trade_time")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if ts_ms == 0 {
            continue;
        }
        let direction = entry
            .get("direction")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // 1=Open Long, 2=Open Short, 3=Close Long, 4=Close Short
        let is_buyer_maker = matches!(direction, "2" | "3");
        let seq = entry
            .get("trade_id")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(ts_ms);
        let px_i = (price * PRICE_SCALE).round() as Price;
        let qty_i = (size * QTY_SCALE).round() as Qty;
        let trade = Trade::new(px_i, qty_i, ms_to_ns(ts_ms), seq, is_buyer_maker, Some(ms_to_ns(ts_ms)));
        trades.push(trade);
        inserted += 1;
    }
    inserted
}

pub fn update_tickers(s: &str, store: &mut TickerStore) -> Option<(String, TickerSnapshot)> {
    let value: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(err) => {
            log_parse_drop("digifinex_collector", "json", &err, s);
            return None;
        }
    };
    if value.get("event").and_then(|v| v.as_str()) != Some("ticker.update") {
        return None;
    }
    let data = value.get("data")?;
    let symbol = data
        .get("instrument_id")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    let mut snapshot = store.get(&symbol).copied().unwrap_or_default();
    if let Some(last) = parse_f64(data, "last") {
        snapshot.ticker.last_px = (last * PRICE_SCALE).round() as Price;
    }
    if let Some(last_qty) = parse_f64(data, "last_qty") {
        snapshot.ticker.last_qty = (last_qty * QTY_SCALE).round() as Qty;
    }
    if let Some(bid) = parse_f64(data, "best_bid") {
        snapshot.ticker.best_bid = (bid * PRICE_SCALE).round() as Price;
    }
    if let Some(ask) = parse_f64(data, "best_ask") {
        snapshot.ticker.best_ask = (ask * PRICE_SCALE).round() as Price;
    }
    let ts_ms = data.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
    if ts_ms == 0 {
        log_parse_drop("digifinex_collector", "missing_ts", &"missing ts", s);
        return None;
    }
    snapshot.ticker.seq = ts_ms;
    snapshot.ticker.ts = ms_to_ns(ts_ms);
    let stored = store.update(symbol.clone(), snapshot);
    Some((symbol, stored))
}

fn parse_f64(value: &Value, key: &str) -> Option<f64> {
    match value.get(key)? {
        Value::String(s) => {
            let v = s.parse::<f64>().ok()?;
            if v.is_finite() { Some(v) } else { None }
        }
        Value::Number(n) => n.as_f64().filter(|v| v.is_finite()),
        _ => None,
    }
}
