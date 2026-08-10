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
no GUI session) so you have Halo's virtual key (`sf_live_<agent>_...`) and listen
address (`http://127.0.0.1:8787`). Then do all three of the following.
**Back up `openclaw.json` and the auth-profiles file before touching either.**

### 1. Override the Anthropic provider in `openclaw.json`

Patch OpenClaw's own config so the provider points at Halo instead of Anthropic.
Override the **built-in `anthropic` provider ID** rather than creating a new
one, so existing model references keep working.

```bash
openclaw config patch --stdin <<'EOF'
{
  "models": {
    "providers": {
      "anthropic": {
        "baseUrl": "http://127.0.0.1:8787",
        "apiKey": "sf_live_<agent>_xxxxxxxxxxxx",
        "request": { "allowPrivateNetwork": true },
        "models": [ { "id": "claude-sonnet-4-5", "name": "Claude Sonnet 4.5" } ]
      }
    }
  }
}
EOF
openclaw config validate
```

Two gotchas inside that block that will silently break this if you miss them:

- `models` is a **required array**, and `id`/`name` are required within it --
  setting only `baseUrl` fails validation.
- `request.allowPrivateNetwork: true` (nested under the `anthropic` provider,
  not top-level) is mandatory, or OpenClaw's SSRF guard blocks the `127.0.0.1`
  destination before the request leaves the gateway.

### 2. Write Halo's virtual key into the auth store

OpenClaw keeps its Anthropic credential in:

```
~/.openclaw/agents/<agent-id>/agent/auth-profiles.json
```

The **auth store takes precedence over `models.providers.*.apiKey`.** So the
`apiKey` you just patched into `openclaw.json` above is not enough by itself --
put Halo's virtual key into the auth profile too. Skip this and OpenClaw routes
to Halo correctly but presents the *real* provider key, which Halo rejects with
`unrecognized or revoked Halo virtual key`.

```bash
python3 -c '
import json
p = "/Users/<service-user>/.openclaw/agents/<agent-id>/agent/auth-profiles.json"
d = json.load(open(p))
d["profiles"]["anthropic:default"]["key"] = "sf_live_<agent>_xxxxxxxxxxxx"
json.dump(d, open(p, "w"), indent=2)
'
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
traffic. Find the gateway's PID, kick off an agent request, then watch its
connections while it's working:

```bash
sudo lsof -nP -i -a -p <runtime-pid> -r2 2>/dev/null | grep -E '8787|:443'
```

(`-r2` repeats the listing every 2 seconds so you can watch it live across the
request.)

- **Through Halo (good):** `->127.0.0.1:8787`.
- **Bypassing Halo (bad):** `->x.x.x.x:443` during a model call, with no
  `127.0.0.1:8787` at any point. That's the env-var-only failure mode above --
  and it's what a partial/incorrect config patch also looks like.

Run both checks. A rising request count without the `lsof` check is not proof --
a partial configuration can route some paths through Halo and bypass others.

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
gotchas, the auth-store precedence, and the two-check `lsof` verification are
lifted verbatim from a field-verified operator runbook contributed by an early
adopter -- verified end-to-end on macOS (Apple Silicon) against OpenClaw. The
Linux equivalent is structurally the same but untested in the field; treat it
as a starting point.*
