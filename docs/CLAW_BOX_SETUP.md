# Pointing a Claw box at Halo instead of the raw Anthropic key

For a single-operator OpenClaw (or similar self-hosted agent runtime) box
running off one Anthropic API key. Takes about 5 minutes. Everything here
runs locally on the claw box itself -- no account, no cloud dependency.

Companion file: [`claw-box.config.yaml`](./claw-box.config.yaml) -- copy it
in at step 3.

## Why bother

Pointing Claw straight at your Anthropic key means no budget ceiling, no
kill switch, and no visibility if something loops or misbehaves -- it just
bills straight against your account. Halo sits in between as a tiny local
proxy: Claw talks to Halo exactly like it would talk to Anthropic, Halo
enforces a hard spending cap and caches repeat/similar calls, then forwards
to the real key, which it holds in the OS keychain. Claw never sees the real
key.

## 1. Install Halo on the claw box

```bash
curl -fsSL https://halo-get.aperion.ai | sh
```

Or with Docker instead (see the main [README](../README.md#quick-start) for
both paths). Verify it landed:

```bash
halo --version
```

## 2. Register Claw as an agent

This is the one step that touches your real Anthropic key. It's stored once,
in your OS keychain -- never written to disk in plaintext, never sent
anywhere.

Pass the key on **stdin** so it never lands in your shell history:

```bash
halo agent add claw --provider anthropic
# ...then paste the key at the prompt (or pipe it: `pbpaste | halo agent add ...`)
```

(You can also set `HALO_PROVIDER_KEY` in the environment. There's a `--key
sk-ant-...` flag too, but avoid it interactively -- it records your live key in
shell history.)

> **Headless / SSH box?** The OS keychain needs a GUI login session, so on a
> remotely-administered box `agent add` will use an encrypted-file vault
> instead and ask you to set `$HALO_VAULT_PASSPHRASE`. That's expected -- see
> [`HEADLESS.md`](./HEADLESS.md), or skip straight to the service-install
> appendix at the bottom, which sets it all up for you.

Halo prints back a **virtual key** and a **base URL** -- copy both, you need
them in step 4:

```
ANTHROPIC_API_KEY=sf_live_claw_xxxxxxxxxxxxxxxx
ANTHROPIC_BASE_URL=http://127.0.0.1:8787
```

(Free tier caps this at 3 registered agents total on one box -- irrelevant
here since you're only registering one.)

## 3. Drop in the config

```bash
cp claw-box.config.yaml ~/.halo/config.yaml
```

Open it and sanity-check `budget.soft_cap_usd` / `hard_cap_usd` -- those are
the numbers that decide when Claw gets throttled (soft) or hard-stopped
(hard) over a rolling 24h window. Defaults are $25 soft / $50 hard; change
them to whatever you're comfortable Claw could spend unattended.

## 4. Start Halo

```bash
halo serve
```

For a quick test, run it in the foreground. For a real always-on box, **don't**
use `screen`/`tmux` -- install it as a proper service so it survives logout and
reboot. On macOS that's one command:

```bash
sudo halo service install --user <the user Claw runs as>
```

See the [service-install appendix](#appendix-run-halo-as-a-service) below for
what it sets up (and the manual/systemd equivalents).

## 5. Point Claw at Halo, not Anthropic

In Claw's own config/env, replace whatever currently holds your raw Anthropic
key and endpoint with the two values from step 2:

```bash
export ANTHROPIC_API_KEY=sf_live_claw_xxxxxxxxxxxxxxxx
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787
```

> **Running the OpenClaw Gateway?** These env vars **do not work for OpenClaw** --
> it ignores the process environment on service installs and reads its key and
> endpoint from its own config + auth store instead. You'll need a config patch
> in `openclaw.json` plus the virtual key written into the agent's
> `auth-profiles.json`. See [`OPENCLAW_INTEGRATION.md`](./OPENCLAW_INTEGRATION.md)
> for the exact recipe, the two gotchas, and how to verify with `lsof` that
> traffic is actually going through Halo.

Restart Claw. It should work exactly as before -- Halo is a transparent
passthrough for normal traffic; the only difference is what happens at the
edges (budget checks, caching, logging).

## 6. Verify it's actually in the loop

```bash
halo report          # spend + cache-savings so far, from the terminal
```

Or open the local dashboard in a browser (no login for read-only views):

```
http://127.0.0.1:8788
```

Send Claw one real request, then re-run `halo report` (or refresh the
dashboard) -- you should see the request show up with a cost attached. If it
does, Claw is correctly routed through Halo.

## 7. (Optional) find the token if you need to change settings from the dashboard

Viewing is open; changing settings or revoking the agent from the dashboard
needs a local token (never leaves the machine):

```bash
halo dashboard token
```

## What you get once this is wired up

- **Hard budget kill switch** -- Claw physically cannot exceed `hard_cap_usd`
  in the rolling window, even if it loops or misbehaves.
- **Caching** -- identical and near-identical repeat prompts get served from
  cache instead of re-billed.
- **Prompt-cache injection** -- large system prompts / tool defs / pasted
  context get Anthropic `cache_control` breakpoints automatically, so Claude
  discounts repeated context instead of re-billing it every turn.
- **A local audit trail** -- `halo report` / the dashboard, entirely on this
  machine, no data leaves it unless you later point it at a shared relay.

## If something looks wrong

- **Claw gets connection-refused:** `halo serve` isn't running, or
  `ANTHROPIC_BASE_URL` doesn't match `listen` in the config (default
  `127.0.0.1:8787`).
- **Requests succeed but `halo report` shows nothing:** Claw is probably
  still using the raw Anthropic key/URL somewhere (check for a second config
  location, e.g. a `.env` Claw itself reads that overrides what you set).
- **Hit the hard cap unexpectedly:** `halo report` shows exactly which agent
  and which requests ate the budget -- raise `hard_cap_usd` in the config and
  restart `halo serve` if it was just set too low for real usage.
- **`halo report` shows $0 after a request that worked:** you're almost
  certainly running `halo report` as a different user than `halo serve`, so it's
  reading a different (empty) `~/.halo`. The report now prints `Data dir:` at
  the top -- check it matches where the proxy writes. On a service install both
  are pinned to `/usr/local/var/halo`.

---

## Appendix: run Halo as a service

The always-on box this doc exists for is never sitting in an interactive shell,
so `screen`/`tmux` isn't good enough -- close the session and every agent
request gets connection-refused. Install Halo as a real service instead.

### macOS (one command)

```bash
sudo halo service install --user openclaw   # the user your agent runtime runs as
```

This generates everything a headless install needs and loads it:

- `/usr/local/libexec/halo/halo-serve.sh` -- wrapper that exports the vault
  passphrase (the keychain is unreachable without a GUI session) and pins a
  fixed data dir, then `exec`s `halo serve`.
- `/usr/local/etc/halo/vault-passphrase` -- a generated passphrase, mode `0600`,
  owned by the service user. **It prints the passphrase once -- back it up.**
- `/usr/local/var/halo` -- the fixed data directory (so `halo report` run by any
  user, or the service under launchd, always reads the same store).
- `/usr/local/var/log/halo/halo.log` + `halo.err.log` -- service logs.
- `/Library/LaunchDaemons/ai.aperion.halo.plist` -- the LaunchDaemon
  (`RunAtLoad` + `KeepAlive`, runs as `--user`).

Then register your agent as that same service user, with the same passphrase and
data dir, so the sealed key is the one the service can read:

```bash
sudo -u openclaw \
  env HALO_HOME=/usr/local/var/halo \
      HALO_VAULT_PASSPHRASE="$(sudo cat /usr/local/etc/halo/vault-passphrase)" \
  halo agent add claw --provider anthropic

sudo launchctl kickstart -k system/ai.aperion.halo   # restart to pick it up
```

Remove it with `sudo halo service uninstall` (leaves the data dir and
passphrase in place so nothing is silently destroyed).

If you'd rather not use the subcommand, the plist above is a plain LaunchDaemon
and the wrapper is a three-line shell script -- copy them by hand from the paths
listed and `sudo launchctl bootstrap system <plist>`.

### Linux (systemd template)

`halo service install` automates macOS only for now. On Linux, drop this unit at
`/etc/systemd/system/halo.service` (adjust `User`, paths, and the binary
location), then `systemctl daemon-reload && systemctl enable --now halo`:

```ini
[Unit]
Description=Smartflow Halo governance proxy
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=openclaw
Environment=HALO_HOME=/var/lib/halo
# Keep the passphrase in a root-readable file and load it here. EnvironmentFile
# lines are `KEY=VALUE`, so store it as `HALO_VAULT_PASSPHRASE=...`:
EnvironmentFile=/etc/halo/vault.env
ExecStart=/usr/local/bin/halo serve
Restart=always
RestartSec=2
# Least privilege: Halo only needs its own data dir.
ReadWritePaths=/var/lib/halo

[Install]
WantedBy=multi-user.target
```

Create the data dir and passphrase file first (`sudo mkdir -p /var/lib/halo &&
sudo chown openclaw /var/lib/halo`; see [`HEADLESS.md`](./HEADLESS.md) for the
passphrase), and register the agent as `openclaw` with the same `HALO_HOME` and
passphrase, exactly as in the macOS steps above.
