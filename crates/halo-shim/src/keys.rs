//! Virtual keys, device identity, and real-credential storage.
//!
//! The agent runtime is configured with a Halo *virtual* key
//! (`sf_live_<agent>_<random>`). The shim maps it to the real provider
//! credential, which is held only in the OS keychain -- or, on a headless box
//! with no keychain, in an Argon2id + XChaCha20-Poly1305 encrypted file whose
//! passphrase comes from `$HALO_VAULT_PASSPHRASE`. Real secrets never land in
//! any plaintext file, never in telemetry, never in the audit log.

use crate::config::Paths;
use crate::util::atomic_write_0600;
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use halo_common::telemetry::Provider;
use halo_common::vkey::{VirtualKeyRecord, VKEY_PREFIX};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Keychain service namespace for Halo credentials.
const KEYCHAIN_SERVICE: &str = "smartflow-halo";

#[derive(Debug, Serialize, Deserialize, Default)]
struct DeviceState {
    device_id: String,
}

/// Handles virtual-key records, device identity, and secret storage.
pub struct KeyStore {
    paths: Paths,
}

impl KeyStore {
    pub fn new(paths: Paths) -> Self {
        Self { paths }
    }

    // ----- device identity -------------------------------------------------

    /// Stable per-install device id, created on first call.
    pub fn device_id(&self) -> Result<String> {
        let path = self.paths.state();
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(s) = serde_json::from_str::<DeviceState>(&raw) {
                if !s.device_id.is_empty() {
                    return Ok(s.device_id);
                }
            }
        }
        let id = format!("dev_{}", uuid::Uuid::new_v4().simple());
        let state = DeviceState {
            device_id: id.clone(),
        };
        atomic_write_0600(&path, serde_json::to_vec_pretty(&state)?.as_slice())
            .context("writing device state")?;
        Ok(id)
    }

    // ----- virtual key records ---------------------------------------------

    pub fn records(&self) -> Result<Vec<VirtualKeyRecord>> {
        let path = self.paths.vkeys();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    fn save_records(&self, recs: &[VirtualKeyRecord]) -> Result<()> {
        atomic_write_0600(&self.paths.vkeys(), serde_json::to_vec_pretty(recs)?.as_slice())?;
        Ok(())
    }

    /// Register an agent: mint a virtual key, store the real provider secret,
    /// and persist the record. Returns the minted virtual key.
    pub fn issue(
        &self,
        agent_id: &str,
        provider: Provider,
        real_secret: &str,
        base_url: Option<String>,
    ) -> Result<String> {
        if agent_id.is_empty() || agent_id.contains('_') {
            return Err(anyhow!(
                "agent id must be non-empty and contain no underscores (got {agent_id:?})"
            ));
        }
        let mut recs = self.records()?;
        if recs.iter().any(|r| r.agent_id == agent_id && r.is_active()) {
            return Err(anyhow!("agent '{agent_id}' already registered; revoke it first"));
        }
        let random = uuid::Uuid::new_v4().simple().to_string();
        let vkey = format!("{VKEY_PREFIX}{agent_id}_{random}");

        self.store_secret(agent_id, real_secret)
            .context("storing provider secret")?;

        recs.push(VirtualKeyRecord {
            agent_id: agent_id.to_string(),
            virtual_key: vkey.clone(),
            provider,
            created_at: chrono::Utc::now(),
            revoked_at: None,
            base_url,
        });
        self.save_records(&recs)?;
        Ok(vkey)
    }

    pub fn revoke(&self, agent_id: &str) -> Result<()> {
        let mut recs = self.records()?;
        let mut found = false;
        for r in recs.iter_mut() {
            if r.agent_id == agent_id && r.is_active() {
                r.revoked_at = Some(chrono::Utc::now());
                found = true;
            }
        }
        if !found {
            return Err(anyhow!("no active agent '{agent_id}'"));
        }
        self.save_records(&recs)?;
        let _ = self.delete_secret(agent_id);
        Ok(())
    }

    /// Resolve a virtual key to its active record (used at ingress).
    pub fn resolve(&self, vkey: &str) -> Result<Option<VirtualKeyRecord>> {
        Ok(self
            .records()?
            .into_iter()
            .find(|r| r.virtual_key == vkey && r.is_active()))
    }

    // ----- secret storage: keychain first, encrypted file fallback ---------

    pub fn store_secret(&self, agent_id: &str, secret: &str) -> Result<()> {
        match keychain_entry(agent_id).and_then(|e| e.set_password(secret).map_err(Into::into)) {
            Ok(()) => Ok(()),
            Err(_) => self.fallback_store(agent_id, secret),
        }
    }

    pub fn get_secret(&self, agent_id: &str) -> Result<String> {
        if let Ok(entry) = keychain_entry(agent_id) {
            if let Ok(pw) = entry.get_password() {
                return Ok(pw);
            }
        }
        self.fallback_get(agent_id)
    }

    pub fn delete_secret(&self, agent_id: &str) -> Result<()> {
        if let Ok(entry) = keychain_entry(agent_id) {
            let _ = entry.delete_credential();
        }
        self.fallback_delete(agent_id)
    }

    // ----- encrypted-file fallback -----------------------------------------

    fn fallback_store(&self, agent_id: &str, secret: &str) -> Result<()> {
        let pass = vault_passphrase().context(
            "no OS keychain available and $HALO_VAULT_PASSPHRASE is unset; \
             cannot store the provider secret securely",
        )?;
        let mut file = self.read_fallback();
        file.secrets
            .insert(agent_id.to_string(), encrypt_secret(&pass, secret)?);
        atomic_write_0600(
            &self.paths.cred_fallback(),
            serde_json::to_vec_pretty(&file)?.as_slice(),
        )?;
        Ok(())
    }

    fn fallback_get(&self, agent_id: &str) -> Result<String> {
        let file = self.read_fallback();
        let blob = file
            .secrets
            .get(agent_id)
            .ok_or_else(|| anyhow!("no stored secret for agent '{agent_id}'"))?;
        let pass = vault_passphrase()
            .context("$HALO_VAULT_PASSPHRASE is required to decrypt the credential fallback")?;
        decrypt_secret(&pass, blob)
    }

    fn fallback_delete(&self, agent_id: &str) -> Result<()> {
        let mut file = self.read_fallback();
        if file.secrets.remove(agent_id).is_some() {
            atomic_write_0600(
                &self.paths.cred_fallback(),
                serde_json::to_vec_pretty(&file)?.as_slice(),
            )?;
        }
        Ok(())
    }

    fn read_fallback(&self) -> FallbackFile {
        std::fs::read_to_string(self.paths.cred_fallback())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}

fn keychain_entry(agent_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, agent_id).map_err(|e| anyhow!("keychain: {e}"))
}

fn vault_passphrase() -> Option<String> {
    std::env::var("HALO_VAULT_PASSPHRASE")
        .ok()
        .filter(|s| !s.is_empty())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FallbackFile {
    #[serde(default)]
    secrets: BTreeMap<String, EncBlob>,
}

/// Envelope for one encrypted secret. All fields base64-standard.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncBlob {
    v: u8,
    salt: String,
    nonce: String,
    ct: String,
}

fn encrypt_secret(passphrase: &str, secret: &str) -> Result<EncBlob> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};

    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    getrandom::getrandom(&mut salt).map_err(|e| anyhow!("rng: {e}"))?;
    getrandom::getrandom(&mut nonce).map_err(|e| anyhow!("rng: {e}"))?;

    let key = derive_key(passphrase, &salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|e| anyhow!("cipher: {e}"))?;
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), secret.as_bytes())
        .map_err(|e| anyhow!("encrypt: {e}"))?;

    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(EncBlob {
        v: 1,
        salt: b64.encode(salt),
        nonce: b64.encode(nonce),
        ct: b64.encode(ct),
    })
}

fn decrypt_secret(passphrase: &str, blob: &EncBlob) -> Result<String> {
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
    let pt = cipher
        .decrypt(XNonce::from_slice(&nonce), ct.as_ref())
        .map_err(|_| anyhow!("decrypt failed (wrong passphrase or tampered file)"))?;
    String::from_utf8(pt).map_err(|e| anyhow!("utf8: {e}"))
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
    fn encrypt_roundtrip() {
        let blob = encrypt_secret("correct horse battery staple", "sk-ant-secret").unwrap();
        let out = decrypt_secret("correct horse battery staple", &blob).unwrap();
        assert_eq!(out, "sk-ant-secret");
    }

    #[test]
    fn wrong_passphrase_fails() {
        let blob = encrypt_secret("right", "sk-secret").unwrap();
        assert!(decrypt_secret("wrong", &blob).is_err());
    }
}
