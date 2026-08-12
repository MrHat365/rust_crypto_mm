use serde_json::Value;
use std::time::Duration;

use crate::exchanges::digifinex::parser::normalize_instrument_id;
use crate::exchanges::endpoints::DigiFinexGet;
use crate::utils::parsing::log_parse_drop;

#[derive(Debug, Clone, Default)]
pub struct DigiFinexInstrumentMeta {
    pub instrument_id: String,
    pub contract_value: Option<f64>,
    pub contract_value_currency: Option<String>,
    pub base_currency: Option<String>,
}

impl DigiFinexInstrumentMeta {
    #[inline(always)]
    pub fn qty_multiplier(&self) -> Option<f64> {
        let base = self.base_currency.as_ref()?.to_ascii_lowercase();
        if let Some(cv) = self.contract_value {
            if let Some(cv_ccy) = self.contract_value_currency.as_ref() {
                if cv_ccy.to_ascii_lowercase() == base {
                    return Some(cv);
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct DigiFinexDepthSnapshot {
    pub instrument_id: String,
    pub timestamp: u64,
    pub asks: Vec<(f64, f64)>,
    pub bids: Vec<(f64, f64)>,
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
        },
        _ => None,
    }
}

fn parse_string(value: Option<&Value>) -> Option<String> {
    value.and_then(|v| v.as_str().map(|s| s.to_string()))
}

fn parse_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => {
            let v = n.as_u64();
            if v.is_none() {
                log_parse_drop("digifinex_rest", "u64", &"non-u64 number", &n.to_string());
            }
            v
        }
        Value::String(s) => match s.parse::<u64>() {
            Ok(v) => Some(v),
            Err(err) => {
                log_parse_drop("digifinex_rest", "u64", &err, s);
                None
            }
        },
        _ => None,
    }
}

fn parse_contract_value(entry: &Value) -> Option<f64> {
    parse_f64(entry.get("contract_value")).or_else(|| parse_f64(entry.get("contract_val")))
}

fn meta_from_entry(instrument_id: &str, entry: &Value) -> DigiFinexInstrumentMeta {
    DigiFinexInstrumentMeta {
        instrument_id: entry
            .get("instrument_id")
            .and_then(|v| v.as_str())
            .unwrap_or(instrument_id)
            .to_string(),
        contract_value: parse_contract_value(entry),
        contract_value_currency: parse_string(entry.get("contract_value_currency"))
            .or_else(|| parse_string(entry.get("contract_val_currency"))),
        base_currency: parse_string(entry.get("base_currency")),
    }
}

fn rest_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
}

async fn get_json(url: reqwest::Url) -> Result<Option<Value>, reqwest::Error> {
    let client = rest_client()?;
    let resp = client.get(url.clone()).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        eprintln!(
            "ERROR: DigiFinex REST GET {} returned {} body=\"{}\"",
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
    Ok(Some(value))
}

fn validate_code(value: &Value, url: &reqwest::Url) -> bool {
    let code = value.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        eprintln!(
            "ERROR: DigiFinex REST GET {} returned code {:?}",
            url,
            value.get("code")
        );
        return false;
    }
    true
}

/// Fetch DigiFinex swap instrument metadata for contract sizing.
pub async fn fetch_instrument_meta(
    symbol: &str,
) -> Result<Option<DigiFinexInstrumentMeta>, reqwest::Error> {
    let instrument_id = normalize_instrument_id(symbol);
    let path = DigiFinexGet::instrument(&instrument_id);
    let url = match reqwest::Url::parse(&format!("{}{path}", DigiFinexGet::BASE)) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("ERROR: invalid DigiFinex REST url {path}: {err}");
            return Ok(None);
        }
    };

    let value = match get_json(url.clone()).await? {
        Some(value) => value,
        None => return Ok(None),
    };
    if !validate_code(&value, &url) {
        return Ok(None);
    }

    let entry = match value.get("data") {
        Some(entry @ Value::Object(_)) => entry,
        _ => {
            eprintln!(
                "ERROR: DigiFinex REST GET {} missing data object for {}",
                url, instrument_id
            );
            return Ok(None);
        }
    };

    Ok(Some(meta_from_entry(&instrument_id, entry)))
}

fn parse_depth_levels(levels: Option<&Value>, source: &str) -> Vec<(f64, f64)> {
    let Some(Value::Array(arr)) = levels else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let Some(pair) = entry.as_array() else {
            continue;
        };
        let px = parse_f64(pair.first());
        let qty = parse_f64(pair.get(1));
        if let (Some(px), Some(qty)) = (px, qty) {
            out.push((px, qty));
        } else {
            log_parse_drop(source, "depth_level", &"invalid depth level", &entry.to_string());
        }
    }
    out
}

/// Optional REST depth bootstrap snapshot.
pub async fn fetch_depth_snapshot(
    symbol: &str,
    limit: Option<u32>,
) -> Result<Option<DigiFinexDepthSnapshot>, reqwest::Error> {
    let instrument_id = normalize_instrument_id(symbol);
    let path = DigiFinexGet::depth(&instrument_id, limit);
    let url = match reqwest::Url::parse(&format!("{}{path}", DigiFinexGet::BASE)) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("ERROR: invalid DigiFinex REST url {path}: {err}");
            return Ok(None);
        }
    };

    let value = match get_json(url.clone()).await? {
        Some(value) => value,
        None => return Ok(None),
    };
    if !validate_code(&value, &url) {
        return Ok(None);
    }

    let data = match value.get("data") {
        Some(Value::Object(_)) => value.get("data").unwrap(),
        _ => {
            eprintln!(
                "ERROR: DigiFinex REST GET {} missing data object for {}",
                url, instrument_id
            );
            return Ok(None);
        }
    };

    let timestamp = match parse_u64(data.get("timestamp")) {
        Some(ts) if ts > 0 => ts,
        _ => {
            log_parse_drop(
                "digifinex_rest",
                "missing_ts",
                &"missing timestamp",
                &data.to_string(),
            );
            return Ok(None);
        }
    };

    Ok(Some(DigiFinexDepthSnapshot {
        instrument_id: data
            .get("instrument_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&instrument_id)
            .to_string(),
        timestamp,
        asks: parse_depth_levels(data.get("asks"), "digifinex_rest"),
        bids: parse_depth_levels(data.get("bids"), "digifinex_rest"),
    }))
}
