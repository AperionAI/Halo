//! Point Nous Hermes Agent at Halo.
//!
//! Hermes treats `~/.hermes/config.yaml` as the source of truth for model /
//! endpoint (not process env). Secrets go in `~/.hermes/.env`. Patching only
//! `ANTHROPIC_BASE_URL` is how you get a false sense of safety — `hermes setup`
//! can clear stale env on the next run. This module writes both files.

use anyhow::{anyhow, Context, Result};
use halo_common::telemetry::Provider;
use serde_yaml::Value;
use std::path::{Path, PathBuf};

/// Merge Halo's listen URL + virtual key into Hermes `config.yaml`.
///
/// Sets a named `providers.halo` entry (config v12 shape) and points
/// `model.provider` / `model.base_url` at it. Leaves `model.default` alone
/// when already set.
pub fn patch_config_yaml(raw: &str, base_url: &str, vkey: &str, provider: Provider) -> Result<String> {
    let mut root: Value = if raw.trim().is_empty() {
        Value::Mapping(serde_yaml::Mapping::new())
    } else {
        serde_yaml::from_str(raw).context("parsing ~/.hermes/config.yaml")?
    };
    let map = root
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("hermes config.yaml root must be a mapping"))?;

    let transport = match provider {
        Provider::Anthropic => "anthropic_messages",
        _ => "chat_completions",
    };
    let api = match provider {
        Provider::Anthropic => base_url.to_string(),
        _ => {
            if base_url.ends_with("/v1") {
                base_url.to_string()
            } else {
                format!("{}/v1", base_url.trim_end_matches('/'))
            }
        }
    };

    let mut halo = serde_yaml::Mapping::new();
    halo.insert(Value::String("api".into()), Value::String(api.clone()));
    halo.insert(Value::String("name".into()), Value::String("Halo".into()));
    halo.insert(Value::String("api_key".into()), Value::String(vkey.into()));
    halo.insert(
        Value::String("transport".into()),
        Value::String(transport.into()),
    );

    let providers = map
        .entry(Value::String("providers".into()))
        .or_insert_with(|| Value::Mapping(serde_yaml::Mapping::new()));
    let providers_map = providers
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("hermes config.yaml providers must be a mapping"))?;
    providers_map.insert(Value::String("halo".into()), Value::Mapping(halo));

    let model = map
        .entry(Value::String("model".into()))
        .or_insert_with(|| Value::Mapping(serde_yaml::Mapping::new()));
    // An empty-string sentinel means "not configured yet" on a fresh Hermes
    // install. Replace it with a mapping rather than fighting the string.
    if model.as_str().is_some() {
        *model = Value::Mapping(serde_yaml::Mapping::new());
    }
    let model_map = model
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("hermes config.yaml model must be a mapping"))?;
    model_map.insert(
        Value::String("provider".into()),
        Value::String("halo".into()),
    );
    model_map.insert(Value::String("base_url".into()), Value::String(api));
    if !matches!(
        model_map.get(&Value::String("default".into())),
        Some(Value::String(s)) if !s.is_empty()
    ) {
        let fallback = match provider {
            Provider::Anthropic => "claude-sonnet-4-5",
            _ => "gpt-4o",
        };
        model_map.insert(
            Value::String("default".into()),
            Value::String(fallback.into()),
        );
    }

    Ok(serde_yaml::to_string(&root)?)
}

/// Upsert Halo's virtual key + base URL into Hermes `.env` without dropping
/// unrelated keys. Comments and blank lines are preserved.
pub fn patch_env(raw: &str, base_url: &str, vkey: &str, provider: Provider) -> String {
    let (key_name, url_name, url_val) = match provider {
        Provider::Anthropic => (
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            base_url.trim_end_matches('/').to_string(),
        ),
        _ => {
            let url = if base_url.ends_with("/v1") {
                base_url.to_string()
            } else {
                format!("{}/v1", base_url.trim_end_matches('/'))
            };
            ("OPENAI_API_KEY", "OPENAI_BASE_URL", url)
        }
    };
    upsert_env(raw, &[(key_name, vkey), (url_name, &url_val)])
}

fn upsert_env(raw: &str, pairs: &[(&str, &str)]) -> String {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out = String::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let key = trimmed.split('=').next().unwrap_or("").trim();
        if let Some((_, val)) = pairs.iter().find(|(k, _)| *k == key) {
            out.push_str(&format!("{key}={val}\n"));
            seen.insert(key.to_string());
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    for (k, v) in pairs {
        if !seen.contains(*k) {
            out.push_str(&format!("{k}={v}\n"));
        }
    }
    out
}

pub struct HermesPaths {
    pub config: PathBuf,
    pub env: PathBuf,
}

impl HermesPaths {
    pub fn resolve(home: PathBuf) -> Self {
        Self {
            config: home.join("config.yaml"),
            env: home.join(".env"),
        }
    }
}

pub struct ApplyResult {
    pub config: PathBuf,
    pub env: PathBuf,
    pub config_out: String,
    pub env_out: String,
    pub wrote: bool,
}

pub fn apply(
    paths: &HermesPaths,
    base_url: &str,
    vkey: &str,
    provider: Provider,
    dry_run: bool,
) -> Result<ApplyResult> {
    if !paths.config.exists() {
        anyhow::bail!(
            "no {} -- is Hermes installed for this user? (override with --home)",
            paths.config.display()
        );
    }
    let config_raw = std::fs::read_to_string(&paths.config)?;
    let config_out = patch_config_yaml(&config_raw, base_url, vkey, provider)?;
    let env_raw = if paths.env.exists() {
        std::fs::read_to_string(&paths.env)?
    } else {
        String::new()
    };
    let env_out = patch_env(&env_raw, base_url, vkey, provider);

    if !dry_run {
        backup_if_exists(&paths.config)?;
        crate::util::atomic_write_0600(&paths.config, config_out.as_bytes())?;
        if paths.env.exists() {
            backup_if_exists(&paths.env)?;
        }
        crate::util::atomic_write_0600(&paths.env, env_out.as_bytes())?;
    }

    Ok(ApplyResult {
        config: paths.config.clone(),
        env: paths.env.clone(),
        config_out,
        env_out,
        wrote: !dry_run,
    })
}

fn backup_if_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let bak = PathBuf::from(format!("{}.halo-bak", path.display()));
    std::fs::copy(path, &bak).with_context(|| format!("backing up {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_provider_and_model_point_at_halo_openai() {
        let out = patch_config_yaml(
            "model: \"\"\n",
            "http://127.0.0.1:8787",
            "sf_live_h_abc",
            Provider::Openai,
        )
        .unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(v["providers"]["halo"]["api"], "http://127.0.0.1:8787/v1");
        assert_eq!(v["providers"]["halo"]["api_key"], "sf_live_h_abc");
        assert_eq!(v["providers"]["halo"]["transport"], "chat_completions");
        assert_eq!(v["model"]["provider"], "halo");
        assert_eq!(v["model"]["base_url"], "http://127.0.0.1:8787/v1");
        assert_eq!(v["model"]["default"], "gpt-4o");
    }

    #[test]
    fn anthropic_uses_messages_transport_without_forced_v1() {
        let out = patch_config_yaml("{}", "http://127.0.0.1:8787", "sf_live_h_abc", Provider::Anthropic)
            .unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(v["providers"]["halo"]["api"], "http://127.0.0.1:8787");
        assert_eq!(v["providers"]["halo"]["transport"], "anthropic_messages");
        assert_eq!(v["model"]["base_url"], "http://127.0.0.1:8787");
    }

    #[test]
    fn keeps_existing_default_model() {
        let raw = "model:\n  default: claude-opus-4\n  provider: anthropic\n";
        let out = patch_config_yaml(raw, "http://127.0.0.1:8787", "k", Provider::Anthropic).unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(v["model"]["default"], "claude-opus-4");
        assert_eq!(v["model"]["provider"], "halo");
    }

    #[test]
    fn env_upserts_without_dropping_other_keys() {
        let raw = "FOO=bar\nANTHROPIC_API_KEY=old\n";
        let out = patch_env(raw, "http://127.0.0.1:8787", "sf_live_h_abc", Provider::Anthropic);
        assert!(out.contains("FOO=bar"));
        assert!(out.contains("ANTHROPIC_API_KEY=sf_live_h_abc"));
        assert!(out.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:8787"));
        assert!(!out.contains("ANTHROPIC_API_KEY=old"));
    }
}
