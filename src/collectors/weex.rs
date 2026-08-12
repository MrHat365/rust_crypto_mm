use crate::base_classes::bbo_store::BboStore;
use crate::base_classes::tickers::{TickerSnapshot, TickerStore};
use crate::base_classes::trades::{FixedTrades, Trade};
use crate::base_classes::types::{Price, Qty};
use crate::exchanges::weex::orderbook::{WeexBook, WeexDepthMsg, PRICE_SCALE, QTY_SCALE};
use crate::utils::parsing::log_parse_drop;
use crate::utils::time::ms_to_ns;
use serde_json::{self, Value};

pub fn events_for<const N: usize>(s: &str, book: &mut WeexBook<N>) -> Vec<(&'static str, f64)> {
    let mut out = Vec::with_capacity(1);
    let value: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(err) => {
            log_parse_drop("weex_collector", "json", &err, s);
            return out;
        }
    };
    let event = match value.get("e").and_then(|v| v.as_str()) {
        Some(e) => e,
        None => return out,
    };
    match event {
        "depth" => {
            match serde_json::from_str::<WeexDepthMsg>(s) {
                Ok(msg) => {
                    if book.apply(&msg) {
                        if let Some(mid) = book.mid_price_f64() {
                            out.push(("orderbook", mid));
                        }
                    }
                }
                Err(err) => log_parse_drop("weex_collector", "depth", &err, s),
            }
        }
        "ticker" => {
            if let Some(data) = value.get("d").and_then(|d| d.as_array()).and_then(|a| a.first()) {
                let bid = parse_f64(data, "b");
                let ask = parse_f64(data, "a");
                let bid_qty = parse_f64(data, "B");
                let ask_qty = parse_f64(data, "A");
                let ts_ms = value.get("E").and_then(|v| v.as_u64()).unwrap_or(0);
                if let (Some(bid), Some(ask), Some(bid_qty), Some(ask_qty)) =
                    (bid, ask, bid_qty, ask_qty)
                {
                    let seq = ts_ms;
                    if book.apply_bbo(bid, bid_qty, ask, ask_qty, seq, ts_ms) {
                        if let Some(mid) = book.mid_price_f64() {
                            out.push(("bbo", mid));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    out
}

pub fn update_bbo_store(s: &str, store: &mut BboStore) -> bool {
    let value: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(err) => {
            log_parse_drop("weex_collector", "json", &err, s);
            return false;
        }
    };
    if value.get("e").and_then(|v| v.as_str()) != Some("ticker") {
        return false;
    }
    let symbol = value
        .get("s")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN");
    let data = match value.get("d").and_then(|d| d.as_array()).and_then(|a| a.first()) {
        Some(d) => d,
        None => return false,
    };
    let bid = match parse_f64(data, "b").or_else(|| parse_f64(data, "bidPrice")) {
        Some(v) => v,
        None => return false,
    };
    let ask = match parse_f64(data, "a").or_else(|| parse_f64(data, "askPrice")) {
        Some(v) => v,
        None => return false,
    };
    let bid_qty = parse_f64(data, "B").unwrap_or(0.0);
    let ask_qty = parse_f64(data, "A").unwrap_or(0.0);
    let ts_ms = value.get("E").and_then(|v| v.as_u64()).unwrap_or(0);
    if ts_ms == 0 {
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
            log_parse_drop("weex_collector", "json", &err, s);
            return 0;
        }
    };
    if value.get("e").and_then(|v| v.as_str()) != Some("trade") {
        return 0;
    }
    let system_ts_ms = value.get("E").and_then(|v| v.as_u64()).unwrap_or(0);
    let data = match value.get("d").and_then(|d| d.as_array()) {
        Some(arr) => arr,
        None => return 0,
    };
    let mut inserted = 0usize;
    for entry in data {
        let price = entry
            .get("p")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| v.is_finite());
        let size = entry
            .get("q")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| v.is_finite());
        let (price, size) = match (price, size) {
            (Some(p), Some(q)) => (p, q),
            _ => continue,
        };
        let ts_ms = entry
            .get("T")
            .and_then(|v| v.as_u64())
            .unwrap_or(system_ts_ms);
        let is_buyer_maker = entry
            .get("m")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let seq = entry
            .get("t")
            .and_then(|v| match v {
                Value::Number(n) => n.as_u64(),
                Value::String(s) => s.parse::<u64>().ok(),
                _ => None,
            })
            .unwrap_or(ts_ms);
        let px_i = (price * PRICE_SCALE).round() as Price;
        let qty_i = (size * QTY_SCALE).round() as Qty;
        let trade = Trade::new(
            px_i,
            qty_i,
            ms_to_ns(ts_ms),
            seq,
            is_buyer_maker,
            Some(ms_to_ns(system_ts_ms)),
        );
        trades.push(trade);
        inserted += 1;
    }
    inserted
}

pub fn update_tickers(s: &str, store: &mut TickerStore) -> Option<(String, TickerSnapshot)> {
    let value: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(err) => {
            log_parse_drop("weex_collector", "json", &err, s);
            return None;
        }
    };
    if value.get("e").and_then(|v| v.as_str()) != Some("ticker") {
        return None;
    }
    let symbol = value.get("s").and_then(|v| v.as_str())?.to_string();
    let data = value.get("d").and_then(|d| d.as_array())?.first()?;
    let mut snapshot = store.get(&symbol).copied().unwrap_or_default();
    if let Some(last) = parse_f64(data, "c") {
        snapshot.ticker.last_px = (last * PRICE_SCALE).round() as Price;
    }
    if let Some(mark) = parse_f64(data, "m") {
        snapshot.mark_px = Some(mark);
    }
    if let Some(index) = parse_f64(data, "i") {
        snapshot.index_px = Some(index);
    }
    let ts_ms = value.get("E").and_then(|v| v.as_u64()).unwrap_or(0);
    if ts_ms == 0 {
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
