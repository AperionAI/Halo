//! Lowest-cost / effort router.
//!
//! Pure function: no Redis, no tokio, no HTTP. Copied from
//! `src/effort_router.rs` in the Smartflow tree — keep the two files in sync.
//!
//! Shape matches NVIDIA Switchyard's `efficient_first` picker: corroborative
//! signed score, `tanh` confidence, default cheap when below threshold.
//! We do not call a judge model. We do not own the HTTP path.

use serde::{Deserialize, Serialize};

/// One full signal is ~0.46 after tanh — just under the 0.5 default
/// threshold, so a second corroborating signal is what commits the pick.
const SIGNAL: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortTier {
    Efficient,
    Capable,
}

impl EffortTier {
    pub fn as_str(self) -> &'static str {
        match self {
            EffortTier::Efficient => "efficient",
            EffortTier::Capable => "capable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LcrMode {
    Off,
    Auto,
    Local,
    Frontier,
}

impl LcrMode {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => LcrMode::Auto,
            "local" | "efficient" | "cheap" => LcrMode::Local,
            "frontier" | "capable" | "cloud" => LcrMode::Frontier,
            _ => LcrMode::Off,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LcrMode::Off => "off",
            LcrMode::Auto => "auto",
            LcrMode::Local => "local",
            LcrMode::Frontier => "frontier",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LcrSettings {
    pub mode: LcrMode,
    pub threshold: f64,
    pub efficient_provider: String,
    pub efficient_model: Option<String>,
    pub capable_provider: Option<String>,
    pub capable_model: Option<String>,
}

impl Default for LcrSettings {
    fn default() -> Self {
        Self {
            mode: LcrMode::Off,
            threshold: 0.5,
            // Empty = do not switch providers. Model-only downgrade on the
            // current provider. Never default to ollama (it usually isn't there).
            efficient_provider: String::new(),
            efficient_model: None,
            capable_provider: None,
            capable_model: None,
        }
    }
}

impl LcrSettings {
    pub fn from_env() -> Self {
        let mut s = Self::default();
        if let Ok(v) = std::env::var("SMARTFLOW_LCR") {
            s.mode = LcrMode::parse(&v);
        }
        if let Ok(v) = std::env::var("SMARTFLOW_LCR_THRESHOLD") {
            if let Ok(t) = v.parse::<f64>() {
                s.threshold = t.clamp(0.0, 1.0);
            }
        }
        if let Ok(v) = std::env::var("SMARTFLOW_LCR_EFFICIENT_PROVIDER") {
            let t = v.trim();
            if !t.is_empty() {
                s.efficient_provider = t.to_ascii_lowercase();
            }
        }
        s.efficient_model = std::env::var("SMARTFLOW_LCR_EFFICIENT_MODEL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        s.capable_provider = std::env::var("SMARTFLOW_LCR_CAPABLE_PROVIDER")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty());
        s.capable_model = std::env::var("SMARTFLOW_LCR_CAPABLE_MODEL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        s
    }

    /// Header wins, then policy, then env. Empty header is ignored.
    pub fn with_overrides(mut self, header: Option<&str>, policy_mode: Option<&str>) -> Self {
        if let Some(h) = header.map(str::trim).filter(|s| !s.is_empty()) {
            self.mode = LcrMode::parse(h);
        } else if let Some(p) = policy_mode.map(str::trim).filter(|s| !s.is_empty()) {
            self.mode = LcrMode::parse(p);
        }
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct EffortSignals<'a> {
    /// Request-side conversation stage (`greeting`, `analysis`, …).
    pub stage: Option<&'a str>,
    /// Intent kind or VAS signature (`Definition` or `Definition:typescript`).
    pub intent: Option<&'a str>,
    pub intent_confidence: f64,
    /// `chat` / `code` / `embedding`.
    pub task_class: &'a str,
    pub prompt_chars: usize,
    pub prior_stage: Option<&'a str>,
    pub mcp_error: bool,
    /// `T1` / `T2` / `T3`. Bias only.
    pub action_risk: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffortDecision {
    pub tier: EffortTier,
    pub confidence: f64,
    pub reasons: Vec<String>,
    pub defaulted: bool,
}

impl EffortDecision {
    pub fn reason_line(&self) -> String {
        format!(
            "lcr:{} conf={:.2} defaulted={} [{}]",
            self.tier.as_str(),
            self.confidence,
            self.defaulted,
            self.reasons.join(",")
        )
    }
}

fn tanh(x: f64) -> f64 {
    let e = (2.0 * x).exp();
    (e - 1.0) / (e + 1.0)
}

fn intent_kind(raw: &str) -> &str {
    raw.split(':').next().unwrap_or(raw).trim()
}

fn norm(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// Keyword hints so Halo (no ConversationClassifier) can fill stage/intent
/// from the user prompt. Smartflow prefers the real classifiers.
pub fn infer_stage(text: &str) -> Option<&'static str> {
    let t = text.to_ascii_lowercase();
    if t.contains("error") || t.contains("traceback") || t.contains("exception") {
        return Some("error");
    }
    if t.contains("analyze") || t.contains("compare") || t.contains("evaluate") {
        return Some("analysis");
    }
    if t.contains("clarify") || t.contains("what do you mean") {
        return Some("clarification");
    }
    if t.contains("hello") || t.contains("hi there") || t.contains("good morning") {
        return Some("greeting");
    }
    if t.contains("thanks") || t.contains("that works") || t.contains("done") {
        return Some("conclusion");
    }
    if t.contains("verify") || t.contains("did it work") {
        return Some("verification");
    }
    None
}

pub fn infer_intent(text: &str) -> Option<&'static str> {
    let t = text.to_ascii_lowercase();
    if t.contains("vs ") || t.contains("versus") || t.contains("compared to") {
        return Some("Comparison");
    }
    if t.contains("not working") || t.contains("fix ") || t.contains("error") {
        return Some("Troubleshooting");
    }
    if t.contains("what is") || t.contains("define ") || t.contains("explain ") {
        return Some("Definition");
    }
    if t.contains("how to") || t.contains("how do i") {
        return Some("Instruction");
    }
    None
}

/// Score effort. Positive raw = Capable, negative = Efficient.
/// Hard override: MCP error or stage=error → Capable regardless of threshold.
pub fn score(signals: &EffortSignals<'_>, threshold: f64) -> EffortDecision {
    let threshold = threshold.clamp(0.0, 1.0);
    let mut raw = 0.0_f64;
    let mut reasons: Vec<String> = Vec::new();

    let stage = signals.stage.map(|s| norm(s));
    let prior = signals.prior_stage.map(|s| norm(s));
    let intent = signals.intent.map(intent_kind).map(|s| s.to_string());
    let class = signals.task_class.trim().to_ascii_lowercase();

    let mut hard_capable = signals.mcp_error;
    if signals.mcp_error {
        reasons.push("mcp_error".into());
        raw += SIGNAL;
    }

    if let Some(ref st) = stage {
        match st.as_str() {
            "error" => {
                hard_capable = true;
                raw += SIGNAL;
                reasons.push("stage:error".into());
            }
            "analysis" | "clarification" => {
                raw += SIGNAL;
                reasons.push(format!("stage:{st}"));
            }
            "greeting" | "conclusion" | "verification" | "maintenance" => {
                raw -= SIGNAL;
                reasons.push(format!("stage:{st}"));
            }
            _ => {}
        }
    }

    if let Some(ref kind) = intent {
        let k = kind.as_str();
        match k {
            "Troubleshooting" | "Comparison" => {
                raw += SIGNAL;
                reasons.push(format!("intent:{k}"));
            }
            "Definition" => {
                raw -= SIGNAL;
                reasons.push(format!("intent:{k}"));
            }
            _ => {}
        }
    }

    if class == "embedding" {
        raw -= SIGNAL;
        reasons.push("class:embedding".into());
    }
    // Coding is hard work. A short "fix this" must not commit to the cheap lane.
    if class == "code" {
        raw += SIGNAL;
        reasons.push("class:code".into());
    }

    let skip_short = class == "code"
        || intent
            .as_deref()
            .is_some_and(|k| k == "Troubleshooting" || k == "Comparison");
    if signals.prompt_chars > 0 && signals.prompt_chars < 200 && !skip_short {
        raw -= SIGNAL;
        reasons.push("short_prompt".into());
    } else if signals.prompt_chars > 2000 {
        raw += SIGNAL;
        reasons.push("long_prompt".into());
    }

    if let Some(ref p) = prior {
        match p.as_str() {
            "error" => {
                raw += SIGNAL;
                reasons.push("prior:error".into());
            }
            "solution" | "verification" => {
                raw -= SIGNAL;
                reasons.push(format!("prior:{p}"));
            }
            _ => {}
        }
    }

    if signals
        .action_risk
        .map(|s| s.eq_ignore_ascii_case("T3"))
        .unwrap_or(false)
    {
        raw += SIGNAL;
        reasons.push("risk:T3".into());
    }

    let confidence = tanh(raw.abs());
    let signed_tier = if raw < 0.0 {
        EffortTier::Efficient
    } else if raw > 0.0 {
        EffortTier::Capable
    } else {
        EffortTier::Efficient
    };

    if hard_capable {
        return EffortDecision {
            tier: EffortTier::Capable,
            confidence: confidence.max(0.99),
            reasons,
            defaulted: false,
        };
    }

    let defaulted = confidence < threshold;
    let tier = if defaulted {
        EffortTier::Efficient
    } else {
        signed_tier
    };

    EffortDecision {
        tier,
        confidence,
        reasons,
        defaulted,
    }
}

/// Decide the tier for a configured LCR mode. `Off` returns None.
pub fn decide(
    mode: LcrMode,
    signals: &EffortSignals<'_>,
    threshold: f64,
) -> Option<EffortDecision> {
    match mode {
        LcrMode::Off => None,
        LcrMode::Local => Some(EffortDecision {
            tier: EffortTier::Efficient,
            confidence: 1.0,
            reasons: vec!["mode:local".into()],
            defaulted: false,
        }),
        LcrMode::Frontier => Some(EffortDecision {
            tier: EffortTier::Capable,
            confidence: 1.0,
            reasons: vec!["mode:frontier".into()],
            defaulted: false,
        }),
        LcrMode::Auto => Some(score(signals, threshold)),
    }
}

/// Quality-escalation: Efficient answered but the body is empty, an error
/// object, or an explicit refuse. 5xx is included so callers can use one fn.
pub fn should_escalate_quality(status: u16, body: &[u8]) -> bool {
    if status >= 500 {
        return true;
    }
    if body.iter().all(|b| b.is_ascii_whitespace()) || body.len() < 8 {
        return true;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    if v.get("error").is_some() {
        return true;
    }
    let content = extract_assistant_text(&v);
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.contains("i can't assist")
        || lower.contains("i cannot assist")
        || lower.contains("i'm unable to help")
        || lower.contains("i am unable to help")
}

fn extract_assistant_text(v: &serde_json::Value) -> String {
    if let Some(s) = v.pointer("/choices/0/message/content").and_then(|x| x.as_str()) {
        return s.to_string();
    }
    if let Some(s) = v.pointer("/content/0/text").and_then(|x| x.as_str()) {
        return s.to_string();
    }
    if let Some(arr) = v.get("content").and_then(|c| c.as_array()) {
        let mut out = String::new();
        for part in arr {
            if let Some(t) = part.get("text").and_then(|x| x.as_str()) {
                out.push_str(t);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    v.get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string()
}

/// True when the chat body asks for SSE (`"stream": true`).
pub fn body_wants_stream(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        let s = String::from_utf8_lossy(body);
        return s.contains("\"stream\"") && (s.contains(":true") || s.contains(": true"));
    };
    v.get("stream").and_then(|x| x.as_bool()).unwrap_or(false)
}

/// Append an LCR line onto an existing routing reason (eco-downgrade, etc.).
pub fn append_routing_reason(existing: Option<&str>, line: &str) -> String {
    match existing.map(str::trim).filter(|s| !s.is_empty()) {
        Some(prev) => format!("{prev}; {line}"),
        None => line.to_string(),
    }
}

pub fn is_local_provider(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "ollama" | "vllm" | "lmstudio" | "localai" | "llamacpp" | "nvidia_nim"
    )
}

/// Cloud → different cloud is a key-isolation bug. Local GPU hops are allowed.
pub fn may_switch_provider(from: &str, to: &str) -> bool {
    let a = from.trim().to_ascii_lowercase();
    let b = to.trim().to_ascii_lowercase();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || is_local_provider(&b)
}

/// Same-provider cheap model when the operator didn't set EFFICIENT_MODEL.
pub fn cheap_model_for_provider(provider: &str) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "openai" | "azure" | "azure_ai" => Some("gpt-4o-mini"),
        "anthropic" => Some("claude-3-5-haiku-20241022"),
        "google" | "gemini" => Some("gemini-2.0-flash"),
        "openrouter" => Some("openai/gpt-4o-mini"),
        "groq" => Some("llama-3.1-8b-instant"),
        _ => None,
    }
}

/// Efficient wire change: maybe switch provider (local only / same name),
/// always a model if we know one. `(None, None)` = stamp the decision, don't
/// touch the request.
pub fn resolve_efficient_hop(
    current_provider: &str,
    settings: &LcrSettings,
) -> (Option<String>, Option<String>) {
    let current = current_provider.trim().to_ascii_lowercase();
    let want = settings.efficient_provider.trim().to_ascii_lowercase();
    let can_switch = !want.is_empty() && want != current && may_switch_provider(&current, &want);
    let switch = if can_switch { Some(want.clone()) } else { None };
    let stay = switch.as_deref().unwrap_or(current.as_str());
    // Env model is for the *wanted* cheap provider. If we refused a cloud
    // hop, don't stamp gpt-4o-mini onto Anthropic.
    let denied_cloud_switch = !want.is_empty() && want != current && !can_switch;
    let model = if denied_cloud_switch {
        cheap_model_for_provider(stay).map(|s| s.to_string())
    } else {
        settings
            .efficient_model
            .clone()
            .filter(|m| !m.is_empty())
            .or_else(|| cheap_model_for_provider(stay).map(|s| s.to_string()))
    };
    (switch, model)
}

/// Rewrite `model` in an OpenAI/Anthropic-style JSON body. Returns true if written.
pub fn rewrite_json_model(body: &mut Vec<u8>, model: &str) -> bool {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let Some(obj) = v.as_object_mut() else {
        return false;
    };
    obj.insert("model".to_string(), serde_json::Value::String(model.to_string()));
    match serde_json::to_vec(&v) {
        Ok(b) => {
            *body = b;
            true
        }
        Err(_) => false,
    }
}

/// String-body variant for Halo (outbound is a `String`).
pub fn rewrite_json_model_str(body: &mut String, model: &str) -> bool {
    let mut bytes = body.as_bytes().to_vec();
    if !rewrite_json_model(&mut bytes, model) {
        return false;
    }
    match String::from_utf8(bytes) {
        Ok(s) => {
            *body = s;
            true
        }
        Err(_) => false,
    }
}

/// Host (no scheme) for the small set of providers LCR actually switches to.
pub fn host_for_provider(provider: &str, ollama_base: &str) -> String {
    let strip = |u: &str| {
        u.trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_string()
    };
    match provider {
        "openrouter" => "openrouter.ai".into(),
        "ollama" => strip(ollama_base),
        "vllm" => strip(
            &std::env::var("VLLM_BASE_URL").unwrap_or_else(|_| "http://localhost:8000".into()),
        ),
        "lmstudio" => strip(
            &std::env::var("LMSTUDIO_BASE_URL").unwrap_or_else(|_| "http://localhost:1234".into()),
        ),
        "localai" => strip(
            &std::env::var("LOCALAI_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".into()),
        ),
        "llamacpp" => strip(
            &std::env::var("LLAMACPP_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".into()),
        ),
        other => format!("{other}.api.endpoint"),
    }
}

/// True when a chat-completions / messages body carries a tool error.
pub fn body_has_tool_error(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    if v.get("isError").and_then(|x| x.as_bool()) == Some(true) {
        return true;
    }
    let msgs = v
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    for m in msgs {
        if m.get("isError").and_then(|x| x.as_bool()) == Some(true) {
            return true;
        }
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "tool" || role == "function" {
            if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
                let l = c.to_ascii_lowercase();
                if l.contains("\"iserror\":true") || l.contains("error:") {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig<'a>(
        stage: Option<&'a str>,
        intent: Option<&'a str>,
        class: &'a str,
        chars: usize,
    ) -> EffortSignals<'a> {
        EffortSignals {
            stage,
            intent,
            intent_confidence: 0.9,
            task_class: class,
            prompt_chars: chars,
            prior_stage: None,
            mcp_error: false,
            action_risk: None,
        }
    }

    #[test]
    fn greeting_definition_short_is_efficient() {
        let d = score(&sig(Some("greeting"), Some("Definition:foo"), "chat", 40), 0.5);
        assert_eq!(d.tier, EffortTier::Efficient);
        assert!(!d.defaulted);
        assert!(d.confidence >= 0.5);
    }

    #[test]
    fn error_troubleshooting_is_capable() {
        let d = score(
            &sig(Some("error"), Some("Troubleshooting"), "chat", 400),
            0.5,
        );
        assert_eq!(d.tier, EffortTier::Capable);
        assert!(!d.defaulted);
    }

    #[test]
    fn unknown_defaults_efficient_first() {
        let d = score(&sig(None, None, "chat", 500), 0.5);
        assert_eq!(d.tier, EffortTier::Efficient);
        assert!(d.defaulted);
        assert!(d.confidence < 0.5);
    }

    #[test]
    fn one_signal_stays_under_threshold() {
        let d = score(&sig(Some("greeting"), None, "chat", 500), 0.5);
        assert_eq!(d.tier, EffortTier::Efficient);
        assert!(d.defaulted, "tanh(0.5)≈0.46; efficient_first default");
    }

    #[test]
    fn mcp_error_hard_override() {
        let mut s = sig(Some("greeting"), Some("Definition"), "chat", 40);
        s.mcp_error = true;
        let d = score(&s, 0.5);
        assert_eq!(d.tier, EffortTier::Capable);
        assert!(!d.defaulted);
    }

    #[test]
    fn t3_alone_does_not_commit() {
        let mut s = sig(None, None, "chat", 500);
        s.action_risk = Some("T3");
        let d = score(&s, 0.5);
        assert_eq!(d.tier, EffortTier::Efficient);
        assert!(d.defaulted);
    }

    #[test]
    fn embedding_class_plus_short_is_efficient() {
        let d = score(&sig(None, None, "embedding", 80), 0.5);
        assert_eq!(d.tier, EffortTier::Efficient);
        assert!(!d.defaulted);
    }

    #[test]
    fn short_troubleshooting_code_is_capable() {
        let d = score(&sig(None, Some("Troubleshooting"), "code", 80), 0.5);
        assert_eq!(d.tier, EffortTier::Capable);
        assert!(!d.defaulted);
    }

    #[test]
    fn anthropic_does_not_switch_to_openai() {
        let s = LcrSettings {
            mode: LcrMode::Auto,
            efficient_provider: "openai".into(),
            efficient_model: Some("gpt-4o-mini".into()),
            ..Default::default()
        };
        let (switch, model) = resolve_efficient_hop("anthropic", &s);
        assert!(switch.is_none(), "must not steal onto platform OpenAI");
        assert_eq!(model.as_deref(), Some("claude-3-5-haiku-20241022"));
    }

    #[test]
    fn unset_provider_stays_put_and_picks_haiku() {
        let s = LcrSettings::default();
        let (switch, model) = resolve_efficient_hop("anthropic", &s);
        assert!(switch.is_none());
        assert_eq!(model.as_deref(), Some("claude-3-5-haiku-20241022"));
    }

    #[test]
    fn local_gpu_switch_is_allowed() {
        let s = LcrSettings {
            efficient_provider: "ollama".into(),
            efficient_model: Some("qwen2.5".into()),
            ..Default::default()
        };
        let (switch, model) = resolve_efficient_hop("openai", &s);
        assert_eq!(switch.as_deref(), Some("ollama"));
        assert_eq!(model.as_deref(), Some("qwen2.5"));
    }

    #[test]
    fn append_keeps_eco_downgrade() {
        let s = append_routing_reason(Some("eco-downgrade: gpt-4o => gpt-4o-mini"), "lcr:efficient conf=0.90 defaulted=false [stage:greeting]");
        assert!(s.starts_with("eco-downgrade:"));
        assert!(s.contains("lcr:efficient"));
    }

    #[test]
    fn stream_flag_detected() {
        assert!(body_wants_stream(br#"{"model":"gpt-4o","stream":true}"#));
        assert!(!body_wants_stream(br#"{"model":"gpt-4o","stream":false}"#));
    }

    #[test]
    fn mode_local_skips_scorer() {
        let d = decide(LcrMode::Local, &sig(Some("error"), None, "chat", 4000), 0.5).unwrap();
        assert_eq!(d.tier, EffortTier::Efficient);
        assert_eq!(d.reasons, vec!["mode:local"]);
    }

    #[test]
    fn mode_off_is_none() {
        assert!(decide(LcrMode::Off, &sig(None, None, "chat", 10), 0.5).is_none());
    }

    #[test]
    fn header_beats_policy_and_env() {
        let s = LcrSettings {
            mode: LcrMode::Off,
            ..Default::default()
        }
        .with_overrides(Some("frontier"), Some("auto"));
        assert_eq!(s.mode, LcrMode::Frontier);
    }

    #[test]
    fn empty_body_escalates() {
        assert!(should_escalate_quality(200, b"   "));
        assert!(should_escalate_quality(502, b"{\"ok\":true}"));
        let refuse = br#"{"choices":[{"message":{"content":"I can't assist with that."}}]}"#;
        assert!(should_escalate_quality(200, refuse));
        let ok = br#"{"choices":[{"message":{"content":"Rust is a language."}}]}"#;
        assert!(!should_escalate_quality(200, ok));
    }

    #[test]
    fn parse_intent_signature() {
        let d = score(
            &sig(Some("discovery"), Some("Comparison:rust_go"), "chat", 400),
            0.5,
        );
        assert!(d.reasons.iter().any(|r| r == "intent:Comparison"));
    }

    #[test]
    fn golden_cluster1_slice_matches_scorer() {
        let raw = include_str!("effort_golden.json");
        let rows: Vec<serde_json::Value> = serde_json::from_str(raw).unwrap();
        assert!(rows.len() >= 8, "need a soak set, not a couple of hand cases");
        for row in rows {
            let id = row.get("id").and_then(|x| x.as_str()).unwrap_or("?");
            let stage = row.get("stage").and_then(|x| x.as_str());
            let intent = row.get("intent").and_then(|x| x.as_str());
            let class = row
                .get("task_class")
                .and_then(|x| x.as_str())
                .unwrap_or("chat");
            let chars = row.get("prompt_chars").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            let want = row
                .get("expected_tier")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let d = score(&sig(stage, intent, class, chars), 0.5);
            assert_eq!(d.tier.as_str(), want, "golden {id}");
        }
    }
}
