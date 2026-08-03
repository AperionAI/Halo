//! Shared types for Smartflow Halo.
//!
//! This crate is the single source of truth for the wire contract between the
//! local shim (which produces telemetry) and the relay (which ingests it).
//! Keeping it in one place is a deliberate lesson-learned: the main Smartflow
//! codebase let the cache schema drift across modules, so here the schema, the
//! provider model, and the virtual-key format live in exactly one crate that
//! both binaries depend on.

pub mod pricing;
pub mod telemetry;
pub mod vkey;

pub use pricing::{estimate_cost_usd, ModelPrice, PriceTable};
pub use telemetry::{PolicyDecision, Provider, TelemetryBatch, TelemetryEvent};
pub use vkey::{parse_virtual_key, VirtualKeyRecord};
