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
  an Argon2id + XChaCha20-Poly1305 encrypted file on headless boxes. The OS
  keychain is unreachable without a GUI login session (e.g. over SSH/sudo on an
  always-on agent box) -- see [`docs/HEADLESS.md`](docs/HEADLESS.md) for the
  encrypted-file vault path, which is the normal setup for a self-hosted box.
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

## Install

The source repo is closed, but the artifacts are public — no GitHub auth
needed. See [`docs/INSTALL.md`](docs/INSTALL.md) for all channels (incl. an
OpenClaw `docker-compose` recipe).

On a multi-user or agent box where the proxy runs as a service user, install to
a shared location so that user can reach it: `HALO_INSTALL_DIR=/usr/local/bin`
(otherwise the one-liner falls back to your personal `~/.local/bin`, which the
service user can't see).

```bash
# One-liner (macOS/Linux, arm64/x64): downloads the right release binary.
curl -fsSL https://halo-get.aperion.ai | sh

# Shared install for a service-user box:
HALO_INSTALL_DIR=/usr/local/bin sudo -E sh -c 'curl -fsSL https://halo-get.aperion.ai | sh'

# Or Docker — public GHCR image, ships both `halo` and `halo-relay`:
docker run --rm -v halo-data:/data -p 8787:8787 ghcr.io/aperionai/halo
# ...or `docker compose up -d halo` with the bundled docker-compose.yml.

# Or build from source (licensees with repo access):
cargo build --release
```

Windows: grab the `.zip` from the [releases page](https://github.com/AperionAI/halo-dist/releases).

## Quick start

```bash
# 1. Register an agent. Mints a virtual key; stores the real key in your
#    keychain. Pass the real key on stdin so it never lands in shell history
#    (there's also a `--key sk-...` flag, but avoid it interactively):
halo agent add researcher --provider openai      # paste the key at the prompt

# 2. Point your runtime at Halo instead of the provider:
export OPENAI_API_KEY=sf_live_researcher_...        # the virtual key printed above
export OPENAI_BASE_URL=http://127.0.0.1:8787/v1

# 3. Run the proxy.
halo serve
```

On a headless / SSH-only box the OS keychain isn't reachable; Halo uses an
encrypted-file vault instead -- see [`docs/HEADLESS.md`](docs/HEADLESS.md). For
an always-on box, install it as a service (`sudo halo service install`) rather
than leaving `halo serve` in a terminal.

Anthropic works the same way (`--provider anthropic`, then set
`ANTHROPIC_API_KEY` to the virtual key and `ANTHROPIC_BASE_URL=http://127.0.0.1:8787`).

Running the OpenClaw Gateway? See [`docs/OPENCLAW_INTEGRATION.md`](docs/OPENCLAW_INTEGRATION.md)
for exactly where it reads those env vars from and the upgrade gotcha to watch.

Any OpenAI-compatible third party (Groq, Together, Fireworks, a local
vLLM/Ollama server) works too — add `--base-url` to `agent add`:

```bash
halo agent add fast-agent --provider openai \
  --key gsk_... --base-url https://api.groq.com/openai
```

Streaming (`"stream": true`) is passed through to the client as it arrives,
not buffered — see `docs/DESIGN_REVIEW.md` for why that matters.

### See spend & savings

```bash
halo status      # live spend by agent, current caps
halo report      # local COGS/savings view — works fully offline
```

### Local dashboard (free, on by default)

`halo serve` also brings up a second, loopback-only web UI at
`http://127.0.0.1:8788` — no relay, no account, nothing leaves the machine.
It's a convenience layer over the same free-tier data `halo status`/`halo
report` already show, plus the ability to revoke an agent or edit
`config.yaml` from a browser instead of the CLI:

- **Overview** — spend, savings, requests, cache-hit rate, by-model breakdown.
- **Agents** — list + one-click revoke (same effect as `halo kill`).
- **AI usage registry** — every agent/provider/endpoint plus lifetime spend
  and savings, downloadable as JSON or CSV (see "AI usage registry" below).
- **Settings** — budgets, cache toggles, semantic-cache threshold, relay URL.
  Writes go straight to `~/.halo/config.yaml`; like most of Halo's config,
  they take effect on the next `halo serve` restart, not live.

Reads need nothing beyond loopback access, matching every other free-tier
surface. Anything that *writes* (revoke, save settings) requires a local
bearer token that never leaves the machine:

```bash
halo dashboard token              # print it (generates one on first use)
halo dashboard token --regenerate # invalidate the old one, mint a new one
```

Disable it entirely with `dashboard.enabled: false` in `config.yaml`, or move
it off the default port with `dashboard.listen`.

### Budgets & kill switch

Set caps in `~/.halo/config.yaml` (see `config/halo.example.yaml`). The **hard
cap is enforced locally and always** — a single request can't overshoot it, and
it does not depend on the relay ever being reachable. To stop an agent instantly:

```bash
halo kill researcher   # revokes its key; proxy refuses it at once
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
halo embeddings set-key            # prompts for the embeddings API key
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

On the free tier `max_entries` is capped (a paid license with
`semantic_cache_unlimited` lifts it); the cache still works either way.

### The relay (optional)

```bash
HALO_RELAY_TOKEN=some-shared-token halo-relay --bind 127.0.0.1:8080
# open http://127.0.0.1:8080 for the savings dashboard
```

Then set `relay_url` + `relay_token` in `~/.halo/config.yaml`. Without a relay,
Halo is fully functional locally; only the hosted dashboard is unavailable.

The dashboard also has a **remote-kill** panel: revoke an agent fleet-wide or
per-device and every shim refuses it on its next poll (~30s). This is a
best-effort backstop only — each shim's local hard-cap kill switch and
`halo kill` work with zero network and are never gated.

### Per-channel / sub-agent attribution

When one runtime process (e.g. an OpenClaw Gateway) fans a single API key out
across many chat channels or sub-agents, set an `X-Halo-Subject` header per
outbound call (`<channel>:<thread-or-user-id>`). Halo records it as metadata
(never content) and rolls up spend/savings "by subject" in `halo report`. The
relay's hosted per-subject drill-down is a paid feature (see below).

### Egress allowlist / region-lock (off by default)

Every egress Halo itself initiates — the LLM provider, the embeddings API,
and the relay telemetry upload — can be constrained to a fixed set of hosts.
This is a hard, proxy-side control, not a log line: a request to a host not
on the list is denied *before it leaves the process*, with an audit event and
a clear error back to the agent. Even a fully automated, or prompt-injected,
agent cannot make Halo reach an endpoint you haven't approved.

```yaml
egress:
  allowed_upstreams:
    - "api.anthropic.com"
    - ".yourcompany.com"   # leading "." matches any subdomain (and the apex)
```

Empty/absent (the default) is unrestricted — nothing changes for an existing
install until you opt in. See `docs/DESIGN_REVIEW.md` for the fail-closed
semantics and why the embeddings path in particular fails closed (it's the
one place raw prompt text would otherwise reach a third party).

### At-rest encryption for cached content (off by default)

The two stores that hold response content on disk — `cache.redb` (exact-match
bodies) and `semantic_cache.redb` (semantic-cache answer text) — can be
encrypted at rest with the same Argon2id + XChaCha20-Poly1305 primitive Halo
already uses for the credential fallback:

```bash
export HALO_VAULT_PASSPHRASE="correct horse battery staple"
```

```yaml
encrypt_at_rest: true
```

If `encrypt_at_rest` is `true` and the passphrase is unset, `halo serve`
refuses to start rather than silently running in plaintext — the one config
problem in Halo that blocks startup, because it's the one case where the
operator explicitly asked for a guarantee. Embedding vectors, the semantic
cache's partition key, and every metadata store (ledger/audit/telemetry) stay
cleartext by design — none of them are reversible to the original prompt or
response text. A row written before encryption was enabled is still readable
after turning it on (and gets sealed the next time it's written); a hard
guarantee of "no plaintext content on disk, ever" requires starting with
encryption on or wiping `~/.halo/cache.redb` / `semantic_cache.redb` first.

### AI usage registry — governance/audit export

A metadata-only export of what's running: every agent, its provider and
endpoint, status, and lifetime request/spend/savings totals, plus the MCP
servers Halo fronts (name + command only — `env`, which can hold secrets, is
never included). This is the evidence pack a compliance reviewer or an AI
system inventory mandate (e.g. Austin's Resolution 55) asks for, built purely
from data Halo already tracks — no new instrumentation, no prompt/response
content.

```bash
halo registry export                          # JSON to stdout
halo registry export --format csv --out registry.csv
```

The same data is one click away in the local dashboard's **AI usage
registry** panel (`/api/registry`, `/api/registry.csv`) — read-only, no
token needed, works fully offline.

## Tiers & licensing

Halo follows an OSS-core model. **The free tier's safety net is unconditional
forever** — budgets + kill switch, exact-match cache, compression, prompt-cache
injection, MCP cloak/taint, local audit log, and `halo report` are all fully
functional, uncapped, and never gated. Nothing that keeps a self-hoster safe
from a runaway bill is ever paywalled.

Two things scale with a **paid license** (an offline, Ed25519-signed key — no
phone-home, verified against a public key baked into the binary): how big
Halo can get on *this one machine*, and hosted/multi-seat conveniences once
you're running more than one:

| Feature | Free | Paid |
|---|---|---|
| Local budgets, hard-cap kill switch, `halo kill` | ✅ | ✅ |
| Exact-match cache, compression, prompt-cache injection | ✅ | ✅ |
| MCP cloak/taint, local audit log, `halo report` | ✅ | ✅ |
| Egress allowlist / region-lock, at-rest encryption | ✅ | ✅ |
| AI usage registry export (JSON/CSV, `halo registry`) | ✅ | ✅ |
| Registered agents (`halo agent add`) | 3 | ✅ unlimited |
| Semantic cache | ✅ (capped entries) | ✅ (raised cap) |
| Budget alerting webhooks | — | ✅ |
| Best-effort remote kill (pull from relay) | — | ✅ |
| Relay multi-seat tokens | — | ✅ |
| Hosted per-subject cost drill-down | — | ✅ |

The 3-agent cap is the one non-fleet reason to upgrade — a solo power user
running a researcher, a coder, and a reviewer agent off one Halo install hits
it well before ever caring about remote-kill or multi-seat. Hitting it doesn't
break anything already running: `halo agent add` just refuses to mint a
*4th* virtual key until you revoke one or license up.

```bash
halo license show          # current tier, features, expiry
```

Set `license_key` (the token, or a path to a file holding it) in
`~/.halo/config.yaml`. A missing, invalid, or expired key silently resolves to
the free tier — it never blocks startup. The relay reads its own license from
`HALO_RELAY_LICENSE` to gate multi-seat tokens (`HALO_RELAY_TOKENS`) and the
per-subject drill-down.

## License

Proprietary — binaries and images only, no source distributed. See
[LICENSE](LICENSE) (Aperion AI Halo Binary License Agreement).
