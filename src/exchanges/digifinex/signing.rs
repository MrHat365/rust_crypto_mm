//! DigiFinex private REST signing (HMAC-SHA256 hex).

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Lowercase hex encode.
pub fn hex_bytes(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// Sign DigiFinex private request.
///
/// prehash = `timestamp + METHOD + requestPath + body`
/// ACCESS-SIGN = hex(HMAC_SHA256(secret, prehash))
pub fn sign_request(secret: &str, timestamp_ms: &str, method: &str, request_path: &str, body: &str) -> String {
    let mut prehash = String::with_capacity(
        timestamp_ms.len() + method.len() + request_path.len() + body.len(),
    );
    prehash.push_str(timestamp_ms);
    prehash.push_str(method);
    prehash.push_str(request_path);
    prehash.push_str(body);

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 accepts any key size");
    mac.update(prehash.as_bytes());
    hex_bytes(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_is_deterministic_hex() {
        let sig = sign_request(
            "secret",
            "1657522488402",
            "GET",
            "/swap/v2/account/positions?instrument_id=BTCUSDTPERP",
            "",
        );
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        let sig2 = sign_request(
            "secret",
            "1657522488402",
            "GET",
            "/swap/v2/account/positions?instrument_id=BTCUSDTPERP",
            "",
        );
        assert_eq!(sig, sig2);
    }
}
