//! Shared types for Smartflow Halo.
//!
//! This crate is the single source of truth for the wire contract between the
//! local shim (which produces telemetry) and the relay (which ingests it).
//! Keeping it in one place is a deliberate lesson-learned: the main Smartflow
//! codebase let the cache schema drift across modules, so here the schema, the
//! provider model, and the virtual-key format live in exactly one crate that
//! both binaries depend on.

pub mod effort;
pub mod license;
pub mod pricing;
pub mod telemetry;
pub mod vkey;

pub use effort::{
    append_routing_reason, body_has_tool_error, body_wants_stream, cheap_model_for_provider, decide,
    infer_intent, infer_stage, may_switch_provider, resolve_efficient_hop, rewrite_json_model,
    rewrite_json_model_str, score, should_escalate_quality, EffortDecision, EffortSignals,
    EffortTier, LcrMode, LcrSettings,
};
pub use license::{feature, Entitlements, Ladder, LicenseClaims, LicenseStatus, Tier};
pub use pricing::{estimate_cost_usd, ModelPrice, PriceTable};
pub use telemetry::{PolicyDecision, Provider, TelemetryBatch, TelemetryEvent};
pub use vkey::{parse_virtual_key, VirtualKeyRecord};
