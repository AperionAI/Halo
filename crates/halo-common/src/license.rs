//! Offline-verifiable license entitlements.
//!
//! Reuses the exact signed-envelope primitive Compass uses for attestation
//! bundles (`tools/compass-standalone/src/attest.rs`): a canonical-JSON payload
//! signed with Ed25519, verified offline against Aperion's published public
//! key. No network call, no phone-home -- consistent with Halo's offline-first
//! trust model.
//!
//! The cardinal rule (enforced by [`Entitlements::from_license_key`]): a
//! missing, malformed, wrong-key, or expired license degrades to the **free
//! tier**. It NEVER prevents the proxy from starting or serving. Nothing that
//! keeps a solo self-hoster safe from a runaway bill is ever gated (caps,
//! kill switch, denylist). Cache, compression, and prompt-cache injection
//! are Cut -- that's the bill cut. Free still meters them as a starred
//! "would have saved" figure.
//!
//! Wire shape (base64url of a compact JSON envelope, so a license key is a
//! single paste-friendly token):
//! ```text
//! base64url({ "payload": <claims>, "alg": "EdDSA", "keyid": "<hex8>", "signature_b64": "<std-b64>" })
//! ```

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Free-tier ceiling on the semantic cache's stored-entry count. Semantic
/// cache *serve* is Cut (with exact cache and compression). This ceiling
/// still applies to a paid key that doesn't include
/// [`feature::SEMANTIC_CACHE_UNLIMITED`].
pub const FREE_SEMANTIC_CACHE_MAX_ENTRIES: u64 = 200;

/// Free-tier ceiling on the number of *active* registered agents (`halo agent
/// add`) on a single install. Deliberately a scale/convenience cap, not a
/// safety one: budgets, the kill switch, and the denylist are never
/// capped, and 3 is enough to fully evaluate Halo (e.g. one agent per
/// provider plus a fast/cheap one). This is the main non-fleet reason to
/// license Halo -- a solo power user who outgrows 3 agents on one machine
/// hits it long before ever needing remote-kill or multi-seat.
pub const FREE_AGENT_LIMIT: u32 = 3;

/// Free-tier local history window (7 days). Cut is 30; Route/Govern is 90.
pub const FREE_HISTORY_HOURS: u64 = 7 * 24;
pub const CUT_HISTORY_HOURS: u64 = 30 * 24;
pub const ROUTE_HISTORY_HOURS: u64 = 90 * 24;

/// Aperion's production license-signing public key (Ed25519, base64url, no pad).
/// The matching private key is held by Aperion offline and never ships in any
/// binary. Override at verify time (e.g. staging keys) with the
/// `HALO_LICENSE_PUBKEY` env var (same base64url encoding).
pub const APERION_LICENSE_PUBKEY_B64URL: &str = "lQbv4rTnKo-hn1b1sv6nlM04QF2dh4jwacpSW59SwkY";

/// Paid feature flags. Kept as string constants (not an enum) on purpose: a
/// newer license that names a feature an older binary doesn't recognize is
/// simply ignored, never a hard parse error. Forward-compatible by design.
pub mod feature {
    /// Budget soft/hard-cap crossings POST to a configured webhook.
    pub const ALERTING: &str = "alerting";
    /// Shim pulls a best-effort "revoked agents" list from the relay.
    pub const REMOTE_KILL: &str = "remote_kill";
    /// Raises the free-tier ceiling on semantic-cache `max_entries`.
    pub const SEMANTIC_CACHE_UNLIMITED: &str = "semantic_cache_unlimited";
    /// Per-channel / sub-agent cost attribution drill-down.
    pub const SUBJECT_ATTRIBUTION: &str = "subject_attribution";
    /// Multi-seat / multi-token relay dashboard.
    pub const MULTI_SEAT: &str = "multi_seat";
    /// Raises (removes) the free-tier ceiling on registered agent count
    /// (`halo agent add`). The one non-fleet paid feature -- useful to a
    /// solo self-hoster scaling up on a single machine, not just a team.
    pub const MULTI_AGENT_UNLIMITED: &str = "multi_agent_unlimited";

    /// Paid Cut tier ($50): cache, compression, prompt-cache injection.
    /// That's the bill cut. Also 30-day history and more than 3 agents.
    pub const CUT: &str = "cut";
    /// Paid Route tier ($100): failover, task-class routing, and effort routing on top of Cut.
    pub const ROUTE: &str = "route";
    /// Paid Govern tier ($250). Gated on the relay; the flag exists so a
    /// minted key can name it without an older binary failing to parse.
    pub const GOVERN: &str = "govern";

    /// All known features (for `halo license show` and issuer validation help).
    pub const ALL: &[&str] = &[
        CUT,
        ROUTE,
        GOVERN,
        ALERTING,
        REMOTE_KILL,
        SEMANTIC_CACHE_UNLIMITED,
        SUBJECT_ATTRIBUTION,
        MULTI_SEAT,
        MULTI_AGENT_UNLIMITED,
    ];
}

/// The signed claims payload. This is what the issuer (Aperion) signs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LicenseClaims {
    /// Customer/org this license was issued to (display + support).
    pub org: String,
    /// Human tier label ("pro", "team", ...). Display only -- gating is always
    /// by individual `features`, never by parsing this string.
    pub tier: String,
    /// Seats entitled. Informational for the shim; enforced by the hosted relay
    /// (Path B). 0 = unspecified.
    #[serde(default)]
    pub seats: u32,
    /// Feature flags this license unlocks (see [`feature`]). Unknown strings
    /// are preserved but have no effect.
    #[serde(default)]
    pub features: Vec<String>,
    /// RFC3339 issue time.
    pub issued_at: String,
    /// RFC3339 expiry. A license past this instant resolves to free tier.
    pub expires_at: String,
}

/// Coarse tier, derived from whether a valid, unexpired license is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Free,
    Paid,
}

/// Product ladder (Free / Cut / Route / Govern). Coarse [`Tier`] stays
/// Free vs Paid so existing `has()` gates don't break; this is the v1.0 name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ladder {
    Free,
    Cut,
    Route,
    Govern,
}

impl Ladder {
    pub fn as_str(self) -> &'static str {
        match self {
            Ladder::Free => "free",
            Ladder::Cut => "cut",
            Ladder::Route => "route",
            Ladder::Govern => "govern",
        }
    }
}

/// Why the resolved entitlements are what they are -- surfaced verbatim by
/// `halo license show` so a customer can self-diagnose a bad key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseStatus {
    /// No `license_key` configured at all.
    None,
    /// Verified signature, not expired.
    Active,
    /// Signature verified but the license is past `expires_at`.
    Expired,
    /// Could not decode / wrong key / bad signature. Carries a human reason.
    Invalid(String),
}

impl LicenseStatus {
    pub fn label(&self) -> String {
        match self {
            LicenseStatus::None => "no license (free tier)".to_string(),
            LicenseStatus::Active => "active".to_string(),
            LicenseStatus::Expired => "expired (free tier)".to_string(),
            LicenseStatus::Invalid(r) => format!("invalid (free tier): {r}"),
        }
    }
}

/// The resolved, runtime view the rest of the shim gates on. Constructed once
/// at startup and shared read-only. `has()` is the single gating entry point.
#[derive(Debug, Clone)]
pub struct Entitlements {
    pub tier: Tier,
    pub tier_label: String,
    pub org: Option<String>,
    pub seats: u32,
    pub features: Vec<String>,
    pub expires_at: Option<String>,
    pub status: LicenseStatus,
}

impl Default for Entitlements {
    fn default() -> Self {
        Self::free(LicenseStatus::None)
    }
}

impl Entitlements {
    /// Free tier with a given explanatory status.
    pub fn free(status: LicenseStatus) -> Self {
        Self {
            tier: Tier::Free,
            tier_label: "free".to_string(),
            org: None,
            seats: 0,
            features: Vec::new(),
            expires_at: None,
            status,
        }
    }

    /// True iff a valid, unexpired paid license unlocks `feature`. Free tier is
    /// always `false`. This is the ONLY method feature-gating should call.
    pub fn has(&self, feature: &str) -> bool {
        matches!(self.tier, Tier::Paid) && self.features.iter().any(|f| f == feature)
    }

    /// v1.0 ladder. Any active paid license that doesn't name Route/Govern
    /// is Cut, including legacy `pro`/`team` keys.
    pub fn ladder(&self) -> Ladder {
        if !matches!(self.tier, Tier::Paid) {
            return Ladder::Free;
        }
        let label = self.tier_label.to_ascii_lowercase();
        if self.has(feature::GOVERN) || label == "govern" {
            return Ladder::Govern;
        }
        if self.has(feature::ROUTE) || label == "route" {
            return Ladder::Route;
        }
        Ladder::Cut
    }

    pub fn max_history_hours(&self) -> u64 {
        match self.ladder() {
            Ladder::Free => FREE_HISTORY_HOURS,
            Ladder::Cut => CUT_HISTORY_HOURS,
            Ladder::Route | Ladder::Govern => ROUTE_HISTORY_HOURS,
        }
    }

    /// Cap a requested report/dashboard window. `None` or `0` (the old
    /// "all time" choice) becomes the tier max so Free is actually 7 days.
    pub fn clamp_history_hours(&self, requested: Option<i64>) -> i64 {
        let cap = self.max_history_hours() as i64;
        match requested {
            None | Some(0) => cap,
            Some(h) if h < 0 => cap,
            Some(h) => h.min(cap),
        }
    }

    /// Default feature list when minting `--tier cut|route|govern` with no
    /// explicit `--feature` flags.
    pub fn default_features_for_tier(tier: &str) -> Vec<String> {
        match tier.trim().to_ascii_lowercase().as_str() {
            "cut" => vec![
                feature::CUT.to_string(),
                feature::ALERTING.to_string(),
                feature::SEMANTIC_CACHE_UNLIMITED.to_string(),
                feature::MULTI_AGENT_UNLIMITED.to_string(),
            ],
            "route" => vec![
                feature::CUT.to_string(),
                feature::ROUTE.to_string(),
                feature::ALERTING.to_string(),
                feature::SEMANTIC_CACHE_UNLIMITED.to_string(),
                feature::MULTI_AGENT_UNLIMITED.to_string(),
            ],
            "govern" => vec![
                feature::CUT.to_string(),
                feature::ROUTE.to_string(),
                feature::GOVERN.to_string(),
                feature::ALERTING.to_string(),
                feature::REMOTE_KILL.to_string(),
                feature::SEMANTIC_CACHE_UNLIMITED.to_string(),
                feature::SUBJECT_ATTRIBUTION.to_string(),
                feature::MULTI_SEAT.to_string(),
                feature::MULTI_AGENT_UNLIMITED.to_string(),
            ],
            _ => Vec::new(),
        }
    }

    /// Resolve entitlements from a configured license key using the embedded
    /// (or `HALO_LICENSE_PUBKEY`-overridden) Aperion public key. Infallible:
    /// any problem degrades to the free tier with a descriptive status.
    pub fn from_license_key(license_key: Option<&str>) -> Self {
        let key = match license_key.map(str::trim).filter(|s| !s.is_empty()) {
            Some(k) => k,
            None => return Self::free(LicenseStatus::None),
        };
        let vk = match embedded_pubkey() {
            Some(vk) => vk,
            None => {
                return Self::free(LicenseStatus::Invalid(
                    "embedded license public key is unreadable".to_string(),
                ))
            }
        };
        Self::verify_with_key(key, &vk, chrono::Utc::now())
    }

    /// Testable core: verify against an explicit key at an explicit `now`.
    pub fn verify_with_key(
        license_key: &str,
        vk: &VerifyingKey,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let claims = match verify_claims(license_key, vk) {
            Ok(c) => c,
            Err(e) => return Self::free(LicenseStatus::Invalid(e)),
        };

        let expires = match chrono::DateTime::parse_from_rfc3339(&claims.expires_at) {
            Ok(t) => t.with_timezone(&chrono::Utc),
            Err(_) => {
                return Self::free(LicenseStatus::Invalid(format!(
                    "unparseable expires_at `{}`",
                    claims.expires_at
                )))
            }
        };

        if now > expires {
            // Still surface org/features so `show` is useful, but tier is Free.
            return Self {
                tier: Tier::Free,
                tier_label: "free".to_string(),
                org: Some(claims.org),
                seats: claims.seats,
                features: claims.features,
                expires_at: Some(claims.expires_at),
                status: LicenseStatus::Expired,
            };
        }

        Self {
            tier: Tier::Paid,
            tier_label: claims.tier,
            org: Some(claims.org),
            seats: claims.seats,
            features: claims.features,
            expires_at: Some(claims.expires_at),
            status: LicenseStatus::Active,
        }
    }
}

/// Issuer convenience: sign `claims` from a raw 32-byte Ed25519 seed. Lets the
/// CLI mint a license without depending on `ed25519-dalek` directly.
pub fn issue_from_seed(claims: &LicenseClaims, seed: &[u8; 32]) -> String {
    issue(claims, &SigningKey::from_bytes(seed))
}

/// Companion to [`issue_from_seed`]: the base64url public key matching a
/// seed, so a caller (tests, or `HALO_LICENSE_PUBKEY` for a staging key) can
/// verify a seed-issued license without depending on `ed25519-dalek` either.
pub fn pubkey_b64url_from_seed(seed: &[u8; 32]) -> String {
    let vk = SigningKey::from_bytes(seed).verifying_key();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(vk.to_bytes())
}

/// Issuer side (Aperion): sign `claims` into a paste-friendly license key. The
/// signing key never ships in a released binary -- this is used by an offline
/// minting tool (`halo license issue --signing-key ...`).
pub fn issue(claims: &LicenseClaims, signing_key: &SigningKey) -> String {
    let payload = serde_json::to_value(claims).unwrap_or(Value::Null);
    let canonical = canonical_json(&payload);
    let sig = signing_key.sign(&canonical);
    let vk = signing_key.verifying_key();
    let envelope = json!({
        "payload": payload,
        "alg": "EdDSA",
        "keyid": keyid_for(&vk),
        "signature_b64": base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
    });
    let compact = serde_json::to_vec(&envelope).unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(compact)
}

/// Verify a license key against `vk`, returning the claims on success.
fn verify_claims(license_key: &str, vk: &VerifyingKey) -> Result<LicenseClaims, String> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(license_key.trim())
        .map_err(|_| "license key is not valid base64url".to_string())?;
    let envelope: Value =
        serde_json::from_slice(&raw).map_err(|_| "license envelope is not valid JSON".to_string())?;

    let payload = envelope
        .get("payload")
        .ok_or_else(|| "license envelope missing `payload`".to_string())?;
    let sig_b64 = envelope
        .get("signature_b64")
        .and_then(|s| s.as_str())
        .ok_or_else(|| "license envelope missing `signature_b64`".to_string())?;

    let canonical = canonical_json(payload);
    if !verify_sig(vk, &canonical, sig_b64) {
        return Err("signature does not verify against Aperion's key".to_string());
    }

    serde_json::from_value::<LicenseClaims>(payload.clone())
        .map_err(|e| format!("license claims malformed: {e}"))
}

// ── crypto helpers (mirrors compass-standalone/src/attest.rs) ───────────────

fn embedded_pubkey() -> Option<VerifyingKey> {
    let spec = std::env::var("HALO_LICENSE_PUBKEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| APERION_LICENSE_PUBKEY_B64URL.to_string());
    pubkey_from_b64url(&spec)
}

pub fn pubkey_from_b64url(s: &str) -> Option<VerifyingKey> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim())
        .ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    VerifyingKey::from_bytes(&arr).ok()
}

fn verify_sig(vk: &VerifyingKey, payload: &[u8], sig_b64: &str) -> bool {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(sig_b64.trim()) {
        Ok(b) if b.len() == 64 => b,
        _ => return false,
    };
    let mut arr = [0u8; 64];
    arr.copy_from_slice(&bytes);
    vk.verify(payload, &Signature::from_bytes(&arr)).is_ok()
}

fn keyid_for(vk: &VerifyingKey) -> String {
    let h = Sha256::digest(vk.to_bytes());
    hex::encode(&h[..8])
}

/// Canonical JSON bytes: recursively sort object keys, serialise compactly.
/// Identical to Compass's canonicaliser so the two share a trust model.
fn canonical_json(v: &Value) -> Vec<u8> {
    serde_json::to_vec(&sort_value(v)).unwrap_or_default()
}

fn sort_value(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let bt: std::collections::BTreeMap<String, Value> = m
                .iter()
                .map(|(k, val)| (k.clone(), sort_value(val)))
                .collect();
            Value::Object(bt.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(sort_value).collect()),
        _ => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair() -> SigningKey {
        // Deterministic seed for reproducible tests.
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn claims(expires_at: &str) -> LicenseClaims {
        LicenseClaims {
            org: "Acme Corp".to_string(),
            tier: "team".to_string(),
            seats: 10,
            features: vec![
                feature::ALERTING.to_string(),
                feature::REMOTE_KILL.to_string(),
            ],
            issued_at: "2026-01-01T00:00:00Z".to_string(),
            expires_at: expires_at.to_string(),
        }
    }

    #[test]
    fn issue_then_verify_round_trip_unlocks_features() {
        let sk = keypair();
        let key = issue(&claims("2099-01-01T00:00:00Z"), &sk);
        let now = chrono::Utc::now();
        let ent = Entitlements::verify_with_key(&key, &sk.verifying_key(), now);

        assert_eq!(ent.tier, Tier::Paid);
        assert_eq!(ent.tier_label, "team");
        assert_eq!(ent.org.as_deref(), Some("Acme Corp"));
        assert_eq!(ent.status, LicenseStatus::Active);
        assert!(ent.has(feature::ALERTING));
        assert!(ent.has(feature::REMOTE_KILL));
        assert!(!ent.has(feature::MULTI_SEAT));
    }

    #[test]
    fn expired_license_degrades_to_free_but_keeps_display_fields() {
        let sk = keypair();
        let key = issue(&claims("2020-01-01T00:00:00Z"), &sk);
        let ent = Entitlements::verify_with_key(&key, &sk.verifying_key(), chrono::Utc::now());

        assert_eq!(ent.tier, Tier::Free);
        assert_eq!(ent.status, LicenseStatus::Expired);
        assert!(!ent.has(feature::ALERTING));
        assert_eq!(ent.org.as_deref(), Some("Acme Corp"));
    }

    #[test]
    fn wrong_key_degrades_to_free_invalid() {
        let sk = keypair();
        let key = issue(&claims("2099-01-01T00:00:00Z"), &sk);

        let other = SigningKey::from_bytes(&[9u8; 32]);
        let ent = Entitlements::verify_with_key(&key, &other.verifying_key(), chrono::Utc::now());

        assert_eq!(ent.tier, Tier::Free);
        assert!(matches!(ent.status, LicenseStatus::Invalid(_)));
        assert!(!ent.has(feature::ALERTING));
    }

    #[test]
    fn tampered_claims_break_verification() {
        let sk = keypair();
        let key = issue(&claims("2099-01-01T00:00:00Z"), &sk);

        // Decode, bump the feature list, re-encode without re-signing.
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&key)
            .unwrap();
        let mut env: Value = serde_json::from_slice(&raw).unwrap();
        env["payload"]["features"] = json!(["alerting", "remote_kill", "multi_seat"]);
        let tampered = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&env).unwrap());

        let ent =
            Entitlements::verify_with_key(&tampered, &sk.verifying_key(), chrono::Utc::now());
        assert_eq!(ent.tier, Tier::Free);
        assert!(matches!(ent.status, LicenseStatus::Invalid(_)));
    }

    #[test]
    fn no_key_is_free_tier() {
        let ent = Entitlements::from_license_key(None);
        assert_eq!(ent.tier, Tier::Free);
        assert_eq!(ent.status, LicenseStatus::None);
    }

    #[test]
    fn garbage_key_is_free_invalid() {
        let ent = Entitlements::from_license_key(Some("not-a-real-key"));
        assert_eq!(ent.tier, Tier::Free);
        assert!(matches!(ent.status, LicenseStatus::Invalid(_)));
    }

    #[test]
    fn embedded_pubkey_is_valid() {
        assert!(
            pubkey_from_b64url(APERION_LICENSE_PUBKEY_B64URL).is_some(),
            "the embedded production key must always decode"
        );
    }

    #[test]
    fn free_history_is_seven_days_cut_is_thirty_route_is_ninety() {
        let free = Entitlements::free(LicenseStatus::None);
        assert_eq!(free.ladder(), Ladder::Free);
        assert_eq!(free.max_history_hours(), 7 * 24);
        assert_eq!(free.clamp_history_hours(None), 7 * 24);
        assert_eq!(free.clamp_history_hours(Some(0)), 7 * 24);
        assert_eq!(free.clamp_history_hours(Some(24)), 24);
        assert_eq!(free.clamp_history_hours(Some(9999)), 7 * 24);

        let sk = keypair();
        let mut cut_claims = claims("2099-01-01T00:00:00Z");
        cut_claims.tier = "cut".into();
        cut_claims.features = Entitlements::default_features_for_tier("cut");
        let cut = Entitlements::verify_with_key(
            &issue(&cut_claims, &sk),
            &sk.verifying_key(),
            chrono::Utc::now(),
        );
        assert_eq!(cut.ladder(), Ladder::Cut);
        assert_eq!(cut.max_history_hours(), 30 * 24);
        assert!(cut.has(feature::CUT));

        let mut route_claims = claims("2099-01-01T00:00:00Z");
        route_claims.tier = "route".into();
        route_claims.features = Entitlements::default_features_for_tier("route");
        let route = Entitlements::verify_with_key(
            &issue(&route_claims, &sk),
            &sk.verifying_key(),
            chrono::Utc::now(),
        );
        assert_eq!(route.ladder(), Ladder::Route);
        assert_eq!(route.max_history_hours(), 90 * 24);

        // Legacy paid "team" key with no ladder feature is still Cut.
        let legacy = Entitlements::verify_with_key(
            &issue(&claims("2099-01-01T00:00:00Z"), &sk),
            &sk.verifying_key(),
            chrono::Utc::now(),
        );
        assert_eq!(legacy.ladder(), Ladder::Cut);
        assert_eq!(legacy.max_history_hours(), 30 * 24);
    }
}
