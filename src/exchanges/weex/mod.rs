//! WEEX contract (USDT-margined perpetual) market-data integration (API V3).

pub mod orderbook;
pub mod parser;
pub mod rest;

pub use orderbook::{WeexBook, WeexDepthMsg};
pub use parser::{WeexFrame, WeexHandler};
pub use rest::*;
