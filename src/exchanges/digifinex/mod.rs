//! Digifinex USDT perpetual swap integration (market data).
//!
//! REST: https://docs.digifinex.com/en-ww/swap/v2/rest.html
//! WS:   https://docs.digifinex.com/en-ww/swap/v2/websocket.html
//!
//! Instrument ids look like `BTCUSDTPERP`. WS payloads are zlib-deflated.

pub mod orderbook;
pub mod parser;
pub mod rest;

pub use orderbook::DigifinexBook;
pub use parser::{DigifinexFrame, DigifinexHandler};
pub use rest::*;
