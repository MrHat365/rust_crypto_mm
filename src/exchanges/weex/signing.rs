use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub fn sign_base64(secret: &str, message: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts arbitrary key sizes");
    mac.update(message.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

pub fn rest_message(
    timestamp_ms: &str,
    method: &str,
    request_path: &str,
    query_string: &str,
    body: &str,
) -> String {
    let query = if query_string.is_empty() {
        String::new()
    } else if query_string.starts_with('?') {
        query_string.to_string()
    } else {
        format!("?{query_string}")
    };
    format!(
        "{}{}{}{}{}",
        timestamp_ms,
        method.to_ascii_uppercase(),
        request_path,
        query,
        body
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_is_prefixed_exactly_once() {
        assert_eq!(
            rest_message("1", "get", "/capi/v3/orders", "symbol=BTCUSDT", ""),
            "1GET/capi/v3/orders?symbol=BTCUSDT"
        );
        assert_eq!(
            rest_message("1", "GET", "/capi/v3/orders", "?symbol=BTCUSDT", ""),
            "1GET/capi/v3/orders?symbol=BTCUSDT"
        );
    }
}
