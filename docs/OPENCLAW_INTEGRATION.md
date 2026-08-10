# Pointing OpenClaw at Halo

A worked example for the **OpenClaw Gateway** specifically. For a generic agent
runtime, setting `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL` in the runtime's
environment is enough. **For the OpenClaw Gateway it is not** -- read the warning
below before you do anything else.

## ⚠️ The env-var method does NOT work on OpenClaw

Do **not** integrate OpenClaw by setting `ANTHROPIC_API_KEY` and
`ANTHROPIC_BASE_URL` in the gateway's environment. It looks like it works and
does nothing.

OpenClaw has a config key, `env.shellEnv.enabled`, that is **off by default on
service installs** (its own schema describes it as disabling env import for
locked-down service environments). With it off, the embedded agent runtime
ignores the process environment entirely and resolves both the key and the
endpoint from its **own config and auth store**.

This was proven in the field: both variables set correctly and confirmed on the
running process with `ps eww`, yet traffic still went straight to Anthropic. An
`lsof` during an agent run showed the gateway connected directly to the provider
on `:443` and **never** to Halo on `127.0.0.1:8787`.

The dangerous part is the false sense of safety. If you follow the old
env-var instructions, you'll see both lines present, conclude you're capped and
metered, and run completely **unmetered** with no budget ceiling and no
visibility -- and nothing errors to tell you.

## What actually works (three parts, all required)

Register the agent in Halo first (see [`CLAW_BOX_SETUP.md`](./CLAW_BOX_SETUP.md)
step 2 for the stdin-safe way, and [`HEADLESS.md`](./HEADLESS.md) for a box with
no GUI session) so you have Halo's virtual key (`sf_live_claw_...`) and listen
address (`http://127.0.0.1:8787`). Then do all three of the following.

> The JSON below mirrors the structure that worked in the field. Values and the
> exact profile shape vary by OpenClaw version -- lift the canonical, verbatim
> JSON from **§6 of the field runbook** and match your file's existing shape.

### 1. Override the Anthropic provider in `openclaw.json`

Patch OpenClaw's own config so the provider points at Halo instead of Anthropic.
Two gotchas that will silently break this if you miss them:

- The `models` array is **required** (each entry needs `id` and `name`) -- a
  bare `baseUrl` override without it won't take.
- `request.allowPrivateNetwork` must be `true`, or OpenClaw's SSRF guard blocks
  the `127.0.0.1` base URL before the request leaves the gateway.

```jsonc
{
  "models": {
    "providers": {
      "anthropic": {
        "baseUrl": "http://127.0.0.1:8787",     // Halo's listen address
        "models": [
          { "id": "claude-sonnet-4", "name": "claude-sonnet-4" }
          // ...list the model ids your agents actually request
        ]
      }
    }
  },
  "request": {
    "allowPrivateNetwork": true                  // else the SSRF guard blocks 127.0.0.1
  }
}
```

### 2. Write Halo's virtual key into the auth store

OpenClaw keeps its Anthropic credential in:

```
agents/<id>/agent/auth-profiles.json
```

The **auth store takes precedence over `models.providers.*.apiKey`.** So put
Halo's virtual key into that profile (in place of the real provider key). Skip
this and OpenClaw routes to Halo correctly but presents the *real* provider key,
which Halo rejects -- and the run fails.

```jsonc
// agents/<id>/agent/auth-profiles.json -- set the anthropic profile's key to
// Halo's virtual key. Match your file's existing shape; see runbook §6.
{
  "anthropic": {
    "apiKey": "sf_live_claw_xxxxxxxxxxxxxxxx"
  }
}
```

> **Not via the CLI.** OpenClaw's `models auth paste-token` only accepts
> `sk-ant-oat01-` OAuth tokens. It rejects standard `sk-ant-api03-` Console keys
> and will likewise reject Halo's `sf_live_...` key. Write the file directly --
> the CLI is the wrong door.

### 3. Restart the gateway

Restart so it re-reads both files. If Halo is installed as a service, make sure
Halo is up first, then bounce the gateway.

## Verifying: two checks, not one

A rising request count in `halo report` (or the dashboard) is **necessary but
not sufficient** -- a partial bypass can coexist with a rising count, so "the
number went up" does not prove all traffic is going through Halo.

The definitive check is `lsof` on the running gateway process during real agent
traffic. Kick off an agent request, then, while it's working:

```bash
# find the gateway pid, then watch its established TCP connections
sudo lsof -nP -iTCP -sTCP:ESTABLISHED | grep -Ei 'node|openclaw'
```

- **Through Halo (good):** a connection to `127.0.0.1:8787`.

  ```
  node  4982 openclaw  32u  TCP 127.0.0.1:xxxxx->127.0.0.1:8787 (ESTABLISHED)
  ```

- **Bypassing Halo (bad):** a direct connection to the provider on `:443` (e.g.
  `...->160.79.104.10:443`) and **no** `127.0.0.1:8787` at any point. That's the
  env-var-only failure mode above.

Run both checks. Only the `lsof` result proves you're actually in the loop.

## After any OpenClaw upgrade or reconfigure

An OpenClaw upgrade, reinstall, or reconfigure can rewrite `openclaw.json` and/or
the agent's auth profile and quietly revert the override -- sending traffic back
to Anthropic with no cap and no visibility, and no error to tell you. After
**any** OpenClaw change:

1. Re-apply all three parts above (provider override, auth-profile key, restart).
2. Re-run the **two-check** verification -- especially the `lsof` check.

Put this on your OpenClaw upgrade checklist.

---

*The config-patch recipe, the SSRF/`allowPrivateNetwork` and `models`-array
gotchas, the auth-store precedence, and the two-check `lsof` verification all
come from Craig's field install chronology and runbook (§6 config, §7
verification). Lift exact JSON from there.*
