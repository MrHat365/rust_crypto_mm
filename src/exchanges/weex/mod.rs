//! WEEX USDT perpetual contract integration (market data).
//!
//! REST: https://www.weex.com/api-doc/contract/intro
//! WS:   wss://ws-contract.weex.com/v3/ws/public
//!
//! Public streams are Binance-like: `{SYMBOL}@depth15`, `{SYMBOL}@trade`, `{SYMBOL}@ticker`.

pub mod orderbook;
pub mod parser;
pub mod rest;

pub use orderbook::WeexBook;
pub use parser::{WeexFrame, WeexHandler};
pub use rest::*;
