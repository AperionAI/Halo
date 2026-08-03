# Smartflow Halo

A lightweight, standalone governance proxy for **self-hosted AI agents**. Halo
sits between your agent runtime and the model providers (and your MCP servers),
so you can:

- **See the bill** — per-agent, per-model spend and a real COGS/savings number,
  split into what came from Halo's own cache vs. a baseline that holds even
  when that cache never hits (see "Compression & provider prompt-cache" below).
- **Cut the obvious waste** — exact-match response cache + prompt compression.
- **Never get a runaway invoice** — local token/spend budgets with a hard-cap
  kill switch that works even fully offline.
- **Keep secrets out of the model** — reversible cloaking + secret-shape
  detection at the MCP seam (reused from [Aperion Shield](https://github.com/AperionAI/shield)).
- **Prove it** — a tamper-evident, hash-chained local audit log and a
  metadata-only savings dashboard.

It is deliberately **light**: its own Rust workspace, a lean dependency tree,
`redb`/SQLite single-file stores (no Redis, no Mongo, no Postgres), and **no
model of any kind running inside the process** — the optional semantic cache
(below) gets its vectors from an external embeddings API call, never a local
model. Same posture as `shield-standalone` and `compass-standalone`.

## Non-negotiable trust model

- **Provider API keys never leave your machine.** They live in your OS keychain
  (macOS Keychain / Linux kernel keyutils / Windows Credential Manager), or in
  an Argon2id + XChaCha20-Poly1305 encrypted file on headless boxes.
- **The relay receives metadata only** — token counts, model, cost, cache-hit
  flag. Never prompts, responses, tool arguments, or vectors. Model traffic
  never transits the relay. The full schema is published in
  [`docs/TELEMETRY_SCHEMA.md`](docs/TELEMETRY_SCHEMA.md).

## Layout

```
crates/
  halo-common/   shared types: telemetry schema, pricing, virtual-key format
  halo-shim/     the `halo` binary (local proxy + CLI)
  halo-relay/    the `halo-relay` binary (metadata ingest + savings dashboard)
config/halo.example.yaml
docs/TELEMETRY_SCHEMA.md
```

## Quick start

```bash
cargo build --release

# 1. Register an agent. Mints a virtual key; stores the real key in your keychain.
./target/release/halo agent add researcher --provider openai --key sk-...

# 2. Point your runtime at Halo instead of the provider:
export OPENAI_API_KEY=sf_live_researcher_...        # the virtual key printed above
export OPENAI_BASE_URL=http://127.0.0.1:8787/v1

# 3. Run the proxy.
./target/release/halo serve
```

Anthropic works the same way (`--provider anthropic`, then set
`ANTHROPIC_API_KEY` to the virtual key and `ANTHROPIC_BASE_URL=http://127.0.0.1:8787`).

Any OpenAI-compatible third party (Groq, Together, Fireworks, a local
vLLM/Ollama server) works too — add `--base-url` to `agent add`:

```bash
./target/release/halo agent add fast-agent --provider openai \
  --key gsk_... --base-url https://api.groq.com/openai
```

Streaming (`"stream": true`) is passed through to the client as it arrives,
not buffered — see `docs/DESIGN_REVIEW.md` for why that matters.

### See spend & savings

```bash
./target/release/halo status      # live spend by agent, current caps
./target/release/halo report      # local COGS/savings view — works fully offline
```

### Budgets & kill switch

Set caps in `~/.halo/config.yaml` (see `config/halo.example.yaml`). The **hard
cap is enforced locally and always** — a single request can't overshoot it, and
it does not depend on the relay ever being reachable. To stop an agent instantly:

```bash
./target/release/halo kill researcher   # revokes its key; proxy refuses it at once
```

A single long-running stream can't be pre-charged for its eventual size, so
streaming requests also get a coarse mid-stream stop-loss on top of the same
pre-flight check (see `docs/DESIGN_REVIEW.md`). The built-in price table is a
small, hand-maintained approximation, not a continuously-updated feed —
override anything it gets wrong via `price_overrides` in `config.yaml`.

### MCP interception

List your MCP servers under `mcp_servers` in the config and point your runtime's
MCP config at `http://127.0.0.1:8787/mcp/<name>`. Halo runs each server
internally and, at the seam, reuses Shield's:

- **cloak** — reference secrets in tool args as `{{cloak:NAME}}` (register them
  with `aperion-shield`); Halo resolves them only on the copy sent upstream and
  scrubs any leaked secret values out of results before your agent sees them.
- **taint** — credential-shape detection flags raw secrets heading to a tool or
  echoed back by one, recorded (kinds only) in the audit log.

### Compression & provider prompt-cache (on by default)

Every request gets two savings mechanisms that apply whether or not Halo's
own cache ever hits — the floor a deployment gets even at a 0% cache-hit rate:

- **Compression that survives to the wire.** Verbose-phrase reduction (`"In
  order to"` -> `"To"`, ported from the main Smartflow proxy's phrase table)
  and whitespace collapsing (blank-line runs, trailing spaces — never
  leading/indentation, so pasted code/YAML/lists can't be corrupted) both
  genuinely shrink the outbound body. Aggressive single-word abbreviations
  (`"and"` -> `"&"`) exist but are opt-in (`compression.aggressive_abbreviations`)
  since they can change meaning.
- **Anthropic `cache_control` breakpoints**, injected on the system prompt,
  `tools` definitions, and the first message's attachment-shaped content
  (a pasted document, screenshot, or RAG context block ahead of the per-turn
  question) whenever a block is large (>=4000 chars) or smaller-but-repeated
  (>=2000 chars, seen 3+ times this process). This is the "flip the cache
  flag when we see repetitive data or attachments" behavior, extended beyond
  the main proxy's system-prompt-only version (confirmed via code review) to
  also cover tools and attachments — the two other places a large, stable
  block commonly recurs turn to turn. OpenAI caches its own prompt prefix
  automatically; Halo just parses `cached_tokens` off the response.

`halo report` and the relay dashboard split savings into this baseline
(compression + provider cache) vs. hit-driven savings (Halo's own exact/
semantic cache), so a low Halo-cache-hit-rate deployment doesn't look
misleadingly unimpressive — see `docs/DESIGN_REVIEW.md` for the accounting.

### Semantic cache (cross-provider, off by default)

Exact-match caching only catches byte-identical requests. The semantic cache
catches the much larger set of *reworded* repeats — a question asked twice in
different words, possibly even routed through different agents/providers —
without running any model locally:

```yaml
semantic_cache:
  enabled: true
  provider: openai        # "openai" | "ollama" (self-hosted) | "mock" (offline dev)
  model: text-embedding-3-small
  similarity_threshold: 0.85
  max_entries: 500
```

```bash
./target/release/halo embeddings set-key            # prompts for the embeddings API key
```

How it stays safe and cheap rather than a source of wrong or stale answers:

- **Cosine similarity is always re-checked** against the live query vector —
  a cheap keyword partition (conversational stage + intent, no provider/model)
  only narrows *which* candidates get compared; it never decides a hit by
  itself. Below `similarity_threshold`, it's a miss, full stop.
- **Cross-provider and cross-model by design.** A question answered once via
  Anthropic can serve a similar question later routed to OpenAI — the cached
  answer is re-rendered into the requesting endpoint's own JSON (or SSE
  stream) shape, never replayed as a raw stored HTTP body.
- **Tool calls, structured output (`response_format` beyond plain text), and
  multi-turn history are excluded entirely** — a similar-but-not-identical
  prompt in any of those modes may legitimately need a different tool call or
  may not conform to a schema; replaying free text in its place would be
  unsafe. These fall straight through to a live call, same as today.
- **The embedding lookup's own (small) cost is billed and shown separately**
  in `halo report` and `halo-relay`'s aggregation (`SemanticCacheHit` is its
  own telemetry decision, distinct from the free exact-match `CacheHit`) —
  never silently folded into "$0, it was cached."
- **Off by default.** Unlike exact-match caching, this makes a real API call
  (unless `provider: mock`/`ollama`) on every miss, so it's opt-in.

### The relay (optional)

```bash
HALO_RELAY_TOKEN=some-shared-token ./target/release/halo-relay --bind 127.0.0.1:8080
# open http://127.0.0.1:8080 for the savings dashboard
```

Then set `relay_url` + `relay_token` in `~/.halo/config.yaml`. Without a relay,
Halo is fully functional locally; only the hosted dashboard is unavailable.

## What's intentionally NOT in v1.1

- **Multi-turn semantic matching.** The semantic cache only considers a
  fresh, single-user-turn request (see `eligible_query` in
  `semantic_cache.rs`) — embedding and safely comparing a full conversation
  history is a materially harder correctness problem, deferred rather than
  shipped half-safe.
- Encrypted audit escrow (v1 is tamper-evident via the HMAC chain, not
  confidential), model routing tiers, multi-seat, alerting/webhooks, remote
  kill, licensing on the relay, and per-model allowlists/RBAC.
- Windows keychain persistence tuning.

These are v1.2+ items; v1.1 ships only what's needed to be genuinely useful
and cheap to run. (Token-level SSE streaming passthrough, exact-match caching
of streamed requests, and the cross-provider semantic cache are all
implemented — see `docs/DESIGN_REVIEW.md`.)

## License

Apache-2.0.
