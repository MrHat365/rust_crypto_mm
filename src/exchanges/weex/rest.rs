use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::{Client, Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::signing::{rest_message, sign_base64};

pub const WEEX_CONTRACT_ORIGIN: &str = "https://api-contract.weex.com";

#[derive(Debug, Clone)]
pub struct WeexCredentials {
    pub api_key: String,
    pub api_secret: String,
    pub passphrase: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WeexSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WeexPositionSide {
    Long,
    Short,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WeexOrderType {
    Limit,
    Market,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WeexTimeInForce {
    Gtc,
    Ioc,
    Fok,
    PostOnly,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeexOrderRequest {
    pub symbol: String,
    pub side: WeexSide,
    pub position_side: WeexPositionSide,
    #[serde(rename = "type")]
    pub order_type: WeexOrderType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_in_force: Option<WeexTimeInForce>,
    pub quantity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    pub new_client_order_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeexOrderAck {
    pub order_id: String,
    pub client_order_id: String,
    pub success: bool,
    pub error_code: String,
    pub error_message: String,
}

pub struct WeexClient {
    http: Client,
    credentials: WeexCredentials,
    origin: Url,
}

impl WeexClient {
    pub fn new(credentials: WeexCredentials) -> Result<Self> {
        Self::with_origin(credentials, WEEX_CONTRACT_ORIGIN)
    }

    pub fn with_origin(credentials: WeexCredentials, origin: &str) -> Result<Self> {
        if credentials.api_key.trim().is_empty()
            || credentials.api_secret.trim().is_empty()
            || credentials.passphrase.trim().is_empty()
        {
            bail!("WEEX credentials and passphrase must be non-empty");
        }
        let origin = Url::parse(origin).context("invalid WEEX REST origin")?;
        if origin.scheme() != "https" || origin.host_str().is_none() || origin.path() != "/" {
            bail!("WEEX REST origin must be an absolute https origin without an API path");
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("rust-crypto-mm/weex-v3")
            .build()
            .context("failed to build WEEX HTTP client")?;
        Ok(Self {
            http,
            credentials,
            origin,
        })
    }

    pub async fn exchange_info(&self, symbol: Option<&str>) -> Result<Value> {
        let query = symbol
            .map(|symbol| vec![("symbol", symbol.to_string())])
            .unwrap_or_default();
        self.request(
            Method::GET,
            "/capi/v3/market/exchangeInfo",
            &query,
            None,
            false,
        )
        .await
    }

    pub async fn depth(&self, symbol: &str, limit: u16) -> Result<Value> {
        if !matches!(limit, 15 | 200) {
            bail!("WEEX depth limit must be 15 or 200, got {limit}");
        }
        self.request(
            Method::GET,
            "/capi/v3/market/depth",
            &[
                ("symbol", symbol.to_string()),
                ("limit", limit.to_string()),
            ],
            None,
            false,
        )
        .await
    }

    pub async fn balances(&self) -> Result<Value> {
        self.request(
            Method::GET,
            "/capi/v3/account/balance",
            &[],
            None,
            true,
        )
        .await
    }

    pub async fn positions(&self, symbol: Option<&str>) -> Result<Value> {
        let (path, query) = match symbol {
            Some(symbol) => (
                "/capi/v3/account/position/singlePosition",
                vec![("symbol", symbol.to_string())],
            ),
            None => ("/capi/v3/account/position/allPosition", Vec::new()),
        };
        self.request(Method::GET, path, &query, None, true).await
    }

    pub async fn place_order(&self, order: &WeexOrderRequest) -> Result<WeexOrderAck> {
        validate_order(order)?;
        let value = self
            .request(
                Method::POST,
                "/capi/v3/order",
                &[],
                Some(serde_json::to_value(order)?),
                true,
            )
            .await?;
        let ack: WeexOrderAck =
            serde_json::from_value(value).context("invalid WEEX order acknowledgement schema")?;
        if !ack.success {
            bail!(
                "WEEX order rejected code={} message={}",
                ack.error_code,
                ack.error_message
            );
        }
        Ok(ack)
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        if order_id.trim().is_empty() {
            bail!("WEEX cancel order_id must be non-empty");
        }
        let value = self
            .request(
                Method::DELETE,
                "/capi/v3/order",
                &[("orderId", order_id.to_string())],
                None,
                true,
            )
            .await?;
        if value.get("success").and_then(Value::as_bool) != Some(true) {
            bail!("WEEX cancel was not successful: {}", truncate(&value.to_string()));
        }
        Ok(())
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        authenticated: bool,
    ) -> Result<Value> {
        if !path.starts_with("/capi/v3/") {
            bail!("invalid WEEX V3 API path '{path}'");
        }
        let query_string = serde_urlencoded::to_string(query)?;
        let request_path = if query_string.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{query_string}")
        };
        let url = self.origin.join(&request_path)?;
        let body_text = match body {
            Some(body) => serde_json::to_string(&body)?,
            None => String::new(),
        };
        let mut request = self.http.request(method.clone(), url.clone());
        if !body_text.is_empty() {
            request = request
                .header("Content-Type", "application/json")
                .body(body_text.clone());
        }
        if authenticated {
            let timestamp = now_ms()?.to_string();
            let message = rest_message(
                &timestamp,
                method.as_str(),
                path,
                &query_string,
                &body_text,
            );
            request = request
                .header("ACCESS-KEY", &self.credentials.api_key)
                .header(
                    "ACCESS-SIGN",
                    sign_base64(&self.credentials.api_secret, &message),
                )
                .header("ACCESS-PASSPHRASE", &self.credentials.passphrase)
                .header("ACCESS-TIMESTAMP", timestamp);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("WEEX {} {} request failed", method, url))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .with_context(|| format!("WEEX {} {} response read failed", method, url))?;
        if !status.is_success() {
            bail!(
                "WEEX {} {} returned HTTP {} body={}",
                method,
                url,
                status,
                truncate(&text)
            );
        }
        let value: Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "WEEX {} {} returned invalid JSON body={}",
                method,
                url,
                truncate(&text)
            )
        })?;
        if value.get("success").and_then(Value::as_bool) == Some(false) {
            bail!(
                "WEEX {} {} returned unsuccessful response body={}",
                method,
                url,
                truncate(&text)
            );
        }
        if let Some(code) = value.get("code").and_then(Value::as_str) {
            if code != "200" && code != "0" {
                bail!(
                    "WEEX {} {} returned code={} body={}",
                    method,
                    url,
                    code,
                    truncate(&text)
                );
            }
        }
        Ok(value)
    }
}

fn validate_order(order: &WeexOrderRequest) -> Result<()> {
    if order.symbol.trim().is_empty() {
        bail!("WEEX order symbol must be non-empty");
    }
    if order.new_client_order_id.is_empty()
        || order.new_client_order_id.len() > 36
        || !order
            .new_client_order_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".:/_-".contains(&byte))
    {
        bail!("WEEX new_client_order_id does not satisfy the documented 1-36 character format");
    }
    let quantity: f64 = order
        .quantity
        .parse()
        .context("WEEX order quantity must be numeric")?;
    if !quantity.is_finite() || quantity <= 0.0 {
        bail!("WEEX order quantity must be finite and > 0");
    }
    if matches!(order.order_type, WeexOrderType::Limit) {
        if order.time_in_force.is_none() {
            bail!("WEEX limit order requires time_in_force");
        }
        let price: f64 = order
            .price
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("WEEX limit order requires price"))?
            .parse()
            .context("WEEX order price must be numeric")?;
        if !price.is_finite() || price <= 0.0 {
            bail!("WEEX order price must be finite and > 0");
        }
    }
    Ok(())
}

fn now_ms() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_millis())
}

fn truncate(value: &str) -> String {
    value.chars().take(512).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_limit_without_tif() {
        let order = WeexOrderRequest {
            symbol: "BTCUSDT".to_string(),
            side: WeexSide::Buy,
            position_side: WeexPositionSide::Long,
            order_type: WeexOrderType::Limit,
            time_in_force: None,
            quantity: "0.01".to_string(),
            price: Some("100".to_string()),
            new_client_order_id: "test-1".to_string(),
        };
        assert!(validate_order(&order).is_err());
    }
}
