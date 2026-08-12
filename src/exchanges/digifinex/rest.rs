#![allow(dead_code)]

/// Convert a generic symbol (e.g. `BTCUSDT`, `BTC_USDT`) to Digifinex perp instrument id.
pub fn to_instrument_id(symbol: &str) -> String {
    let normalized = symbol.replace('_', "").to_ascii_uppercase();
    if normalized.ends_with("PERP") {
        normalized
    } else if normalized.ends_with("USDT") {
        format!("{normalized}PERP")
    } else {
        format!("{normalized}USDTPERP")
    }
}

/// Check whether the instrument exists on Digifinex swap.
pub async fn fetch_instrument_supported(instrument_id: &str) -> bool {
    let url = format!(
        "https://openapi.digifinex.com/swap/v2/public/instrument?instrument_id={instrument_id}"
    );
    let client = reqwest::Client::new();
    let resp = match client.get(url).send().await {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!("Digifinex instrument check request failed for {instrument_id}: {err}");
            return false;
        }
    };
    if !resp.status().is_success() {
        eprintln!(
            "Digifinex instrument check HTTP {} for {instrument_id}",
            resp.status()
        );
        return false;
    }
    let value: serde_json::Value = match resp.json().await {
        Ok(json) => json,
        Err(err) => {
            eprintln!("Digifinex instrument check JSON decode failed for {instrument_id}: {err}");
            return false;
        }
    };
    value.get("code").and_then(|c| c.as_i64()) == Some(0)
        && value.get("data").is_some()
}
