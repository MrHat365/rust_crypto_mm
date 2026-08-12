//! DigiFinex REST execution gateway (swap v2).

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;

use super::digifinex_client::DigiFinexClient;
use super::gateway::ExecutionGateway;
use super::types::{
    ClientOrderId, ExchangeOrderId, ExecutionReport, OrderAck, OrderStatus, QuoteIntent,
    TimeInForce, Venue,
};
use crate::base_classes::types::Side;
use crate::exchanges::digifinex::normalize_instrument_id;

pub struct DigiFinexGateway {
    client: DigiFinexClient,
    instrument_id: String,
    /// client_order_id -> exchange_order_id
    live_orders: Mutex<HashMap<ClientOrderId, ExchangeOrderId>>,
}

impl DigiFinexGateway {
    pub fn new(client: DigiFinexClient, symbol: &str) -> Self {
        Self {
            client,
            instrument_id: normalize_instrument_id(symbol),
            live_orders: Mutex::new(HashMap::new()),
        }
    }

    fn map_side_to_type(side: Side, reduce_only: bool) -> u8 {
        // DigiFinex: 1 open long, 2 open short, 3 close long, 4 close short
        match (side, reduce_only) {
            (Side::Bid, false) => 1,
            (Side::Ask, false) => 2,
            (Side::Ask, true) => 3,
            (Side::Bid, true) => 4,
        }
    }

    fn map_tif(tif: TimeInForce, post_only: bool) -> (u8, bool) {
        // order_type: 0 limit, 4 IOC custom price, 9 FOK custom price
        match tif {
            TimeInForce::PostOnly => (0, true),
            TimeInForce::Ioc => (4, false),
            TimeInForce::Fok => (9, false),
            TimeInForce::Gtc => (0, post_only),
        }
    }
}

#[async_trait]
impl ExecutionGateway for DigiFinexGateway {
    async fn submit(&self, intents: &[QuoteIntent]) -> Result<Vec<OrderAck>> {
        let mut acks = Vec::with_capacity(intents.len());
        for intent in intents {
            if intent.venue != Venue::Digifinex {
                bail!(
                    "DigiFinexGateway got intent for venue {:?}",
                    intent.venue
                );
            }
            let reduce_only = intent
                .client_order_id
                .0
                .contains("reduce")
                || intent.client_order_id.0.contains("exit");
            let side_type = Self::map_side_to_type(intent.side, reduce_only);
            let (order_type, post_only) = Self::map_tif(intent.tif, false);
            let exchange_id = self
                .client
                .place_order(
                    &self.instrument_id,
                    side_type,
                    order_type,
                    intent.size,
                    Some(intent.price),
                    post_only,
                )
                .await?;
            let exchange_order_id = ExchangeOrderId(exchange_id);
            {
                let mut guard = self.live_orders.lock().map_err(|e| {
                    anyhow!("DigiFinexGateway live_orders lock poisoned: {e}")
                })?;
                guard.insert(intent.client_order_id.clone(), exchange_order_id.clone());
            }
            acks.push(OrderAck {
                client_order_id: intent.client_order_id.clone(),
                exchange_order_id: Some(exchange_order_id),
            });
        }
        Ok(acks)
    }

    async fn cancel_batch(&self, ids: &[ClientOrderId]) -> Result<()> {
        for id in ids {
            let exchange_id = {
                let guard = self.live_orders.lock().map_err(|e| {
                    anyhow!("DigiFinexGateway live_orders lock poisoned: {e}")
                })?;
                guard.get(id).cloned()
            };
            let Some(exchange_id) = exchange_id else {
                eprintln!(
                    "WARN: DigiFinex cancel skipped; unknown client_order_id={}",
                    id.0
                );
                continue;
            };
            self.client
                .cancel_order(&self.instrument_id, &exchange_id.0)
                .await?;
            let mut guard = self.live_orders.lock().map_err(|e| {
                anyhow!("DigiFinexGateway live_orders lock poisoned: {e}")
            })?;
            guard.remove(id);
        }
        Ok(())
    }

    async fn poll_reports(&self) -> Result<Vec<ExecutionReport>> {
        // REST polling snapshot of tracked orders. Not a push stream.
        let snapshot: Vec<(ClientOrderId, ExchangeOrderId)> = {
            let guard = self.live_orders.lock().map_err(|e| {
                anyhow!("DigiFinexGateway live_orders lock poisoned: {e}")
            })?;
            guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        if snapshot.is_empty() {
            return Ok(Vec::new());
        }
        let mut reports = Vec::new();
        for (client_id, exchange_id) in snapshot {
            let value = match self
                .client
                .order_info(&self.instrument_id, &exchange_id.0)
                .await
            {
                Ok(v) => v,
                Err(err) => {
                    eprintln!(
                        "WARN: DigiFinex order_info failed for {}: {:#}",
                        exchange_id.0, err
                    );
                    continue;
                }
            };
            let data = match value.get("data") {
                Some(d) => d,
                None => {
                    eprintln!(
                        "WARN: DigiFinex order_info missing data for {}",
                        exchange_id.0
                    );
                    continue;
                }
            };
            let status_raw = data
                .get("status")
                .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| {
                    v.as_i64().map(|n| n.to_string())
                }))
                .unwrap_or_else(|| "unknown".to_string());
            let status = match status_raw.to_ascii_lowercase().as_str() {
                "0" | "live" | "open" | "new" => OrderStatus::New,
                "1" | "partially_filled" | "partial" => OrderStatus::PartiallyFilled,
                "2" | "filled" | "done" => OrderStatus::Filled,
                "3" | "canceled" | "cancelled" => OrderStatus::Canceled,
                "4" | "rejected" => OrderStatus::Rejected,
                other => {
                    eprintln!(
                        "WARN: DigiFinex unknown order status '{}' for {}",
                        other, exchange_id.0
                    );
                    OrderStatus::Unknown
                }
            };
            let filled_qty = data
                .get("filled_qty")
                .or_else(|| data.get("filled_amount"))
                .or_else(|| data.get("deal_size"))
                .and_then(|v| {
                    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .unwrap_or(0.0);
            let avg_fill_price = data
                .get("avg_price")
                .or_else(|| data.get("price_avg"))
                .and_then(|v| {
                    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                });
            if matches!(
                status,
                OrderStatus::Filled | OrderStatus::Canceled | OrderStatus::Rejected
            ) {
                let mut guard = self.live_orders.lock().map_err(|e| {
                    anyhow!("DigiFinexGateway live_orders lock poisoned: {e}")
                })?;
                guard.remove(&client_id);
            }
            reports.push(ExecutionReport {
                client_order_id: client_id,
                exchange_order_id: Some(exchange_id),
                status,
                filled_qty,
                avg_fill_price,
                ts: None,
            });
        }
        Ok(reports)
    }
}
