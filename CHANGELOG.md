# Changelog

## 1.6.6

Route effort routing is on by default. Name the cheap agent `glm` and
set `efficient_model` if that agent's model id isn't the inbound one.
Streamed calls skip the hop. `halo report` rolls up hops, stream skips,
and quality escalations.

### Changed

- **Route effort** (`routing.effort`) defaults to `mode: auto` when the
  block is missing. Free and Cut still never hop.
- Scorer stays in lockstep with Smartflow LCR (same `score` /
  `infer_*` / `should_escalate_quality` bodies).

### Added

- `efficient_model` on the effort block so GLM hops rewrite the wire
  model (e.g. `glm-4.7`) instead of leaving the inbound model name.
- Effort rollup on `halo report`.

## 1.6.5

Hermes gets the same one-command patch OpenClaw already had. Route can send
a task class to a cheaper agent (the GLM lane).

### Added

- **`halo hermes apply --agent <id>`.** Writes `~/.hermes/config.yaml`
  (`providers.halo` + `model.provider`) and upserts the virtual key in
  `.env`. `--dry-run` prints and writes nothing.
- **Route task-class routing.** `routing.by_class` maps `chat` /
  `embedding` / `code` (or `X-Halo-Task-Class`) to another agent whose
  provider key is used on the wire. `routing.models` optionally rewrites
  the model (e.g. `code: glm-4.7`). Spend still hits the inbound agent's
  cap. Failover still applies on the wire agent.

## 1.6.0

Cut now cuts the bill. Cache, compression, and prompt-cache injection are
paid. Free still meters them and stars what you would have saved. Route
retries once on a dead upstream. MCP taint blocks before the tool runs.

No production users yet, so the gate is on now rather than later.

### Changed

- **Free** writes exact-match cache (so repeats can be scored) but does not
  serve hits, does not rewrite the outbound body, and does not inject
  Anthropic `cache_control`. Each event records `shadow_savings_usd`.
- **Dashboard / `halo report` / `halo license show`.** Free shows
  "You spent $X. Cut would have saved $Y*" instead of pretending cache
  already ran.
- **Per-agent caps and semantic cache serve** are Cut. Global $25/$50
  caps, kill switch, and denylist stay Free.

### Added

- **Route failover.** `failover:` in `config.yaml` maps agent id to a
  backup agent. On transport error or 502/503/504/529, one retry with the
  backup provider/key. No recursive hops.
- **MCP taint block** (default on). Uncloaked secret shapes in tool
  arguments refuse the call before it is forwarded. `mcp_block_uncloaked_secrets: false` to log-only.
- **`telemetry.jsonl` prune** at `halo serve` start to the tier history
  window. `audit.jsonl` is not touched.

## 1.5.1

Production signing key. Paid licenses now verify in stock Halo with no
`HALO_LICENSE_PUBKEY` override. The private seed lives only on the license
box; this release just bakes in the matching pubkey. Govern is still refused.

## 1.5.0

Stripe checkout is live for Cut (Route uses the same path). After you pay,
`halo license apply` writes the key — no YAML hunting.

- **Checkout pages** match docs.aperion.ai (dark nav, same tokens). Buy is a
  branded landing, then Stripe; thanks shows the token plus `halo license apply`.
- **`halo license apply [TOKEN]`** writes `license.key` (0600) and points
  `license_key` at it. Token from argv, a file path, or stdin.
- **Renewals.** `invoice.paid` with `subscription_cycle` mints a fresh window.
  First payment still comes from Checkout so we don't double-issue.
- Optional Postmark (`POSTMARK_SERVER_TOKEN` + `POSTMARK_FROM`) emails the
  token. Thanks page works without it.

Govern is still refused.

## 1.4.1

Operator nits from the 1.4.0 smoke. Same Free firewall as 1.4.0.

- **`halo status` while `halo serve` is running.** The spend ledger is exclusive; status used to die with `Database already open`. It now reads spend from the local log instead.
- **Installer PATH hint.** Detects zsh/bash/fish, writes the matching rc file (`~/.zshrc` on this Mac), and prints `export PATH=...` for the current shell so the next `halo` works.
- **`halo agent add` prints the data directory** so a second terminal does not mint the key into `~/.halo` while the proxy is on `$HALO_HOME`.

## 1.4.0

R3 starts here: the Free / Cut / Route / Govern ladder is a real license
shape, and Free history is 7 days so Cut has something to sell. Cache and
compression stay on Free — the savings number has to be true on a $0
install. Stripe checkout is the next R3 slice; today you still paste
`license_key` into `~/.halo/config.yaml`.

### Added

- **Ladder.** `cut` / `route` / `govern` feature flags. Legacy paid
  `pro`/`team` keys count as Cut. `halo license issue --tier cut` (default)
  fills the Cut feature set when you don't pass `--feature`.
- **History caps.** Free 7 days, Cut 30, Route/Govern 90. `halo report` and
  the dashboard clamp to the cap (`--hours 0` / "all time" is no longer
  unbounded).
- **Upgrade CTA** on the loopback dashboard for Free: would-have cost and
  Halo saved, from this install's own traffic, plus Cut at $50/mo.
- **`halo report --format json [--out file]`.** Same rollup as the text
  report, pipeable. Window is still the tier cap (7 / 30 / 90 days).

## 1.3.5

R2 hardening of the public-free install path. Same Free scope as 1.3.4;
OpenClaw is no longer a three-file hand patch. Halo LICENSE is still the
binary agreement — publishing the shim as open source is a separate legal
gate (entity structure, Craig/Frank), not this drop.

### Added

- **`halo openclaw apply --agent <id>`.** Writes the field-verified
  OpenClaw config + auth-store patches (baseUrl, virtual key, nested
  `allowPrivateNetwork`), backs up the previous files, `--dry-run` to
  preview. Env vars still do not work for OpenClaw; this is the command
  that does.
- **Shield README pointer** to Halo, worded so Shield's terms are
  unchanged.

## 1.3.4

First-run firewall: a fresh `halo` write now arms spend caps and a starter
egress denylist so an install refuses a runaway and blocks cloud-metadata
exfil without anyone editing YAML first. `halo report` and the loopback
dashboard also roll up by task and by hour. Relay stays optional; with
`relay_url` unset nothing is uploaded.

### Added

- **Starter egress denylist.** Cloud metadata hosts (`169.254.169.254`,
  `metadata.google.internal`, …) and a short list of paste/exfil sinks are
  always denied. Extra hosts go in `egress.denied_upstreams`. Deny wins over
  the allowlist. Empty extras do not open metadata. `api.openai.com` /
  `api.anthropic.com` cannot be blocked by the denylist.
- **Armed default caps on first config write.** `$25` soft / `$50` hard per
  24h, matching `docs/claw-box.config.yaml`. Existing files that already set
  `budget.window_hours` without caps are left uncapped. `halo status` and the
  dashboard show remaining budget and how to raise it.
- **Spend by task and by hour** in `halo report` and `/api/summary`.

### Changed

- README / INSTALL / CLAW_BOX lead with local-only: nothing leaves the
  machine when `relay_url` is unset. The hosted relay is not a Free
  requirement.

## 1.3.3

A field-verified operator runbook came in from an early adopter after 1.3.2
shipped. It confirmed the 1.3.2 correction was pointed the right direction but
got some exact values wrong (reconstructed from a description, not a verified
copy), and it surfaced one real gap in the service installer itself.

### Fixed

- **`OPENCLAW_INTEGRATION.md` now has the exact, field-verified values,**
  replacing the reconstructed ones from 1.3.2: `request.allowPrivateNetwork` is
  nested under `models.providers.anthropic`, not top-level; the auth-profile
  path/shape is `~/.openclaw/agents/<agent-id>/agent/auth-profiles.json` with
  `profiles["anthropic:default"]["key"]`; the config patch is applied via
  `openclaw config patch --stdin` + `openclaw config validate`; and the `lsof`
  verification command is `lsof -nP -i -a -p <pid> -r2 | grep -E '8787|:443'`.
- **`halo service install`'s generated LaunchDaemon now sets `ThrottleInterval`
  to 10 seconds.** Without it, a misconfigured wrapper or binary that exits
  immediately respawns in a tight loop, pinning a CPU and flooding the error
  log. Field-verified as deliberate, not a launchd default to rely on.
  `CLAW_BOX_SETUP.md`'s manual plist and systemd (`RestartSec`) examples
  updated to match.

### Added

- **`CLAW_BOX_SETUP.md`: operations appendix** -- rotating the provider key,
  rotating the vault passphrase, confirming reboot survival, and a FileVault
  gotcha (a cold boot with FileVault + no auto-login stops at the pre-boot
  unlock screen; use `sudo fdesetup authrestart` for planned remote reboots and
  test the power-loss case deliberately).
- **`CLAW_BOX_SETUP.md`: honest caching/savings expectations** -- exact-match
  cache hit rates are low on real agent traffic, compression saves little on
  structured JSON payloads (~0.005% measured on an 18k-token request), and
  provider prompt-cache injection (not Halo's own cache) is where the real
  savings come from for this workload. The budget cap and audit trail deliver
  value regardless.
- **`h()` shell helper** in troubleshooting docs -- wraps `halo` with the
  service user/HOME/passphrase so `halo report` can't silently read the wrong,
  empty data store by accident.
- Launchd troubleshooting: exit code `78` (wrapper path wrong) and
  immediately-exits (passphrase file unreadable by the service user), plus a
  reminder to use a LaunchDaemon, never a LaunchAgent, for a service account.

## 1.3.2 (docs)

Docs-only correction from a second field report. No binary change.

### Fixed

- **OpenClaw integration was documented wrong and is now corrected.** The
  previous guidance said to set `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL` in the
  gateway's environment. **That does not work on OpenClaw service installs** --
  `env.shellEnv.enabled` is off by default, so the gateway ignores the process
  environment and reads its key/endpoint from its own config + auth store,
  sending traffic straight to the provider while looking correctly configured
  (no cap, no metering, no error). `docs/OPENCLAW_INTEGRATION.md` now documents
  the method that was proven in the field: a provider override in `openclaw.json`
  (with the required `models` array and `request.allowPrivateNetwork: true` for
  the SSRF guard), Halo's virtual key written into the agent's
  `auth-profiles.json` (which takes precedence over `models.providers.*.apiKey`),
  and a gateway restart. README and `CLAW_BOX_SETUP.md` updated to match.
- **Verification now requires two checks.** A rising request count in
  `halo report` is necessary but not sufficient (a partial bypass can coexist
  with it). Docs now call for `lsof` on the running gateway during real traffic
  to confirm a `127.0.0.1:8787` connection and no direct `:443` to the provider.
- **`docs/UPGRADING.md`** now covers moving an existing `~/.halo` store into a
  `halo service install` (it does not migrate automatically; the new service
  passphrase means the sealed key must be re-registered) and confirms the
  0.2.x → 1.3.x on-disk format is unchanged (the version jump was a
  release-numbering change, not a data-format break).

## 1.3.1

(1.3.0 was published earlier from a pre-release commit; this is the first build
carrying the changes below.)

Focused on the headless / always-on box story after the beta 1 field report:
the product worked, but the documented install path assumed an interactive
desktop session the target user is never in. This release closes that gap and
fixes the smaller "misleading or silently wrong" papercuts alongside it.

### Added

- **`halo service install` / `halo service uninstall` (macOS)** -- generates the
  wrapper, vault passphrase file, fixed data directory, log files, and a
  LaunchDaemon, then loads it, so Halo survives logout and reboot without
  hand-building any of it. Linux ships as a documented systemd template in
  `docs/CLAW_BOX_SETUP.md`.
- **`docs/HEADLESS.md`** -- why the OS keychain is unreachable without a GUI
  session, and how to use the encrypted-file vault (`$HALO_VAULT_PASSPHRASE`):
  generating one, storing it, and that it's needed on every `halo serve` start.
- **`docs/OPENCLAW_INTEGRATION.md`** -- worked OpenClaw example. (The method
  described in this release was later found to be wrong for service installs and
  corrected in 1.3.2 -- see above.)
- **Per-request log line.** `halo serve` now prints one terse line per request
  (agent, model, tokens, cost, cache hit/miss) at the default log level, so the
  foreground proxy no longer looks dead while traffic flows. Quiet with
  `RUST_LOG=warn`.

### Changed

- **Headless credential error is now explicit.** When the keychain is
  unreachable and `$HALO_VAULT_PASSPHRASE` is unset, the error names the no-GUI
  cause and points at `docs/HEADLESS.md` instead of a generic message.
- **`agent add` reports the real storage backend.** It no longer claims "stored
  in your OS keychain" when the key actually went to the encrypted-file vault.
- **`halo report` / `halo status` print the data directory** and, when the store
  is missing or empty, say so instead of showing a confident `$0.0000` (the
  wrong-user-reads-empty-store trap).
- **Docs lead with stdin for provider keys.** `--key` is demoted to a note with
  a shell-history warning.
- **Version alignment.** The binary version now matches the public `halo-vX.Y.Z`
  release tags and the one-pager (was an internal `0.2.x` that disagreed with the
  `v1.x` docs; `halo --version` now reports `1.3.1`).
- **Sample config no longer trips the free-tier warning.** `claw-box.config.yaml`
  ships `semantic_cache.max_entries: 200`, and the ceiling warning only fires
  when the semantic cache is actually enabled.
- **Installer guidance for multi-user boxes.** `HALO_INSTALL_DIR` is now
  documented in the README/`docs/INSTALL.md`, and the installer prints a hint
  when it falls back to a per-user `~/.local/bin`.

### Verification

Re-verify P0 items end-to-end on the same headless macOS / OpenClaw box that
produced the field report (headless keychain path, `halo service install`,
OpenClaw env injection).
