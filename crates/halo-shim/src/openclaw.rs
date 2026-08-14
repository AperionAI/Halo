//! Point an OpenClaw Gateway at Halo.
//!
//! OpenClaw ignores process env (`ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY`)
//! on service installs. The field-verified path is a three-part patch of
//! OpenClaw's own files. The transformers here are pure JSON so they can be
//! unit-tested without a live Gateway.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Default model entry OpenClaw's schema requires when the provider block
/// has no `models` array yet. Field-verified against OpenClaw 2026.
const DEFAULT_MODEL_ID: &str = "claude-sonnet-4-5";
const DEFAULT_MODEL_NAME: &str = "Claude Sonnet 4.5";

/// Merge Halo's listen URL + virtual key into `openclaw.json`.
///
/// Sets `models.providers.anthropic.baseUrl`, `apiKey`, and
/// `request.allowPrivateNetwork` (the SSRF flag). Leaves any existing
/// `models` array alone; if it's missing, inserts the default so
/// `openclaw config validate` still passes.
pub fn patch_openclaw_json(raw: &str, base_url: &str, vkey: &str) -> Result<String> {
    let mut root: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(raw).context("parsing openclaw.json")?
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("openclaw.json root must be an object"))?;

    let models = obj.entry("models").or_insert_with(|| json!({}));
    let models_obj = models
        .as_object_mut()
        .ok_or_else(|| anyhow!("openclaw.json models must be an object"))?;
    let providers = models_obj.entry("providers").or_insert_with(|| json!({}));
    let providers_obj = providers
        .as_object_mut()
        .ok_or_else(|| anyhow!("openclaw.json models.providers must be an object"))?;
    let anthropic = providers_obj.entry("anthropic").or_insert_with(|| json!({}));
    let anth_obj = anthropic
        .as_object_mut()
        .ok_or_else(|| anyhow!("openclaw.json models.providers.anthropic must be an object"))?;

    anth_obj.insert("baseUrl".into(), json!(base_url));
    anth_obj.insert("apiKey".into(), json!(vkey));

    let request = anth_obj.entry("request").or_insert_with(|| json!({}));
    let request_obj = request
        .as_object_mut()
        .ok_or_else(|| anyhow!("openclaw.json models.providers.anthropic.request must be an object"))?;
    request_obj.insert("allowPrivateNetwork".into(), json!(true));

    let needs_models = match anth_obj.get("models") {
        Some(Value::Array(a)) if !a.is_empty() => false,
        _ => true,
    };
    if needs_models {
        anth_obj.insert(
            "models".into(),
            json!([{ "id": DEFAULT_MODEL_ID, "name": DEFAULT_MODEL_NAME }]),
        );
    }

    Ok(serde_json::to_string_pretty(&root)?)
}

/// Write Halo's virtual key into OpenClaw's auth store.
///
/// The auth store takes precedence over `models.providers.*.apiKey`, so
/// skipping this is how you get "routes to Halo, presents the real key,
/// Halo 401s".
pub fn patch_auth_profiles(raw: &str, vkey: &str) -> Result<String> {
    let mut root: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(raw).context("parsing auth-profiles.json")?
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("auth-profiles.json root must be an object"))?;
    let profiles = obj.entry("profiles").or_insert_with(|| json!({}));
    let profiles_obj = profiles
        .as_object_mut()
        .ok_or_else(|| anyhow!("auth-profiles.json profiles must be an object"))?;
    let entry = profiles_obj
        .entry("anthropic:default")
        .or_insert_with(|| json!({}));
    let entry_obj = entry
        .as_object_mut()
        .ok_or_else(|| anyhow!("auth-profiles.json profiles['anthropic:default'] must be an object"))?;
    entry_obj.insert("key".into(), json!(vkey));
    Ok(serde_json::to_string_pretty(&root)?)
}

/// Paths OpenClaw actually reads. `config` is `openclaw.json`; `auth` is
/// `agents/<runtime>/agent/auth-profiles.json`.
#[derive(Debug)]
pub struct OpenclawPaths {
    pub config: PathBuf,
    pub auth: PathBuf,
    pub runtime_agent: String,
}

impl OpenclawPaths {
    pub fn resolve(home: PathBuf, runtime_agent: Option<&str>, halo_agent: &str) -> Result<Self> {
        let runtime = discover_runtime_agent(&home, runtime_agent, halo_agent)?;
        let config = home.join("openclaw.json");
        let auth = home
            .join("agents")
            .join(&runtime)
            .join("agent")
            .join("auth-profiles.json");
        Ok(Self {
            config,
            auth,
            runtime_agent: runtime,
        })
    }
}

fn discover_runtime_agent(home: &Path, explicit: Option<&str>, halo_agent: &str) -> Result<String> {
    if let Some(name) = explicit {
        return Ok(name.to_string());
    }
    let agents = home.join("agents");
    let named = agents.join(halo_agent);
    if named.is_dir() {
        return Ok(halo_agent.to_string());
    }
    let main = agents.join("main");
    if main.is_dir() {
        return Ok("main".to_string());
    }
    if agents.is_dir() {
        let mut dirs: Vec<String> = std::fs::read_dir(&agents)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        dirs.sort();
        if dirs.len() == 1 {
            return Ok(dirs.into_iter().next().unwrap());
        }
        if !dirs.is_empty() {
            anyhow::bail!(
                "multiple OpenClaw agents under {} ({}); pass --runtime-agent",
                agents.display(),
                dirs.join(", ")
            );
        }
    }
    anyhow::bail!(
        "no OpenClaw agent dir under {}/agents; pass --runtime-agent or run OpenClaw once first",
        home.display()
    )
}

/// Patch both files. `dry_run` returns the would-be contents without writing.
pub fn apply(
    paths: &OpenclawPaths,
    base_url: &str,
    vkey: &str,
    dry_run: bool,
) -> Result<ApplyResult> {
    if !paths.config.exists() {
        anyhow::bail!(
            "no {} -- is OpenClaw installed for this user? (override with --home)",
            paths.config.display()
        );
    }
    let config_raw = std::fs::read_to_string(&paths.config)?;
    let config_out = patch_openclaw_json(&config_raw, base_url, vkey)?;
    let auth_raw = if paths.auth.exists() {
        std::fs::read_to_string(&paths.auth)?
    } else {
        String::new()
    };
    let auth_out = patch_auth_profiles(&auth_raw, vkey)?;

    if !dry_run {
        backup_if_exists(&paths.config)?;
        crate::util::atomic_write_0600(&paths.config, config_out.as_bytes())?;
        if let Some(parent) = paths.auth.parent() {
            std::fs::create_dir_all(parent)?;
        }
        backup_if_exists(&paths.auth)?;
        crate::util::atomic_write_0600(&paths.auth, auth_out.as_bytes())?;
    }

    Ok(ApplyResult {
        config: paths.config.clone(),
        auth: paths.auth.clone(),
        runtime_agent: paths.runtime_agent.clone(),
        config_out,
        auth_out,
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

pub struct ApplyResult {
    pub config: PathBuf,
    pub auth: PathBuf,
    pub runtime_agent: String,
    pub config_out: String,
    pub auth_out: String,
    pub wrote: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_base_url_key_and_ssrf_flag() {
        let out = patch_openclaw_json("{}", "http://127.0.0.1:8787", "sf_live_claw_abc").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let p = &v["models"]["providers"]["anthropic"];
        assert_eq!(p["baseUrl"], "http://127.0.0.1:8787");
        assert_eq!(p["apiKey"], "sf_live_claw_abc");
        assert_eq!(p["request"]["allowPrivateNetwork"], true);
        assert_eq!(p["models"][0]["id"], DEFAULT_MODEL_ID);
    }

    #[test]
    fn keeps_existing_models_array() {
        let raw = r#"{
          "models": {
            "providers": {
              "anthropic": {
                "models": [{ "id": "claude-opus-4", "name": "Opus" }]
              }
            }
          }
        }"#;
        let out = patch_openclaw_json(raw, "http://127.0.0.1:8787", "sf_live_claw_abc").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["models"]["providers"]["anthropic"]["models"][0]["id"], "claude-opus-4");
        assert_eq!(
            v["models"]["providers"]["anthropic"]["request"]["allowPrivateNetwork"],
            true
        );
    }

    #[test]
    fn nests_ssrf_flag_under_provider_not_top_level() {
        let out = patch_openclaw_json("{}", "http://127.0.0.1:8787", "k").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("request").is_none());
        assert_eq!(
            v["models"]["providers"]["anthropic"]["request"]["allowPrivateNetwork"],
            true
        );
    }

    #[test]
    fn auth_store_sets_anthropic_default_key() {
        let out = patch_auth_profiles("{}", "sf_live_claw_abc").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["profiles"]["anthropic:default"]["key"], "sf_live_claw_abc");
    }

    #[test]
    fn auth_store_preserves_other_profile_fields() {
        let raw = r#"{ "profiles": { "anthropic:default": { "key": "old", "provider": "anthropic" } } }"#;
        let out = patch_auth_profiles(raw, "sf_live_claw_abc").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["profiles"]["anthropic:default"]["key"], "sf_live_claw_abc");
        assert_eq!(v["profiles"]["anthropic:default"]["provider"], "anthropic");
    }

    #[test]
    fn discover_prefers_matching_halo_agent_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("agents/claw")).unwrap();
        std::fs::create_dir_all(dir.path().join("agents/main")).unwrap();
        let p = OpenclawPaths::resolve(dir.path().to_path_buf(), None, "claw").unwrap();
        assert_eq!(p.runtime_agent, "claw");
    }

    #[test]
    fn discover_errors_on_ambiguous_agents() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("agents/a")).unwrap();
        std::fs::create_dir_all(dir.path().join("agents/b")).unwrap();
        let err = OpenclawPaths::resolve(dir.path().to_path_buf(), None, "claw").unwrap_err();
        assert!(err.to_string().contains("--runtime-agent"));
    }

    #[test]
    fn apply_dry_run_does_not_write() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("agents/main/agent")).unwrap();
        std::fs::write(dir.path().join("openclaw.json"), "{}").unwrap();
        let paths = OpenclawPaths::resolve(dir.path().to_path_buf(), Some("main"), "claw").unwrap();
        let r = apply(&paths, "http://127.0.0.1:8787", "sf_live_claw_abc", true).unwrap();
        assert!(!r.wrote);
        let on_disk = std::fs::read_to_string(dir.path().join("openclaw.json")).unwrap();
        assert_eq!(on_disk, "{}");
    }

    #[test]
    fn apply_writes_and_backs_up() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("agents/main/agent")).unwrap();
        std::fs::write(dir.path().join("openclaw.json"), "{}\n").unwrap();
        let paths = OpenclawPaths::resolve(dir.path().to_path_buf(), Some("main"), "claw").unwrap();
        let r = apply(&paths, "http://127.0.0.1:8787", "sf_live_claw_abc", false).unwrap();
        assert!(r.wrote);
        let written = std::fs::read_to_string(&paths.config).unwrap();
        assert!(written.contains("127.0.0.1:8787"));
        let bak = std::fs::read_to_string(format!("{}.halo-bak", paths.config.display())).unwrap();
        assert_eq!(bak, "{}\n");
        let auth = std::fs::read_to_string(&paths.auth).unwrap();
        assert!(auth.contains("sf_live_claw_abc"));
    }
}
