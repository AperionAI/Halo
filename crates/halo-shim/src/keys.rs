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
use crate::vault::{self, EncBlob};
use anyhow::{anyhow, Context, Result};
use halo_common::telemetry::Provider;
use halo_common::vkey::{VirtualKeyRecord, VKEY_PREFIX};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Keychain service namespace for Halo credentials.
const KEYCHAIN_SERVICE: &str = "smartflow-halo";

/// Which storage backend actually persisted a secret. Reported back so the
/// CLI can tell the operator the truth about where their key landed instead
/// of always claiming the OS keychain (the encrypted-file vault is used on
/// headless boxes with no keychain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretBackend {
    /// The OS keychain (macOS Keychain / Linux keyutils / Windows Cred Mgr).
    Keychain,
    /// The Argon2id + XChaCha20-Poly1305 encrypted file (`cred-fallback.json`).
    EncryptedFile,
}

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
    /// and persist the record. Returns the minted virtual key and which
    /// backend actually stored the secret (so the CLI can report it truthfully).
    pub fn issue(
        &self,
        agent_id: &str,
        provider: Provider,
        real_secret: &str,
        base_url: Option<String>,
    ) -> Result<(String, SecretBackend)> {
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

        let backend = self
            .store_secret(agent_id, real_secret)
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
        Ok((vkey, backend))
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

    pub fn store_secret(&self, agent_id: &str, secret: &str) -> Result<SecretBackend> {
        match keychain_entry(agent_id).and_then(|e| e.set_password(secret).map_err(Into::into)) {
            Ok(()) => Ok(SecretBackend::Keychain),
            Err(keychain_err) => self.fallback_store(agent_id, secret, &keychain_err),
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

    fn fallback_store(
        &self,
        agent_id: &str,
        secret: &str,
        keychain_err: &anyhow::Error,
    ) -> Result<SecretBackend> {
        let pass = vault_passphrase().ok_or_else(|| headless_store_error(keychain_err))?;
        let mut file = self.read_fallback();
        file.secrets
            .insert(agent_id.to_string(), encrypt_secret(&pass, secret)?);
        atomic_write_0600(
            &self.paths.cred_fallback(),
            serde_json::to_vec_pretty(&file)?.as_slice(),
        )?;
        Ok(SecretBackend::EncryptedFile)
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

/// Build the error shown when the OS keychain is unreachable AND
/// `$HALO_VAULT_PASSPHRASE` is unset, so neither storage backend can hold the
/// secret. When the keychain failure looks like the macOS "no GUI session"
/// case, say so explicitly -- otherwise the operator is left to infer why a
/// documented feature failed on a headless box.
fn headless_store_error(keychain_err: &anyhow::Error) -> anyhow::Error {
    let detail = keychain_err.to_string();
    if looks_like_no_gui_session(&detail) {
        anyhow!(
            "the OS keychain is unreachable because there is no interactive GUI login \
             session -- this is expected on a headless box, or when reached via `sudo` \
             over SSH ({detail}).\n\n\
             Set $HALO_VAULT_PASSPHRASE to use the encrypted-file vault instead. See \
             docs/HEADLESS.md for how to generate one, where to persist it, and note \
             that it is required again on every `halo serve` start (not just now)."
        )
    } else {
        anyhow!(
            "no OS keychain available ({detail}) and $HALO_VAULT_PASSPHRASE is unset; \
             cannot store the provider secret securely. Set $HALO_VAULT_PASSPHRASE to \
             use the encrypted-file vault -- see docs/HEADLESS.md."
        )
    }
}

/// Heuristic for the macOS Keychain "User interaction is not allowed"
/// error (errSecInteractionNotAllowed / -25308), which is what surfaces when
/// the keychain is touched outside a GUI login session (headless / SSH+sudo).
fn looks_like_no_gui_session(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("interaction is not allowed")
        || e.contains("interaction not allowed")
        || e.contains("25308")
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

/// Thin `&str` wrapper over `vault::seal_blob` -- kept so the on-disk
/// `EncBlob` shape (and this module's existing tests) are unaffected by the
/// `vault.rs` extraction.
fn encrypt_secret(passphrase: &str, secret: &str) -> Result<EncBlob> {
    vault::seal_blob(passphrase, secret.as_bytes())
}

fn decrypt_secret(passphrase: &str, blob: &EncBlob) -> Result<String> {
    let pt = vault::open_blob(passphrase, blob)?;
    String::from_utf8(pt).map_err(|e| anyhow!("utf8: {e}"))
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

    #[test]
    fn detects_no_gui_session_from_macos_error_text() {
        assert!(looks_like_no_gui_session(
            "keychain: User interaction is not allowed."
        ));
        assert!(looks_like_no_gui_session("SecKeychain error -25308"));
        // Unrelated failures must not be mistaken for the headless case.
        assert!(!looks_like_no_gui_session("keychain: item not found"));
    }

    #[test]
    fn headless_error_names_the_vault_env_and_doc() {
        let err = headless_store_error(&anyhow!("keychain: User interaction is not allowed."));
        let msg = err.to_string();
        assert!(msg.contains("HALO_VAULT_PASSPHRASE"));
        assert!(msg.contains("docs/HEADLESS.md"));
        assert!(msg.contains("GUI login session"));
    }
}
