#![cfg(feature = "gate_exec")]

use rust_test::exchanges::digifinex::{
    DigiFinexPrivateEvent, DigiFinexPrivateSubscription,
    private_ws::parse_private_message as parse_digifinex_private,
    signing::{hmac_sha256_base64, hmac_sha256_hex},
};
use rust_test::exchanges::weex::{
    WeexPrivateEvent, WeexPrivateSubscription,
    private_ws::parse_private_message as parse_weex_private,
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

    let subscription_message: serde_json::Value =
        serde_json::from_str(&subscription.subscribe_message(9)).expect("valid subscription JSON");
    assert_eq!(
        subscription_message["params"],
        serde_json::json!(["account", "positions", "orders", "fill"])
    );
    let event = parse_weex_private(
        r#"{"e":"orders","E":1773295738939,"v":46654,"msgEvent":"OrderUpdate","d":[]}"#,
    )
    .expect("valid versioned order event");
    assert!(matches!(
        event,
        WeexPrivateEvent::Orders { version: 46654, .. }
    ));
}

#[test]
fn digifinex_private_updates_match_documented_field_names() {
    let account = parse_digifinex_private(
        r#"{"event":"account.update","data":[{"equity":"100","currency":"USDT","margin":"1","frozen_margin":"2","realized_pnl":"3","avail_balance":"94","unrealized_pnl":"4","time_stamp":1662346135090}]}"#,
    )
    .expect("documented account update");
    let DigiFinexPrivateEvent::Account(accounts) = account else {
        panic!("expected account update");
    };
    assert_eq!(accounts[0].avail_balance, "94");

    let position = parse_digifinex_private(
        r#"{"event":"position.update","data":[{"instrument_id":"BTCUSDTPERP","margin_mode":"crossed","avail_position":"18","avg_cost":"13600.1","leverage":"20","position":"18","side":"long","timestamp":1662349557399}]}"#,
    )
    .expect("documented position update");
    let DigiFinexPrivateEvent::Positions(positions) = position else {
        panic!("expected position update");
    };
    assert_eq!(positions[0].position, "18");

    let order = parse_digifinex_private(
        r#"{"event":"order.update","data":[{"order_id":"1","instrument_id":"BTCUSDTPERP","price":"1998","size":"100","filled_qty":"8","price_avg":"1998","state":1,"time_stamp":1662068860901}]}"#,
    )
    .expect("documented order update");
    let DigiFinexPrivateEvent::Orders(orders) = order else {
        panic!("expected order update");
    };
    assert_eq!(orders[0].state, 1);
}
