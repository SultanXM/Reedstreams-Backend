use hex;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

pub struct SignatureUtil {
    secret: String,
}

impl SignatureUtil {
    pub fn new(secret: String) -> Self {
        Self { secret }
    }

    /// sig is based on: client_id + expiry + url + secret
    /// client_id is a hash of IP + User-Agent
    pub fn generate_signature(&self, client_id: &str, expiry: i64, url: &str) -> String {
        let message = format!("{}{}{}", client_id, expiry, url);

        let mut mac = HmacSha256::new_from_slice(self.secret.as_bytes())
            .expect("HMAC can take key of any size");

        mac.update(message.as_bytes());

        let result = mac.finalize();
        let code_bytes = result.into_bytes();

        hex::encode(code_bytes)
    }

    pub fn verify_signature(
        &self,
        client_id: &str,
        expiry: i64,
        url: &str,
        signature: &str,
    ) -> bool {
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        if current_time > expiry {
            return false;
        }

        // see if we can regenerate the signature, if we can then it's valid
        let expected_signature = self.generate_signature(client_id, expiry, url);

        signature.len() == expected_signature.len()
            && signature
                .as_bytes()
                .iter()
                .zip(expected_signature.as_bytes().iter())
                .fold(0, |acc, (a, b)| acc | (a ^ b))
                == 0
    }

    pub fn generate_expiry(hours: i64) -> i64 {
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        current_time + (hours * 3600)
    }
}
