//! Sealing credentials at rest: base64(nonce || XChaCha20-Poly1305 ciphertext).
//!
//! The same scheme mcp-gateway uses for its vault. A random 24-byte nonce per
//! seal is safe with XChaCha's nonce size, so there is no counter to persist.

use anyhow::{Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, Generate, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

#[derive(Clone)]
pub struct Sealer(XChaCha20Poly1305);

impl Sealer {
    pub fn new(key: &[u8; 32]) -> Self {
        Self(XChaCha20Poly1305::new(key.into()))
    }

    pub fn seal(&self, plaintext: &str) -> String {
        let nonce = XNonce::generate();
        let ciphertext = self.0.encrypt(&nonce, plaintext.as_bytes()).expect("encryption is infallible");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ciphertext);
        STANDARD.encode(out)
    }

    pub fn open(&self, sealed: &str) -> Result<String> {
        let bytes = STANDARD.decode(sealed)?;
        if bytes.len() < 24 {
            return Err(anyhow!("sealed value too short"));
        }
        let (nonce, ciphertext) = bytes.split_at(24);
        let nonce = XNonce::try_from(nonce).map_err(|_| anyhow!("bad nonce"))?;
        let plaintext = self.0.decrypt(&nonce, ciphertext).map_err(|_| anyhow!("cannot unseal: wrong SEAL_KEY or corrupt value"))?;
        Ok(String::from_utf8(plaintext)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_another_key() {
        let a = Sealer::new(&[1; 32]);
        let sealed = a.seal("hunter2");
        assert_eq!(a.open(&sealed).unwrap(), "hunter2");
        assert_ne!(sealed, a.seal("hunter2"), "nonce must differ per seal");
        assert!(Sealer::new(&[2; 32]).open(&sealed).is_err());
    }
}
