//! DigiFinex perpetual-swap V2 integration.

pub mod public_ws;

#[cfg(feature = "gate_exec")]
pub mod private_ws;
#[cfg(feature = "gate_exec")]
pub mod rest;
#[cfg(feature = "gate_exec")]
pub mod signing;

pub use public_ws::{DigiFinexFrame, DigiFinexHandler, DigiFinexMarketEvent};
#[cfg(feature = "gate_exec")]
pub use private_ws::{DigiFinexPrivateEvent, DigiFinexPrivateSubscription};
#[cfg(feature = "gate_exec")]
pub use rest::{
    DigiFinexClient, DigiFinexCredentials, DigiFinexOrderRequest, DigiFinexOrderType,
    DigiFinexPositionAction,
};
