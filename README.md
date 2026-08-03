# Smartflow Halo

A lightweight, standalone governance proxy for **self-hosted AI agents**. Halo
sits between your agent runtime and the model providers (and your MCP servers),
so you can:

- **See the bill** — per-agent, per-model spend and a real COGS/savings number.
- **Cut the obvious waste** — exact-match response cache + prompt compression.
- **Never get a runaway invoice** — local token/spend budgets with a hard-cap
  kill switch that works even fully offline.
- **Keep secrets out of the model** — reversible cloaking + secret-shape
  detection at the MCP seam (reused from [Aperion Shield](https://github.com/AperionAI/shield)).
- **Prove it** — a tamper-evident, hash-chained local audit log and a
  metadata-only savings dashboard.

It is deliberately **light**: its own Rust workspace, a lean dependency tree,
`redb`/SQLite single-file stores (no Redis, no Mongo, no Postgres), and **no
embedding/semantic-cache path** in v1. Same posture as `shield-standalone` and
`compass-standalone`.

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

### The relay (optional)

```bash
HALO_RELAY_TOKEN=some-shared-token ./target/release/halo-relay --bind 127.0.0.1:8080
# open http://127.0.0.1:8080 for the savings dashboard
```

Then set `relay_url` + `relay_token` in `~/.halo/config.yaml`. Without a relay,
Halo is fully functional locally; only the hosted dashboard is unavailable.

## What's intentionally NOT in v1

- Semantic / embedding (L2) cache and any embedding provider — the heaviest,
  least production-ready part of the main proxy. Exact-match captures the
  within-user repetition that dominates real savings.
- Encrypted audit escrow (v1 is tamper-evident via the HMAC chain, not
  confidential), model routing tiers, multi-seat, alerting/webhooks, remote
  kill, licensing on the relay, and per-model allowlists/RBAC.
- Caching of streamed responses (exact-match cache only applies to
  non-streamed requests) and Windows keychain persistence tuning.

These are v1.1+ items; v1 ships only what's needed to be genuinely useful and
cheap to run. (Token-level SSE streaming passthrough itself *is* in v1 -- see
`docs/DESIGN_REVIEW.md` for why buffering full completions would have been a
regression, not a reasonable v1 cut.)

## License

Apache-2.0.
