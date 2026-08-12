//! DigiFinex authenticated REST client for swap v2 private endpoints.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{Client, Method, Url};
use serde_json::{json, Value};

use crate::exchanges::digifinex::signing::sign_request;
use crate::exchanges::endpoints::DigiFinexGet;
use crate::utils::parsing::log_parse_drop;

#[derive(Debug, Clone)]
pub struct DigiFinexCredentials {
    pub api_key: String,
    pub api_secret: String,
    pub base_url: String,
}

impl DigiFinexCredentials {
    pub fn new(api_key: impl Into<String>, api_secret: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            base_url: DigiFinexGet::BASE.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DigiFinexClient {
    credentials: DigiFinexCredentials,
    http: Client,
}

impl DigiFinexClient {
    pub fn new(credentials: DigiFinexCredentials) -> Result<Self> {
        if credentials.api_key.trim().is_empty() || credentials.api_secret.trim().is_empty() {
            bail!("DigiFinex credentials missing api_key/api_secret");
        }
        let base = Url::parse(&credentials.base_url)
            .with_context(|| format!("invalid DigiFinex base_url {}", credentials.base_url))?;
        if base.scheme() != "https" {
            bail!(
                "DigiFinex base_url must use https scheme, got {}",
                base.scheme()
            );
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("failed to build DigiFinex HTTP client")?;
        Ok(Self { credentials, http })
    }

    fn now_ms() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX_EPOCH")
            .as_millis()
            .to_string()
    }

    async fn request(&self, method: Method, path_with_query: &str, body: Option<&str>) -> Result<Value> {
        if !path_with_query.starts_with('/') {
            bail!("DigiFinex request path must start with '/', got {path_with_query}");
        }
        let url = format!("{}{}", self.credentials.base_url, path_with_query);
        let url = Url::parse(&url).with_context(|| format!("invalid DigiFinex url {url}"))?;
        let body_str = body.unwrap_or("");
        let ts = Self::now_ms();
        let method_uc = method.as_str().to_ascii_uppercase();
        // Signature uses path as `/swap/v2...` (docs include full API path).
        let sign_path = if path_with_query.starts_with("/swap/v2") {
            path_with_query.to_string()
        } else {
            format!("/swap/v2{path_with_query}")
        };
        let sign = sign_request(
            &self.credentials.api_secret,
            &ts,
            &method_uc,
            &sign_path,
            body_str,
        );

        let mut builder = self
            .http
            .request(method, url.clone())
            .header("ACCESS-KEY", &self.credentials.api_key)
            .header("ACCESS-SIGN", sign)
            .header("ACCESS-TIMESTAMP", ts)
            .header("Content-Type", "application/json");
        if !body_str.is_empty() {
            builder = builder.body(body_str.to_string());
        }
        let resp = builder.send().await.with_context(|| {
            format!("DigiFinex {} {} transport error", method_uc, path_with_query)
        })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .with_context(|| format!("DigiFinex {} {} body read failed", method_uc, path_with_query))?;
        if !status.is_success() {
            bail!(
                "DigiFinex {} {} HTTP {} body=\"{}\"",
                method_uc,
                path_with_query,
                status,
                text.chars().take(256).collect::<String>()
            );
        }
        let value: Value = serde_json::from_str(&text).map_err(|err| {
            log_parse_drop("digifinex_client", "json", &err, &text);
            anyhow!(
                "DigiFinex {} {} JSON parse failed: {err}; sample=\"{}\"",
                method_uc,
                path_with_query,
                text.chars().take(256).collect::<String>()
            )
        })?;
        let code = value
            .get("code")
            .and_then(|c| c.as_i64())
            .ok_or_else(|| {
                anyhow!(
                    "DigiFinex {} {} missing code; sample=\"{}\"",
                    method_uc,
                    path_with_query,
                    text.chars().take(256).collect::<String>()
                )
            })?;
        if code != 0 {
            bail!(
                "DigiFinex {} {} business code {} body=\"{}\"",
                method_uc,
                path_with_query,
                code,
                text.chars().take(256).collect::<String>()
            );
        }
        Ok(value)
    }

    /// Place a swap order. Returns exchange order id.
    pub async fn place_order(
        &self,
        instrument_id: &str,
        order_type_side: u8,
        order_type: u8,
        size: f64,
        price: Option<f64>,
        post_only: bool,
    ) -> Result<String> {
        if instrument_id.trim().is_empty() {
            bail!("DigiFinex place_order missing instrument_id");
        }
        if !size.is_finite() || size <= 0.0 {
            bail!("DigiFinex place_order invalid size {size}");
        }
        let mut body = json!({
            "instrument_id": instrument_id,
            "type": order_type_side,
            "order_type": order_type,
            "size": size,
            "post_only": post_only,
        });
        if let Some(px) = price {
            if !px.is_finite() || px <= 0.0 {
                bail!("DigiFinex place_order invalid price {px}");
            }
            body["price"] = json!(format!("{px}"));
        }
        let body_str = serde_json::to_string(&body)?;
        let value = self
            .request(Method::POST, "/trade/order_place", Some(&body_str))
            .await?;
        value
            .get("data")
            .and_then(|d| d.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("DigiFinex place_order missing data order id: {value}"))
    }

    pub async fn cancel_order(&self, instrument_id: &str, order_id: &str) -> Result<()> {
        if instrument_id.trim().is_empty() || order_id.trim().is_empty() {
            bail!("DigiFinex cancel_order missing instrument_id/order_id");
        }
        let body = json!({
            "instrument_id": instrument_id,
            "order_id": order_id,
        });
        let body_str = serde_json::to_string(&body)?;
        let _ = self
            .request(Method::POST, "/trade/cancel_order", Some(&body_str))
            .await?;
        Ok(())
    }

    pub async fn order_info(&self, instrument_id: &str, order_id: &str) -> Result<Value> {
        if instrument_id.trim().is_empty() || order_id.trim().is_empty() {
            bail!("DigiFinex order_info missing instrument_id/order_id");
        }
        let path = format!(
            "/trade/order_info?instrument_id={instrument_id}&order_id={order_id}"
        );
        self.request(Method::GET, &path, None).await
    }

    pub async fn open_orders(&self, instrument_id: &str) -> Result<Value> {
        if instrument_id.trim().is_empty() {
            bail!("DigiFinex open_orders missing instrument_id");
        }
        let path = format!("/trade/open_orders?instrument_id={instrument_id}");
        self.request(Method::GET, &path, None).await
    }

    pub async fn positions(&self, instrument_id: &str) -> Result<Value> {
        if instrument_id.trim().is_empty() {
            bail!("DigiFinex positions missing instrument_id");
        }
        let path = format!("/account/positions?instrument_id={instrument_id}");
        self.request(Method::GET, &path, None).await
    }
}
