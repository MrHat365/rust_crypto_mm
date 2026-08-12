use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::{Client, Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::signing::hmac_sha256_hex;

pub const DIGIFINEX_SWAP_ORIGIN: &str = "https://openapi.digifinex.com";

#[derive(Debug, Clone)]
pub struct DigiFinexCredentials {
    pub api_key: String,
    pub api_secret: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(into = "u8")]
pub enum DigiFinexPositionAction {
    OpenLong,
    OpenShort,
    CloseLong,
    CloseShort,
}

impl From<DigiFinexPositionAction> for u8 {
    fn from(value: DigiFinexPositionAction) -> Self {
        match value {
            DigiFinexPositionAction::OpenLong => 1,
            DigiFinexPositionAction::OpenShort => 2,
            DigiFinexPositionAction::CloseLong => 3,
            DigiFinexPositionAction::CloseShort => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(into = "u8")]
pub enum DigiFinexOrderType {
    Limit,
    IocCustomPrice,
    FokCustomPrice,
}

impl From<DigiFinexOrderType> for u8 {
    fn from(value: DigiFinexOrderType) -> Self {
        match value {
            DigiFinexOrderType::Limit => 0,
            DigiFinexOrderType::IocCustomPrice => 4,
            DigiFinexOrderType::FokCustomPrice => 9,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DigiFinexOrderRequest {
    pub instrument_id: String,
    #[serde(rename = "type")]
    pub action: DigiFinexPositionAction,
    pub order_type: DigiFinexOrderType,
    pub size: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    pub post_only: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DigiFinexOrderAck {
    pub order_id: String,
}

pub struct DigiFinexClient {
    http: Client,
    credentials: DigiFinexCredentials,
    origin: Url,
}

impl DigiFinexClient {
    pub fn new(credentials: DigiFinexCredentials) -> Result<Self> {
        Self::with_origin(credentials, DIGIFINEX_SWAP_ORIGIN)
    }

    pub fn with_origin(credentials: DigiFinexCredentials, origin: &str) -> Result<Self> {
        if credentials.api_key.trim().is_empty() || credentials.api_secret.trim().is_empty() {
            bail!("DigiFinex API credentials must be non-empty");
        }
        let origin = Url::parse(origin).context("invalid DigiFinex REST origin")?;
        if origin.scheme() != "https" || origin.host_str().is_none() {
            bail!("DigiFinex REST origin must be an absolute https URL");
        }
        if origin.path() != "/" {
            bail!("DigiFinex REST origin must not include an API path");
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("rust-crypto-mm/digifinex-v2")
            .build()
            .context("failed to build DigiFinex HTTP client")?;
        Ok(Self {
            http,
            credentials,
            origin,
        })
    }

    pub async fn instruments(&self) -> Result<Value> {
        self.public_get("/swap/v2/public/instruments", &[]).await
    }

    pub async fn depth(&self, instrument_id: &str, limit: u16) -> Result<Value> {
        if !(1..=100).contains(&limit) {
            bail!("DigiFinex depth limit must be in 1..=100, got {limit}");
        }
        self.public_get(
            "/swap/v2/public/depth",
            &[
                ("instrument_id", instrument_id.to_string()),
                ("limit", limit.to_string()),
            ],
        )
        .await
    }

    pub async fn balances(&self, currency: Option<&str>) -> Result<Value> {
        let query = currency
            .map(|currency| vec![("currency", currency.to_string())])
            .unwrap_or_default();
        self.private_request(Method::GET, "/swap/v2/account/balance", &query, None)
            .await
    }

    pub async fn positions(&self, instrument_id: Option<&str>) -> Result<Value> {
        let query = instrument_id
            .map(|instrument_id| vec![("instrument_id", instrument_id.to_string())])
            .unwrap_or_default();
        self.private_request(Method::GET, "/swap/v2/account/positions", &query, None)
            .await
    }

    pub async fn place_order(
        &self,
        order: &DigiFinexOrderRequest,
    ) -> Result<DigiFinexOrderAck> {
        validate_order(order)?;
        let value = self
            .private_request(
                Method::POST,
                "/swap/v2/trade/order_place",
                &[],
                Some(serde_json::to_value(order)?),
            )
            .await?;
        serde_json::from_value(value)
            .context("DigiFinex order response did not contain a valid order_id")
    }

    pub async fn cancel_order(&self, instrument_id: &str, order_id: &str) -> Result<()> {
        if instrument_id.trim().is_empty() || order_id.trim().is_empty() {
            bail!("DigiFinex cancel requires non-empty instrument_id and order_id");
        }
        self.private_request(
            Method::POST,
            "/swap/v2/trade/cancel_order",
            &[],
            Some(serde_json::json!({
                "instrument_id": instrument_id,
                "order_id": order_id,
            })),
        )
        .await?;
        Ok(())
    }

    pub async fn open_orders(&self, instrument_id: Option<&str>) -> Result<Value> {
        let query = instrument_id
            .map(|instrument_id| vec![("instrument_id", instrument_id.to_string())])
            .unwrap_or_default();
        self.private_request(Method::GET, "/swap/v2/trade/open_orders", &query, None)
            .await
    }

    async fn public_get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.request(Method::GET, path, query, None, false).await
    }

    async fn private_request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        self.request(method, path, query, body, true).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        authenticated: bool,
    ) -> Result<Value> {
        if !path.starts_with("/swap/v2/") {
            bail!("invalid DigiFinex API path '{path}'");
        }
        let query_string = serde_urlencoded::to_string(query)?;
        let request_path = if query_string.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{query_string}")
        };
        let url = self
            .origin
            .join(&request_path)
            .with_context(|| format!("invalid DigiFinex request path {request_path}"))?;
        let body_text = match body {
            Some(value) => serde_json::to_string(&value)?,
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
            let prehash = format!(
                "{}{}{}{}",
                timestamp,
                method.as_str(),
                request_path,
                body_text
            );
            request = request
                .header("ACCESS-KEY", &self.credentials.api_key)
                .header(
                    "ACCESS-SIGN",
                    hmac_sha256_hex(&self.credentials.api_secret, &prehash),
                )
                .header("ACCESS-TIMESTAMP", timestamp);
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("DigiFinex {} {} request failed", method, url))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .with_context(|| format!("DigiFinex {} {} body read failed", method, url))?;
        if !status.is_success() {
            bail!(
                "DigiFinex {} {} returned HTTP {} body={}",
                method,
                url,
                status,
                truncate(&text)
            );
        }
        let envelope: Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "DigiFinex {} {} returned invalid JSON body={}",
                method,
                url,
                truncate(&text)
            )
        })?;
        let code = envelope
            .get("code")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("DigiFinex response missing integer code"))?;
        if code != 0 {
            bail!(
                "DigiFinex {} {} returned code={} body={}",
                method,
                url,
                code,
                truncate(&text)
            );
        }
        envelope
            .get("data")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("DigiFinex successful response missing data"))
    }
}

fn validate_order(order: &DigiFinexOrderRequest) -> Result<()> {
    if order.instrument_id.trim().is_empty() {
        bail!("DigiFinex order instrument_id must be non-empty");
    }
    if !order.size.is_finite() || order.size <= 0.0 {
        bail!("DigiFinex order size must be finite and > 0");
    }
    if matches!(order.order_type, DigiFinexOrderType::Limit)
        && order
            .price
            .as_deref()
            .is_none_or(|price| price.trim().is_empty())
    {
        bail!("DigiFinex limit order requires price");
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
