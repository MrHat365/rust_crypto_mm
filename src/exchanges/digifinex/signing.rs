use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub fn hmac_sha256_hex(secret: &str, payload: &str) -> String {
    let bytes = hmac_sha256(secret, payload);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn hmac_sha256_base64(secret: &str, payload: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(hmac_sha256(secret, payload))
}

fn hmac_sha256(secret: &str, payload: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts arbitrary key sizes");
    mac.update(payload.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_rfc4231_sha256_vector() {
        let key = String::from_utf8(vec![0x0b; 20]).expect("ASCII-compatible key");
        assert_eq!(
            hmac_sha256_hex(&key, "Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hmac_sha256_base64(&key, "Hi There"),
            "sDRMYdjfOFNcqK/OrwvxK4gcwgDJO9OnJuk3wuMyz/c="
        );
    }
}
