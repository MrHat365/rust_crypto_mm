#![allow(dead_code)]

pub fn normalize_symbol(symbol: &str) -> String {
    symbol.replace('_', "").to_ascii_uppercase()
}

pub async fn fetch_symbol_supported(symbol: &str) -> bool {
    let symbol = normalize_symbol(symbol);
    let url = format!(
        "https://api-contract.weex.com/capi/v3/market/depth?symbol={symbol}&limit=15"
    );
    let client = reqwest::Client::new();
    let resp = match client.get(url).send().await {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!("Weex symbol check request failed for {symbol}: {err}");
            return false;
        }
    };
    if !resp.status().is_success() {
        eprintln!("Weex symbol check HTTP {} for {symbol}", resp.status());
        return false;
    }
    let value: serde_json::Value = match resp.json().await {
        Ok(json) => json,
        Err(err) => {
            eprintln!("Weex symbol check JSON decode failed for {symbol}: {err}");
            return false;
        }
    };
    value.get("bids").and_then(|v| v.as_array()).is_some()
        && value.get("asks").and_then(|v| v.as_array()).is_some()
}
