use serde_json::Value;
use std::time::Duration;

use crate::utils::parsing::log_parse_drop;

#[derive(Debug, Clone, Default)]
pub struct DigifinexInstrumentMeta {
    pub instrument_id: String,
    pub contract_value: Option<f64>,
    pub contract_value_currency: Option<String>,
    pub tick_size: Option<f64>,
    pub min_order_amount: Option<f64>,
    pub is_trading: Option<bool>,
}

impl DigifinexInstrumentMeta {
    #[inline(always)]
    pub fn qty_multiplier(&self) -> Option<f64> {
        let contract_value = self.contract_value?;
        if !contract_value.is_finite() || contract_value <= 0.0 {
            return None;
        }
        Some(contract_value)
    }
}

pub fn normalize_instrument_id(symbol: &str) -> String {
    let compact = symbol
        .trim()
        .replace(['-', '/', '_'], "")
        .to_ascii_uppercase();
    if compact.ends_with("PERP") {
        compact
    } else if compact.ends_with("USDT") || compact.ends_with("USDC") || compact.ends_with("USD") {
        format!("{compact}PERP")
    } else {
        format!("{compact}USDTPERP")
    }
}

fn parse_f64(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => {
            let v = n.as_f64()?;
            if v.is_finite() {
                Some(v)
            } else {
                log_parse_drop(
                    "digifinex_rest",
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
                log_parse_drop("digifinex_rest", "non_finite", &"non-finite number", s);
                None
            }
            Err(err) => {
                log_parse_drop("digifinex_rest", "f64", &err, s);
                None
            }
        }
        _ => None,
    }
}

fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
}

/// Fetch a single instrument. `Ok(None)` means the symbol is not listed.
pub async fn fetch_instrument_meta(
    instrument_id: &str,
) -> Result<Option<DigifinexInstrumentMeta>, reqwest::Error> {
    let url = format!(
        "https://openapi.digifinex.com/swap/v2/public/instrument?instrument_id={instrument_id}"
    );
    let url = match reqwest::Url::parse(&url) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("ERROR: invalid Digifinex REST url {url}: {err}");
            return Ok(None);
        }
    };
    let client = http_client()?;
    let resp = client.get(url.clone()).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        eprintln!(
            "ERROR: Digifinex REST GET {} returned {} body=\"{}\"",
            url,
            status,
            body.chars().take(256).collect::<String>()
        );
        return Ok(None);
    }
    let value: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(err) => {
            log_parse_drop("digifinex_rest", "json", &err, &body);
            return Ok(None);
        }
    };
    let code = value.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        eprintln!(
            "ERROR: Digifinex REST GET {} returned code {:?} msg {:?}",
            url,
            value.get("code"),
            value.get("msg")
        );
        return Ok(None);
    }
    let data = match value.get("data") {
        Some(Value::Object(map)) => map,
        Some(Value::Null) | None => return Ok(None),
        Some(other) => {
            log_parse_drop(
                "digifinex_rest",
                "schema",
                &"expected data object",
                &other.to_string(),
            );
            return Ok(None);
        }
    };
    let id = data
        .get("instrument_id")
        .and_then(|v| v.as_str())
        .unwrap_or(instrument_id)
        .to_string();
    Ok(Some(DigifinexInstrumentMeta {
        instrument_id: id,
        contract_value: parse_f64(data.get("contract_value")),
        contract_value_currency: data
            .get("contract_value_currency")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        tick_size: parse_f64(data.get("tick_size")),
        min_order_amount: parse_f64(data.get("min_order_amount")),
        is_trading: data.get("is_trading").and_then(|v| v.as_bool()),
    }))
}

#[cfg(test)]
mod tests {
    use super::normalize_instrument_id;

    #[test]
    fn normalizes_common_symbol_forms() {
        assert_eq!(normalize_instrument_id("BTC_USDT"), "BTCUSDTPERP");
        assert_eq!(normalize_instrument_id("btc-usdt"), "BTCUSDTPERP");
        assert_eq!(normalize_instrument_id("BTCUSDT"), "BTCUSDTPERP");
        assert_eq!(normalize_instrument_id("BTCUSDTPERP"), "BTCUSDTPERP");
    }
}
