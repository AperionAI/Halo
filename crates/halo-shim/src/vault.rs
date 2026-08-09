//! Shared byte-oriented encryption primitive: Argon2id key derivation +
//! XChaCha20-Poly1305 AEAD, serialized as a small versioned JSON envelope.
//!
//! Originally lived inline in `keys.rs` (string-only, for provider secrets in
//! the encrypted-file credential fallback). Extracted here, generalized to
//! `&[u8]`, so the same battle-tested primitive can also seal cache content
//! (`cache.rs`, `semantic_cache.rs`) at rest -- one implementation, one set of
//! tests, used by every "encrypt this at rest" call site in Halo.

use anyhow::{anyhow, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Envelope for one sealed blob. All fields base64-standard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncBlob {
    v: u8,
    salt: String,
    nonce: String,
    ct: String,
}

/// Encrypt `plaintext` under `passphrase`, returning the blob serialized to
/// JSON bytes (ready to store as-is: on disk, in a redb value, wherever).
pub fn seal(passphrase: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    let blob = seal_blob(passphrase, plaintext)?;
    Ok(serde_json::to_vec(&blob)?)
}

/// Inverse of [`seal`]: `sealed` must be the exact bytes `seal` returned.
pub fn open(passphrase: &str, sealed: &[u8]) -> Result<Vec<u8>> {
    let blob: EncBlob = serde_json::from_slice(sealed).map_err(|e| anyhow!("bad envelope: {e}"))?;
    open_blob(passphrase, &blob)
}

pub(crate) fn seal_blob(passphrase: &str, plaintext: &[u8]) -> Result<EncBlob> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};

    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    getrandom::getrandom(&mut salt).map_err(|e| anyhow!("rng: {e}"))?;
    getrandom::getrandom(&mut nonce).map_err(|e| anyhow!("rng: {e}"))?;

    let key = derive_key(passphrase, &salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|e| anyhow!("cipher: {e}"))?;
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .map_err(|e| anyhow!("encrypt: {e}"))?;

    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(EncBlob {
        v: 1,
        salt: b64.encode(salt),
        nonce: b64.encode(nonce),
        ct: b64.encode(ct),
    })
}

pub(crate) fn open_blob(passphrase: &str, blob: &EncBlob) -> Result<Vec<u8>> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};

    let b64 = base64::engine::general_purpose::STANDARD;
    let salt = b64.decode(&blob.salt).map_err(|e| anyhow!("b64 salt: {e}"))?;
    let nonce = b64.decode(&blob.nonce).map_err(|e| anyhow!("b64 nonce: {e}"))?;
    let ct = b64.decode(&blob.ct).map_err(|e| anyhow!("b64 ct: {e}"))?;
    if nonce.len() != 24 {
        return Err(anyhow!("bad nonce length"));
    }

    let key = derive_key(passphrase, &salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|e| anyhow!("cipher: {e}"))?;
    cipher
        .decrypt(XNonce::from_slice(&nonce), ct.as_ref())
        .map_err(|_| anyhow!("decrypt failed (wrong passphrase or tampered data)"))
}

fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; 32]> {
    use argon2::Argon2;
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("argon2: {e}"))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let sealed = seal("correct horse battery staple", b"hello world").unwrap();
        let out = open("correct horse battery staple", &sealed).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn wrong_passphrase_fails() {
        let sealed = seal("right", b"secret bytes").unwrap();
        assert!(open("wrong", &sealed).is_err());
    }

    #[test]
    fn empty_plaintext_roundtrips() {
        let sealed = seal("pass", b"").unwrap();
        let out = open("pass", &sealed).unwrap();
        assert_eq!(out, b"");
    }

    #[test]
    fn tampered_ciphertext_fails_to_open() {
        let sealed = seal("pass", b"important data").unwrap();
        let mut blob: EncBlob = serde_json::from_slice(&sealed).unwrap();
        blob.ct = base64::engine::general_purpose::STANDARD.encode(b"not the real ciphertext at all");
        let tampered = serde_json::to_vec(&blob).unwrap();
        assert!(open("pass", &tampered).is_err());
    }
}
