#![cfg(feature = "gate_exec")]

use rust_test::exchanges::digifinex::{
    DigiFinexPrivateSubscription,
    signing::{hmac_sha256_base64, hmac_sha256_hex},
};
use rust_test::exchanges::weex::{
    WeexPrivateSubscription,
    signing::{rest_message, sign_base64},
};

#[test]
fn digifinex_signing_and_private_auth_are_deterministic() {
    assert_eq!(
        hmac_sha256_hex("secret", "123GET/swap/v2/account/balance"),
        "6781460d50492b853e45e62eb3e3df17825198ce05adfc729e56508020c0e7f5"
    );
    assert_eq!(
        hmac_sha256_base64("secret", "123"),
        "d9445LUOYYoOu5XbYeL0Jpc5FlnYLAZKX4G59I2FzNU="
    );
    let subscription = DigiFinexPrivateSubscription::new("key", "secret", "BTCUSDTPERP")
        .expect("valid DigiFinex credentials");
    let auth: serde_json::Value =
        serde_json::from_str(&subscription.auth_message(123, 7)).expect("valid auth JSON");
    assert_eq!(auth["id"], 7);
    assert_eq!(auth["signature"], hmac_sha256_base64("secret", "123"));
}

#[test]
fn weex_rest_and_websocket_signatures_follow_distinct_specs() {
    let message = rest_message(
        "1659076670000",
        "POST",
        "/capi/v3/order",
        "",
        r#"{"symbol":"BTCUSDT"}"#,
    );
    assert_eq!(
        message,
        r#"1659076670000POST/capi/v3/order{"symbol":"BTCUSDT"}"#
    );
    assert!(!sign_base64("secret", &message).is_empty());

    let subscription =
        WeexPrivateSubscription::new("key", "secret", "pass").expect("valid WEEX credentials");
    let headers = subscription
        .handshake_headers(1_659_076_670_000)
        .expect("valid handshake headers");
    assert_eq!(headers["access-key"], "key");
    assert_eq!(headers["access-passphrase"], "pass");
    assert_ne!(headers["access-sign"], sign_base64("secret", &message));
}
