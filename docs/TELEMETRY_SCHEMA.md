# Halo Telemetry Schema (v1)

This is the **complete** set of fields Halo's shim ever sends to the relay.
It is published verbatim, before launch, so the claim is independently
checkable by capturing the actual wire traffic between your shim and relay
(e.g. point `relay_url` at a debug endpoint or packet-capture the connection)
and confirming it never exceeds this schema.

## The one invariant

**Metadata only.** The relay never receives, and the shim never transmits:

- prompt text or response text,
- system prompts,
- tool names' *arguments* or tool results,
- file paths, URLs, or hostnames from your requests,
- embeddings or vectors of any kind.

Model traffic (your actual calls to Anthropic/OpenAI) never transits the relay.
Provider API keys never leave your machine — they live in your OS keychain.

## `TelemetryEvent`

One event is emitted per proxied request, asynchronously, after the response is
returned to your agent (never on the hot path).

| Field | Type | Meaning |
|---|---|---|
| `device_id` | string | Stable per-install id (random UUID at first run). Not derived from any content. |
| `agent_id` | string | The handle you chose (e.g. `researcher`). User-controlled. |
| `subject` | string (optional) | Sub-identity within one agent, set by the runtime via the `X-Halo-Subject` request header (e.g. `slack:general`, a sub-agent name, or a thread id). User-controlled routing label — MUST NOT carry content; trimmed and length-capped by the shim. Omitted entirely when unset. |
| `timestamp` | RFC 3339 | When the request completed. |
| `provider` | enum | `anthropic` \| `openai` \| `other`. |
| `model` | string | Model name as sent to the provider (e.g. `claude-3-5-sonnet`). |
| `tokens_in` | integer | Input tokens billed (provider-reported). |
| `tokens_out` | integer | Output tokens billed (provider-reported). |
| `tokens_cached` | integer | Provider-reported cached input tokens (Anthropic cache reads / OpenAI automatic cache). |
| `cache_hit` | boolean | True when Halo served the response from its local exact-match cache (provider never called). |
| `task_class` | string | Coarse class: `chat` \| `embedding`. Never content. |
| `latency_ms` | integer | End-to-end latency of the proxied call. |
| `estimated_cost` | number | Estimated USD actually paid to the provider. |
| `counterfactual_cost` | number | Estimated USD the request would have cost with no cache and no compression. |
| `policy_decision` | enum | `allow` \| `cache_hit` \| `semantic_cache_hit` \| `budget_blocked` \| `soft_cap_warn` \| `policy_blocked`. |
| `compression_ratio` | number | chars-after / chars-before over compressed text (`1.0` = unchanged). |
| `error_class` | string | Error class if the call failed (`timeout`, `transport`, `http_429`, …), else empty. |

## Batch envelope

```json
{ "device_id": "dev_…", "events": [ /* TelemetryEvent, … */ ] }
```

Uploaded to `POST <relay_url>/v1/telemetry` with a bearer token issued at
device registration. Failures spool to `~/.halo/spool/` and replay on
reconnect. The durable local log at `~/.halo/telemetry.jsonl` is the source of
truth for `halo report` and is never cleared by upload.

## Savings math (canonical, server-side)

The relay recomputes both costs from the token metadata using the same shared
price table as the shim (`halo-common::pricing`), so the headline savings figure
is canonical rather than simply whatever a shim reported:

- **actual** = `$0` on a cache hit, else input (partly cached) + output at list price.
- **counterfactual** = compressed `tokens_in` scaled back up by `compression_ratio`,
  cached tokens billed at the full input rate, plus output.
- **savings** = `max(0, counterfactual − actual)`.
