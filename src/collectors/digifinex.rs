use crate::base_classes::bbo_store::BboStore;
use crate::base_classes::tickers::{TickerSnapshot, TickerStore};
use crate::base_classes::trades::{FixedTrades, Trade};
use crate::base_classes::types::{Price, Qty, Seq};
use crate::exchanges::digifinex::{DigiFinexBook, DigiFinexFrame};
use crate::exchanges::endpoints::DigiFinexWs;
use crate::utils::parsing::log_parse_drop;
use crate::utils::time::ms_to_ns;
use serde_json::Value;

pub const PRICE_SCALE: f64 = DigiFinexBook::<1>::PRICE_SCALE;
pub const QTY_SCALE: f64 = DigiFinexBook::<1>::QTY_SCALE;

pub fn events_for<const N: usize>(
    frame: &mut DigiFinexFrame,
    book: &mut DigiFinexBook<N>,
) -> Vec<(&'static str, f64)> {
    let mut out = Vec::with_capacity(1);
    if matches!(frame.event(), Some(DigiFinexWs::DEPTH_UPDATE)) {
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
                    "digifinex_collector",
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
                log_parse_drop("digifinex_collector", "non_finite", &"non-finite number", s);
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
        Value::Number(n) => {
            let v = n.as_u64();
            if v.is_none() {
                log_parse_drop(
                    "digifinex_collector",
                    "u64",
                    &"non-u64 number",
                    &n.to_string(),
                );
            }
            v
        }
        Value::String(s) => match s.parse::<u64>() {
            Ok(v) => Some(v),
            Err(err) => {
                log_parse_drop("digifinex_collector", "u64", &err, s);
                None
            }
        },
        _ => None,
    }
}

fn best_from_depth_side(levels: &Value, ascending: bool) -> Option<(f64, f64)> {
    let arr = levels.as_array()?;
    let mut best: Option<(f64, f64)> = None;
    for entry in arr {
        let pair = entry.as_array()?;
        let px = as_f64(pair.first()?)?;
        let qty = as_f64(pair.get(1)?)?;
        if qty <= 0.0 {
            continue;
        }
        best = match best {
            None => Some((px, qty)),
            Some((best_px, best_qty)) => {
                if ascending {
                    if px < best_px {
                        Some((px, qty))
                    } else {
                        Some((best_px, best_qty))
                    }
                } else if px > best_px {
                    Some((px, qty))
                } else {
                    Some((best_px, best_qty))
                }
            }
        };
    }
    best
}

pub fn update_bbo_store(
    frame: &mut DigiFinexFrame,
    store: &mut BboStore,
    qty_multiplier: f64,
) -> bool {
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
    let event = match raw.get("event").and_then(|v| v.as_str()) {
        Some(ev) => ev,
        None => return false,
    };

    let data = match raw.get("data") {
        Some(Value::Object(map)) => map,
        _ => return false,
    };

    let (instrument_id, bid_px, bid_qty, ask_px, ask_qty, ts_ms) = match event {
        DigiFinexWs::TICKER_UPDATE => {
            let instrument_id = match data.get("instrument_id").and_then(|v| v.as_str()) {
                Some(id) => id,
                None => {
                    log_parse_drop(
                        "digifinex_collector",
                        "missing_instrument_id",
                        &"missing instrument_id",
                        sample.as_str(),
                    );
                    return false;
                }
            };
            let bid_px = data.get("best_bid").and_then(as_f64).or_else(|| {
                log_parse_drop(
                    "digifinex_collector",
                    "missing_best_bid",
                    &"missing best_bid",
                    sample.as_str(),
                );
                None
            });
            let ask_px = data.get("best_ask").and_then(as_f64).or_else(|| {
                log_parse_drop(
                    "digifinex_collector",
                    "missing_best_ask",
                    &"missing best_ask",
                    sample.as_str(),
                );
                None
            });
            let bid_qty = data.get("best_bid_size").and_then(as_f64).unwrap_or(0.0);
            let ask_qty = data.get("best_ask_size").and_then(as_f64).unwrap_or(0.0);
            let ts_ms = data.get("timestamp").and_then(as_u64).or_else(|| {
                log_parse_drop(
                    "digifinex_collector",
                    "missing_ts",
                    &"missing ts",
                    sample.as_str(),
                );
                None
            });
            if bid_px.is_none() || ask_px.is_none() || ts_ms.is_none() {
                return false;
            }
            (
                instrument_id,
                bid_px.unwrap(),
                bid_qty,
                ask_px.unwrap(),
                ask_qty,
                ts_ms.unwrap(),
            )
        }
        DigiFinexWs::DEPTH_UPDATE => {
            let instrument_id = match data.get("instrument_id").and_then(|v| v.as_str()) {
                Some(id) => id,
                None => return false,
            };
            let bid = data.get("bids").and_then(|v| best_from_depth_side(v, false));
            let ask = data.get("asks").and_then(|v| best_from_depth_side(v, true));
            let (bid_px, bid_qty) = match bid {
                Some(v) => v,
                None => return false,
            };
            let (ask_px, ask_qty) = match ask {
                Some(v) => v,
                None => return false,
            };
            let ts_ms = match data.get("timestamp").and_then(as_u64) {
                Some(ts) if ts > 0 => ts,
                _ => return false,
            };
            (instrument_id, bid_px, bid_qty, ask_px, ask_qty, ts_ms)
        }
        _ => return false,
    };

    if bid_px <= 0.0 || ask_px <= 0.0 {
        return false;
    }

    let ts_ns = ms_to_ns(ts_ms);
    store.update(
        instrument_id,
        bid_px,
        bid_qty * qty_multiplier,
        ask_px,
        ask_qty * qty_multiplier,
        ts_ns,
        Some(ts_ns),
    );
    true
}

fn direction_is_buy_aggressor(direction: &str) -> bool {
    direction == "1" || direction == "4"
}

pub fn update_trades<const N: usize>(
    frame: &mut DigiFinexFrame,
    trades: &mut FixedTrades<N>,
    qty_multiplier: f64,
) -> usize {
    if !matches!(frame.event(), Some(DigiFinexWs::TRADES_UPDATE)) {
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
    let entries = match raw.get("data").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return 0,
    };

    let mut inserted = 0usize;
    for trade in entries {
        let price = trade.get("price").and_then(as_f64).or_else(|| {
            log_parse_drop(
                "digifinex_collector",
                "missing_price",
                &"missing price",
                sample.as_str(),
            );
            None
        });
        let qty = trade.get("volume").and_then(as_f64).or_else(|| {
            log_parse_drop(
                "digifinex_collector",
                "missing_qty",
                &"missing volume",
                sample.as_str(),
            );
            None
        });
        let direction = trade.get("direction").and_then(|v| v.as_str()).or_else(|| {
            log_parse_drop(
                "digifinex_collector",
                "missing_direction",
                &"missing direction",
                sample.as_str(),
            );
            None
        });
        let ts_ms = trade.get("trade_time").and_then(as_u64).or_else(|| {
            log_parse_drop(
                "digifinex_collector",
                "missing_ts",
                &"missing trade_time",
                sample.as_str(),
            );
            None
        });
        let seq = trade.get("trade_id").and_then(as_u64).or_else(|| {
            trade
                .get("trade_id")
                .and_then(|v| v.as_str())
                .and_then(|s| match s.parse::<u64>() {
                    Ok(v) => Some(v),
                    Err(err) => {
                        log_parse_drop("digifinex_collector", "seq", &err, s);
                        None
                    }
                })
                .or_else(|| {
                    log_parse_drop(
                        "digifinex_collector",
                        "missing_seq",
                        &"missing trade_id",
                        sample.as_str(),
                    );
                    None
                })
        });
        if price.is_none() || qty.is_none() || direction.is_none() || ts_ms.is_none() {
            continue;
        }
        let seq = match seq {
            Some(v) => v,
            None => continue,
        };
        let is_buyer_maker = !direction_is_buy_aggressor(direction.unwrap());
        let record = Trade::new(
            (price.unwrap() * PRICE_SCALE).round() as Price,
            (qty.unwrap() * qty_multiplier * QTY_SCALE).round() as Qty,
            ms_to_ns(ts_ms.unwrap()),
            seq as Seq,
            is_buyer_maker,
            None,
        );
        trades.push(record);
        inserted += 1;
    }
    inserted
}

pub fn update_tickers(
    frame: &mut DigiFinexFrame,
    store: &mut TickerStore,
    qty_multiplier: f64,
) -> Option<(String, TickerSnapshot)> {
    if !matches!(frame.event(), Some(DigiFinexWs::TICKER_UPDATE)) {
        return None;
    }
    let sample = frame.text().unwrap_or("").to_string();
    let raw = frame.json()?;
    let data = match raw.get("data") {
        Some(Value::Object(map)) => map,
        _ => return None,
    };
    let instrument_id = data
        .get("instrument_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            log_parse_drop(
                "digifinex_collector",
                "missing_instrument_id",
                &"missing instrument_id",
                sample.as_str(),
            );
            None
        })?
        .to_string();

    let mut snapshot = store.get(&instrument_id).copied().unwrap_or_default();

    if let Some(last_px) = data.get("last").and_then(as_f64) {
        snapshot.ticker.last_px = (last_px * PRICE_SCALE).round() as Price;
    }
    if let Some(bid_px) = data.get("best_bid").and_then(as_f64) {
        snapshot.ticker.best_bid = (bid_px * PRICE_SCALE).round() as Price;
    }
    if let Some(ask_px) = data.get("best_ask").and_then(as_f64) {
        snapshot.ticker.best_ask = (ask_px * PRICE_SCALE).round() as Price;
    }
    if let Some(last_qty) = data.get("last_qty").and_then(as_f64) {
        snapshot.ticker.last_qty = (last_qty * qty_multiplier * QTY_SCALE).round() as Qty;
    }
    if let Some(turnover) = data.get("volume_24h").and_then(as_f64) {
        snapshot.turnover_24h = Some(turnover);
    }
    if let Some(oi) = data
        .get("open_interest")
        .and_then(|v| v.as_str())
        .filter(|s| *s != "-")
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
    {
        snapshot.open_interest = Some(oi);
    }

    let ts_ms = match data.get("timestamp").and_then(as_u64) {
        Some(ts_ms) if ts_ms > 0 => ts_ms,
        _ => {
            log_parse_drop(
                "digifinex_collector",
                "missing_ts",
                &"missing ts",
                sample.as_str(),
            );
            return None;
        }
    };
    snapshot.ticker.ts = ms_to_ns(ts_ms);
    snapshot.ticker.seq = ts_ms;

    let stored = store.update(instrument_id.clone(), snapshot);
    Some((instrument_id, stored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchanges::digifinex::parser::normalize_instrument_id;
    use std::time::Instant;

    fn frame_from_str(json: &str) -> DigiFinexFrame {
        let mut frame = DigiFinexFrame::from_text(json, 0, Instant::now());
        frame.preparse_text(json);
        frame
    }

    #[test]
    fn update_trades_maps_direction_to_buyer_maker() {
        let mut frame = frame_from_str(
            r#"{
                "event":"trades.update",
                "data":[{
                    "instrument_id":"ETHUSDTPERP",
                    "trade_id":"1",
                    "trade_time":1662174608295,
                    "volume":"10",
                    "price":"100",
                    "direction":"4"
                },{
                    "instrument_id":"ETHUSDTPERP",
                    "trade_id":"2",
                    "trade_time":1662174608296,
                    "volume":"11",
                    "price":"101",
                    "direction":"2"
                }]
            }"#,
        );
        let mut trades = FixedTrades::<8>::default();
        let inserted = update_trades(&mut frame, &mut trades, 1.0);
        assert_eq!(inserted, 2);
        let items: Vec<_> = trades.iter_last(inserted).collect();
        assert!(!items[0].is_buyer_maker);
        assert!(items[1].is_buyer_maker);
    }

    #[test]
    fn normalize_symbol_matches_handler() {
        assert_eq!(normalize_instrument_id("BTCUSDT"), "BTCUSDTPERP");
    }
}
