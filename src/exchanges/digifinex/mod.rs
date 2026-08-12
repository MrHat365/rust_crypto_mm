//! DigiFinex perpetual swap exchange integration
//!
//! Provides websocket parser, orderbook maintenance, and REST helpers for DigiFinex USDT-margined swaps.

#![allow(dead_code)]

pub mod orderbook;
pub mod parser;
pub mod rest;
#[cfg(feature = "gate_exec")]
pub mod signing;

pub use orderbook::{DigiFinexBook, DigiFinexDepthMsg};
pub use parser::{normalize_instrument_id, DigiFinexFrame, DigiFinexHandler};
pub use rest::*;
#[cfg(feature = "gate_exec")]
pub use signing::sign_request;
