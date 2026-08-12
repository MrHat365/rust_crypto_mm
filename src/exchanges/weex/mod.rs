//! WEEX contract V3 integration.

pub mod public_ws;

#[cfg(feature = "gate_exec")]
pub mod private_ws;
#[cfg(feature = "gate_exec")]
pub mod rest;
#[cfg(feature = "gate_exec")]
pub mod signing;

#[cfg(feature = "gate_exec")]
pub use private_ws::{WeexPrivateEvent, WeexPrivateSubscription};
pub use public_ws::{WeexFrame, WeexHandler, WeexMarketEvent};
#[cfg(feature = "gate_exec")]
pub use rest::{WeexClient, WeexCredentials, WeexOrderRequest, WeexPositionSide};
