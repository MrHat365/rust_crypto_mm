use serde_json::Value;
use std::time::Duration;

use crate::utils::parsing::log_parse_drop;

pub const REST_BASE: &str = "https://api-contract.weex.com";

#[derive(Debug, Clone, Default)]
pub struct WeexContractMeta {
    pub contract_val: Option<f64>,
}

impl WeexContractMeta {
    #[inline(always)]
    pub fn qty_multiplier(&self) -> Option<f64> {
        self.contract_val
    }
}

/// Normalize a user symbol into WEEX contract form: uppercase, no separators, no PERP suffix.
pub fn normalize_symbol<S: AsRef<str>>(symbol: S) -> String {
    let mut sym = symbol.as_ref().to_ascii_uppercase();
    sym = sym.replace('_', "").replace('-', "");
    if sym.ends_with("PERP") && sym.len() > 4 {
        sym = sym[..sym.len() - 4].to_string();
    }
    sym
}

fn parse_f64(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => {
            let v = n.as_f64()?;
            if v.is_finite() {
                Some(v)
            } else {
                log_parse_drop(
                    "weex_rest",
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

fn symbol_entry_matches(entry: &Value, symbol: &str) -> bool {
    entry
        .get("symbol")
        .and_then(|v| v.as_str())
        .map(|s| s.eq_ignore_ascii_case(symbol))
        .unwrap_or(false)
}

/// Fetch contract metadata for a WEEX USDT-margined perpetual.
///
/// Uses `GET /capi/v3/market/exchangeInfo?symbol={symbol}` and returns `Ok(Some(meta))` only
/// when the HTTP response proves the symbol exists. Returns `Ok(None)` when the symbol is
/// absent or the API reports it unsupported. Transport errors propagate as `reqwest::Error`.
pub async fn fetch_contract_meta(symbol: &str) -> Result<Option<WeexContractMeta>, reqwest::Error> {
    let normalized = normalize_symbol(symbol);
    let url = format!(
        "{}/capi/v3/market/exchangeInfo?symbol={normalized}",
        REST_BASE
    );
    let url = match reqwest::Url::parse(&url) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("ERROR: invalid WEEX REST url {url}: {err}");
            return Ok(None);
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("rust_test-weex/0.1")
        .build()?;

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

    let symbols = match value.get("symbols") {
        Some(Value::Array(arr)) => arr,
        _ => {
            eprintln!(
                "ERROR: WEEX REST GET {} missing symbols[] in response",
                url
            );
            return Ok(None);
        }
    };

    let entry = symbols
        .iter()
        .find(|item| symbol_entry_matches(item, &normalized));

    if entry.is_none() {
        eprintln!(
            "WARN: WEEX REST GET {} returned no entry for symbol {}; treating as unsupported",
            url, normalized
        );
        return Ok(None);
    }

    let contract_val = parse_f64(entry.and_then(|e| e.get("contractVal")));

    Ok(Some(WeexContractMeta { contract_val }))
}

/// Lightweight symbol existence check via public depth endpoint.
pub async fn symbol_exists_via_depth(symbol: &str) -> Result<bool, reqwest::Error> {
    let normalized = normalize_symbol(symbol);
    let url = format!(
        "{}/capi/v3/market/depth?symbol={normalized}&limit=15",
        REST_BASE
    );
    let url = match reqwest::Url::parse(&url) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("ERROR: invalid WEEX REST url {url}: {err}");
            return Ok(false);
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("rust_test-weex/0.1")
        .build()?;

    let resp = client.get(url.clone()).send().await?;
    let status = resp.status();
    let body = resp.text().await?;

    if status.as_u16() == 404 {
        return Ok(false);
    }

    if !status.is_success() {
        eprintln!(
            "ERROR: WEEX REST GET {} returned {} body=\"{}\"",
            url,
            status,
            body.chars().take(256).collect::<String>()
        );
        return Ok(false);
    }

    let value: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(err) => {
            log_parse_drop("weex_rest", "json", &err, &body);
            return Ok(false);
        }
    };

  let has_book = value.get("bids").and_then(|v| v.as_array()).is_some()
        && value.get("asks").and_then(|v| v.as_array()).is_some();

    Ok(has_book)
}
