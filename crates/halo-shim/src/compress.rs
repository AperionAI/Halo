//! Prompt compression.
//!
//! Ported verbatim from `src/metacache_api_routes.rs`: 39 verbose-phrase
//! reductions and 29 abbreviations. These are pure functions with zero
//! dependencies.
//!
//! Two important design decisions specific to Halo:
//!
//! 1. **Compression must survive to the wire.** The main proxy's
//!    `semantic_compression.rs` resolved compressed references back to full
//!    text before the provider ever saw them, so the "savings" weren't real on
//!    that path. Halo genuinely rewrites the outbound body and the COGS
//!    estimator only ever counts what actually left the machine -- so a
//!    reported saving is a real saving.
//!
//! 2. **Verbose-phrase reduction is safe and on by default; abbreviations are
//!    aggressive and off by default.** The abbreviation table includes things
//!    like "and" -> "&" and "about" -> "~" that can change meaning. Halo's rule
//!    is never to degrade output silently, so aggressive abbreviations are
//!    strictly opt-in.

/// Multi-word verbose phrases -> shorter, meaning-preserving equivalents.
pub const VERBOSE_PHRASES: &[(&str, &str)] = &[
    ("In order to", "To"),
    ("Due to the fact that", "Because"),
    ("In the event that", "If"),
    ("At this point in time", "Now"),
    ("For the purpose of", "For"),
    ("In a manner of speaking", ""),
    ("It is important to note that", "Note that"),
    ("Despite the fact that", "Although"),
    ("With reference to", "Regarding"),
    ("In light of the fact that", "Since"),
    ("In the vicinity of", "Near"),
    ("A majority of", "Most"),
    ("In spite of the fact that", "Although"),
    ("With regard to", "About"),
    ("Take into consideration", "Consider"),
    ("In relation to", "About"),
    ("In terms of", "Regarding"),
    ("In the course of", "During"),
    ("In the near future", "Soon"),
    ("In close proximity to", "Near"),
    ("At the present time", "Now"),
    ("It would appear that", "Apparently"),
    ("It could be argued that", "Arguably"),
    ("Please be informed that", "Note that"),
    ("Please note that", "Note:"),
    ("For your information", "Note:"),
    ("I would like to", "I'll"),
    ("We would like to", "We'll"),
    ("You would like to", "You'll"),
    ("They would like to", "They'll"),
    ("Would like to", "Will"),
    ("I am going to", "I'll"),
    ("You are going to", "You'll"),
    ("We are going to", "We'll"),
    ("He is going to", "He'll"),
    ("She is going to", "She'll"),
    ("They are going to", "They'll"),
    ("Is going to", "Will"),
    ("Are going to", "Will"),
];

/// Aggressive single-word/symbol abbreviations. OFF by default -- can alter
/// meaning. Opt in via config only.
pub const ABBREVIATIONS: &[(&str, &str)] = &[
    ("for example", "e.g."),
    ("that is", "i.e."),
    ("et cetera", "etc."),
    ("and so on", "etc."),
    ("in other words", "i.e."),
    ("with respect to", "re:"),
    ("with regard to", "re:"),
    ("versus", "vs."),
    ("approximately", "~"),
    ("about", "~"),
    ("without", "w/o"),
    ("with", "w/"),
    ("through", "thru"),
    ("number", "#"),
    ("numbers", "#s"),
    ("percent", "%"),
    ("percentage", "%"),
    ("increase", "↑"),
    ("decrease", "↓"),
    ("up", "↑"),
    ("down", "↓"),
    ("greater than", ">"),
    ("less than", "<"),
    ("equal to", "="),
    ("equivalent to", "="),
    ("not equal to", "≠"),
    ("therefore", "∴"),
    ("because", "∵"),
    ("and", "&"),
];

/// Compress a single text span.
pub fn compress_text(text: &str, verbose: bool, aggressive: bool) -> String {
    let mut out = text.to_string();
    if verbose {
        for (phrase, replacement) in VERBOSE_PHRASES {
            out = out.replace(phrase, replacement);
        }
    }
    if aggressive {
        for (term, abbrev) in ABBREVIATIONS {
            out = out.replace(term, abbrev);
        }
    }
    out
}

/// Result of compressing a whole request body.
pub struct Compressed {
    /// Rewritten body (only present when something actually changed).
    pub body: Option<String>,
    /// chars-after / chars-before over the text fields we touched (1.0 = no
    /// change). This is what the COGS estimator uses -- it reflects only text
    /// that genuinely leaves the machine compressed.
    pub ratio: f64,
}

/// Compress the system prompt and message text of a chat/messages body.
pub fn compress_body(body: &str, verbose: bool, aggressive: bool) -> Compressed {
    if !verbose && !aggressive {
        return Compressed {
            body: None,
            ratio: 1.0,
        };
    }
    let mut json: serde_json::Value = match serde_json::from_str(body) {
        Ok(j) => j,
        Err(_) => {
            return Compressed {
                body: None,
                ratio: 1.0,
            }
        }
    };

    let mut before = 0usize;
    let mut after = 0usize;

    if let Some(sys) = json.get_mut("system") {
        transform_value(sys, verbose, aggressive, &mut before, &mut after);
    }
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for m in messages.iter_mut() {
            if let Some(c) = m.get_mut("content") {
                transform_value(c, verbose, aggressive, &mut before, &mut after);
            }
        }
    }

    let ratio = if before == 0 {
        1.0
    } else {
        after as f64 / before as f64
    };

    if after == before {
        Compressed { body: None, ratio }
    } else {
        Compressed {
            body: serde_json::to_string(&json).ok(),
            ratio,
        }
    }
}

fn transform_value(
    v: &mut serde_json::Value,
    verbose: bool,
    aggressive: bool,
    before: &mut usize,
    after: &mut usize,
) {
    match v {
        serde_json::Value::String(s) => {
            *before += s.chars().count();
            let c = compress_text(s, verbose, aggressive);
            *after += c.chars().count();
            *s = c;
        }
        serde_json::Value::Array(arr) => {
            for block in arr.iter_mut() {
                if let Some(serde_json::Value::String(s)) = block.get_mut("text") {
                    *before += s.chars().count();
                    let c = compress_text(s, verbose, aggressive);
                    *after += c.chars().count();
                    *s = c;
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_reduction_shrinks_and_preserves_meaning() {
        let out = compress_text("In order to win, Due to the fact that we try.", true, false);
        assert!(out.starts_with("To win"));
        assert!(out.contains("Because"));
    }

    #[test]
    fn aggressive_off_by_default_leaves_words() {
        // "about" must NOT become "~" unless aggressive is on.
        let out = compress_text("tell me about this", true, false);
        assert!(out.contains("about"));
    }

    #[test]
    fn body_compression_reports_real_ratio() {
        let body = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"In order to test in order to test"}]}"#;
        let c = compress_body(body, true, false);
        assert!(c.body.is_some());
        assert!(c.ratio < 1.0);
        assert!(c.body.unwrap().contains("To test"));
    }
}
