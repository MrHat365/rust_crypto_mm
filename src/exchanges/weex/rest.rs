use serde_json::Value;
use std::time::Duration;

use crate::utils::parsing::log_parse_drop;

#[derive(Debug, Clone, Default)]
pub struct WeexSnapshot {
    pub last_update_id: u64,
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
}

pub fn normalize_symbol(symbol: &str) -> String {
    symbol
        .trim()
        .replace(['-', '/', '_'], "")
        .to_ascii_uppercase()
}

fn parse_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64().filter(|v| v.is_finite()),
        Value::String(s) => match s.parse::<f64>() {
            Ok(v) if v.is_finite() => Some(v),
            Ok(_) => {
                log_parse_drop("weex_rest", "non_finite", &"non-finite number", s);
                None
            }
            Err(err) => {
                log_parse_drop("weex_rest", "f64", &err, s);
                None
            }
        },
        _ => None,
    }
}

fn parse_levels(value: Option<&Value>) -> Vec<(f64, f64)> {
    let Some(Value::Array(rows)) = value else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let pair = row.as_array()?;
            let px = parse_f64(pair.get(0)?)?;
            let qty = parse_f64(pair.get(1)?)?;
            if px <= 0.0 || qty < 0.0 {
                return None;
            }
            Some((px, qty))
        })
        .collect()
}

fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("rust-crypto-mm/0.1")
        .build()
}

/// Returns true when the contract appears in the public depth endpoint.
pub async fn symbol_supported(symbol: &str) -> bool {
    match fetch_depth_snapshot(symbol, 15).await {
        Ok(Some(snap)) => !snap.bids.is_empty() && !snap.asks.is_empty(),
        Ok(None) => false,
        Err(err) => {
            eprintln!("WEEX symbol check failed for {symbol}: {err}");
            false
        }
    }
}

pub async fn fetch_depth_snapshot(
    symbol: &str,
    limit: u32,
) -> Result<Option<WeexSnapshot>, reqwest::Error> {
    let url = format!(
        "https://api-contract.weex.com/capi/v3/market/depth?symbol={symbol}&limit={limit}"
    );
    let url = match reqwest::Url::parse(&url) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("ERROR: invalid WEEX REST url {url}: {err}");
            return Ok(None);
        }
    };
    let client = http_client()?;
    let resp = client.get(url.clone()).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        eprintln!(
            "ERROR: WEEX REST GET {} returned {} body=\"{}\"",
            url,
            status,
            body.chars().take(256).collect::<String>()
        );
        return Ok(None);
    }
    let value: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(err) => {
            log_parse_drop("weex_rest", "json", &err, &body);
            return Ok(None);
        }
    };
    if value.get("status").and_then(|s| s.as_u64()) == Some(404) {
        return Ok(None);
    }
    let last_update_id = value
        .get("lastUpdateId")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            value
                .get("lastUpdateId")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0);
    let bids = parse_levels(value.get("bids"));
    let asks = parse_levels(value.get("asks"));
    if bids.is_empty() && asks.is_empty() {
        return Ok(None);
    }
    Ok(Some(WeexSnapshot {
        last_update_id,
        bids,
        asks,
    }))
}

#[cfg(test)]
mod tests {
    use super::normalize_symbol;

    #[test]
    fn normalizes_symbol() {
        assert_eq!(normalize_symbol("BTC_USDT"), "BTCUSDT");
        assert_eq!(normalize_symbol("btc-usdt"), "BTCUSDT");
        assert_eq!(normalize_symbol("BTCUSDT"), "BTCUSDT");
    }
}
