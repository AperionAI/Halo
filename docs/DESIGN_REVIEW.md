# Halo v1 design review

A pass over the initial implementation against how comparable LLM
gateways/proxies (LiteLLM Proxy, Portkey AI Gateway, Helicone, OpenRouter)
solve the same problems, plus a code-level audit. Dated 2026-08-03, before
the first public release.

## Headline finding: streaming was wrongly deferred, now fixed

v1 originally buffered the full provider response even when the caller asked
for `"stream": true`, planning to add real SSE passthrough in v1.1. On review
this was the wrong call and has been fixed in this pass, not deferred:

- **Every comparable gateway streams by default.** Buffering silently
  destroys time-to-first-token for interactive agent sessions -- the caller
  would see nothing until the *entire* completion finished, which for a long
  agentic turn can be tens of seconds. That's not a missing nice-to-have,
  it's a regression against baseline "point your SDK at a different
  `base_url`" behavior, which is the whole value proposition of a transparent
  proxy.
- Long completions also risk client-side read timeouts when buffered end to
  end instead of streamed incrementally.

**What changed** (`src/streaming.rs`, `src/ingress.rs`):

- Provider bytes are forwarded to the client as they arrive (`reqwest`'s
  `Response::chunk()` teed into an `axum::body::Body::from_stream`), not
  buffered. Verified live: a mocked 8-chunk SSE stream paced at 400ms/chunk
  was observed arriving at the client across ~3.2s of wall-clock time, not
  all at once at the end.
- OpenAI-shaped streaming requests automatically get
  `stream_options.include_usage: true` injected (unless the caller already
  set it) so the final SSE chunk always carries token counts -- callers don't
  need to know this is required for accounting to work.
- Anthropic streams need no such opt-in; `message_start` /
  `message_delta` always carry usage.
- Usage is parsed from the accumulated SSE bytes once the stream ends (or is
  aborted -- see below), and billing/telemetry/audit run through the exact
  same `AppState::finalize_llm_call` path as the non-streaming case, so the
  two can't drift apart on how a call is billed.
- Verified live end to end: streamed request -> accurate token extraction
  (42 in / 17 out matched the mock's injected usage chunk exactly) -> correct
  cost via the price table -> audit entry appended to the hash chain.

**Known, accepted limitation:** exact-match caching does not apply to
streamed requests (unchanged from before this pass -- `cachekey` already
excludes `"stream": true` requests). Replaying a cached response as a
synthetic SSE stream is a legitimate v1.1 feature, not a correctness gap.

### Mid-stream stop-loss (new)

Pre-flight budget enforcement (the primary kill-switch mechanism, unchanged)
assumes a conservative 1024-token completion. A pathological runaway
generation streaming far beyond that assumption wouldn't be caught until
*after* the fact under a naive implementation. Added a coarse backstop: while
relaying a stream, if the running byte count implies a cost already >3x the
applicable hard cap, Halo stops pulling further chunks from the provider and
closes the connection. The 3x margin exists specifically so the char/4
token approximation and provider framing overhead can't false-positive on a
normal request. This mirrors the reality that **every** comparable proxy's
budget enforcement is fundamentally pre-flight-oriented; this is a backstop,
not a claim of exact mid-generation billing precision.

## Other findings from the review

### Fixed

1. **MCP `pending` map leak on timeout** (`src/mcp.rs`). A request that timed
   out left its `oneshot::Sender` in the pending-response map forever --
   unbounded growth under sustained timeouts. Now removed on timeout.
2. **Price table fallback risk** (`src/config.rs`, `src/main.rs`). The
   built-in price table is a small, hand-maintained ~12-model approximation,
   unlike LiteLLM's continuously-refreshed price file. Its fallback for an
   unrecognized model is a mid-tier guess, which could meaningfully
   over-charge (or under-charge) a cheap/unusual model -- a real problem for
   a budget/kill-switch product where the numbers need to be trusted. Added
   `price_overrides` to `config.yaml` so this is a config change, not a code
   change, when the built-in table is wrong.
3. **No path for OpenAI-compatible third parties** (`src/keys.rs`,
   `halo-common/src/vkey.rs`, `src/ingress.rs`). Groq, Together, Fireworks,
   and local vLLM/Ollama servers all speak the same
   `/v1/chat/completions` shape as OpenAI, and every comparable gateway lets
   you point at them via a custom base URL. Added `--base-url` to
   `halo agent add` for `--provider openai`.
4. **Cosmetic: agent-add printed a hardcoded `127.0.0.1:8787`** regardless of
   the configured `listen` address in `config.yaml` -- now reads the real
   config.
5. **Cosmetic: `halo report` rendered any spend under a cent as `$0.0000`**,
   which looks indistinguishable from zero cost. Now widens to 6 decimals for
   sub-cent amounts.
6. Minor clippy cleanups (`audit.rs`, `compress.rs`, `mcp.rs`) ahead of the
   first public push.

### Reviewed and intentionally left as-is (documented, not gaps)

- **No automatic retries or cross-provider fallback routing.** This is a
  deliberate scope line, not an oversight: Halo's job is governance/cost
  control on the path an agent already uses, not model routing. That's
  Smartflow's own paid Route tier. Retrying inside Halo would also risk
  double-counting spend against the local ledger.
- **One virtual key = one provider + one model family.** An agent can't
  multiplex OpenAI and Anthropic behind a single key. Matches how real
  provider keys already work; a non-issue in practice since each key maps to
  one upstream account anyway.
- **No per-model allowlists / RBAC on which models an agent may call.**
  Common in enterprise gateways (Portkey, LiteLLM) but out of scope for a
  free-tier, single-operator local shim. Worth revisiting if Halo grows a
  team/fleet mode.
- **Relay's canonical cost recompute doesn't know about a shim's local
  `price_overrides`.** The relay recomputes cost server-side from raw token
  counts using its own default table (this "recompute, don't trust the
  client's number" pattern is intentional and matches the architecture
  plan). A shim with overrides will therefore show slightly different
  numbers locally (`halo report`) than what the relay aggregates across a
  fleet. Acceptable for v1: the relay serves many devices that could have
  different local overrides, so it can't adopt any single one as ground
  truth. If this matters later, the fix is to carry the *effective* per-event
  price alongside raw tokens in the telemetry event rather than recomputing
  provider-side -- deferred since it would also remove the relay's "don't
  trust the client's math" property.

## Bottom line

The core architecture (local virtual keys, pre-flight budget kill-switch,
exact-match cache, compression, metadata-only telemetry with server-side
canonical cost recompute, hash-chained local audit) holds up well against
how LiteLLM/Portkey/Helicone/OpenRouter solve the same problems. The one real
gap was streaming, which is now implemented rather than deferred. Everything
else found was either a small, cheap fix (now applied) or an intentional,
documented scope boundary.

## v1.1: cross-provider semantic cache

Exact-match caching (v1) only catches byte-identical requests. The main
Smartflow proxy's own semantic cache was reviewed as prior art going into this
work, specifically to avoid two gaps found in it:

1. **"First bucket match wins."** A cheap keyword partition (stage/intent) was
   sometimes treated as sufficient on its own, without a similarity re-check
   against the specific candidate. Halo's `semantic_cache.rs` always
   cosine-re-checks every candidate in the partition against the live query
   vector; the partition only narrows what gets scanned, never substitutes
   for the check (`lookup_never_returns_below_threshold_even_same_partition`
   is a regression test for exactly this).
2. **No embedding provider abstraction** flexible enough to run fully
   offline. `embeddings.rs` supports `openai` (real API), `ollama`
   (self-hosted, still an HTTP call Halo makes to an already-running server,
   never a model Halo loads itself), and `mock` (deterministic, zero-cost, for
   tests/offline dev) — Halo never links a model runtime.

**Cross-provider by design.** The partition key and stored answer deliberately
exclude provider/model; a question answered once via one provider can serve a
similar question later routed through another. The cached answer is always
*re-rendered* into the requesting endpoint's own shape (`answer.rs`), buffered
or as a synthetic SSE stream, never replayed as a raw stored HTTP body of a
possibly-different shape. Live-verified (mock providers, real HTTP round
trips through the shim): an OpenAI-origin answer served a same-wording
request, a differently-worded paraphrase, and a differently-worded *streamed*
request, all routed to/through Anthropic or back to OpenAI, with zero calls
reaching the "wrong" provider's mock endpoint in any case:

| request | similarity | origin -> serving | streamed |
|---|---|---|---|
| identical wording | 1.00 | openai gpt-4o -> anthropic claude-3.5-sonnet | no |
| paraphrase ("...capital city of Italy") | 0.94 | openai -> anthropic | no |
| same paraphrase | 0.94 | openai -> openai | **yes (SSE)** |

A near-miss case (extra clause + internal punctuation pushing the mock
embedder's crude bag-of-words similarity below threshold) correctly fell
through to a live call rather than false-hitting — the safety property working
as intended, not a bug. (Caveat: the `mock` provider is a dependency-free
hash, not semantically meaningful the way a real embedding model is —
fine for exercising plumbing, but real deployments should expect materially
higher, better-calibrated similarity scores for true paraphrases than the
mock's bag-of-words overlap gives you.)

**Also extended in this pass: exact-match caching now covers streamed
requests too** (`cache.rs` gained an `answer: Option<AnswerExtract>` field;
`answer.rs` is shared by both cache layers), closing the "known, accepted
limitation" called out earlier in this document.

### Two real bugs found via live smoke testing (not just unit tests)

1. **Mock/self-hosted embedding costs were billed at the price-table's
   embedding-fallback rate instead of the provider's own (correctly $0) cost.**
   `finalize_llm_call` computed cost purely from `PolicyDecision`, so a
   `mock`/`ollama` embedding lookup — genuinely free — got charged the
   fallback rate meant for an *unrecognized paid* embedding model. Fixed by
   making an explicit `actual_cost_override` win regardless of decision
   (`state.rs`), since the embedding client already knows its own true cost
   authoritatively.
2. **The relay's canonical cost recompute forced every cache-hit-flagged
   event to $0, including semantic hits that made a real (if small) embedding
   call.** `counterfactual::canonical` only looked at the `cache_hit` boolean;
   a `SemanticCacheHit` sets that flag too (it's still "served from a local
   cache" for dashboard purposes) but isn't actually free the way an
   exact-match hit is. Fixed by threading `policy_decision` and the shim's
   `reported_cost` through to the relay's SQLite schema and recompute logic:
   `CacheHit` stays canonically forced to $0 (recomputed, not trusted, per the
   relay's core "don't trust the client's math" property), `SemanticCacheHit`
   trusts the reported embedding cost (there's no token-count-based way to
   recompute an embedding-model charge from a *different* served model's
   tokens), and everything else is recomputed from tokens exactly as before.

Both were caught by an end-to-end smoke test with real HTTP round trips
against mock providers, not by unit tests in isolation — the unit tests for
each individual module (embedding client, price table, cache store) all
passed correctly in isolation; the bugs were in how the pieces were wired
together across the shim/relay boundary.

## Bottom line (v1.1)

Semantic caching is real, cross-provider, cosine-re-checked, and doesn't run
a model anywhere in the process. Both cost-accounting bugs found during
live testing are fixed and covered by regression tests
(`counterfactual::tests::semantic_cache_hit_trusts_reported_embedding_cost_not_zero`,
`state.rs`'s override-precedence logic). Remaining scope line for v1.2:
multi-turn semantic matching (see README).
