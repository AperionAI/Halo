# Upgrading Halo

How to pull a new Halo version on an always-on / headless box. Takes a couple
of minutes. Nothing here touches your provider keys, your budgets, or your
cached data -- those live in the data directory and survive upgrades.

## 1. Check what you're on (and what's out)

```bash
halo --version                 # what you're running now
```

Latest published version and notes:
<https://github.com/AperionAI/halo-dist/releases> (or the CHANGELOG shipped in
the tarball).

## 2. Upgrade the binary

### If you installed via the one-liner (macOS/Linux)

Re-running the installer pulls the latest release and replaces the binary in
place. **On a service box, install to the shared path the service user reads**
(otherwise it lands in your personal `~/.local/bin` and the service keeps
running the old one):

```bash
HALO_INSTALL_DIR=/usr/local/bin sudo -E sh -c 'curl -fsSL https://halo-get.aperion.ai | sh'
```

Pin a specific version instead of latest:

```bash
HALO_VERSION=halo-v1.6.5 HALO_INSTALL_DIR=/usr/local/bin \
  sudo -E sh -c 'curl -fsSL https://halo-get.aperion.ai | sh'
```

Confirm it took:

```bash
/usr/local/bin/halo --version
```

### If you run the Docker image

```bash
docker pull ghcr.io/aperionai/halo:latest      # or pin :1.6.5
docker compose up -d halo                       # recreate the container
```

## 3. Restart the service so it runs the new binary

The binary is only reloaded when the process restarts.

- **launchd (macOS, `halo service install`):**

  ```bash
  sudo launchctl kickstart -k system/ai.aperion.halo
  ```

- **systemd (Linux):**

  ```bash
  sudo systemctl restart halo
  ```

- **Foreground / screen / tmux:** stop it (Ctrl-C) and run `halo serve` again.
  (For an always-on box, switch to a real service -- see
  [`CLAW_BOX_SETUP.md`](./CLAW_BOX_SETUP.md).)

Your vault passphrase is still required on every start; the service wrapper
handles that automatically, so there's nothing extra to do here.

## 4. Verify it's actually in the loop (and read the right store)

```bash
halo report
```

The report now prints `Data dir:` at the top. Make sure it's the directory the
service writes to (`/usr/local/var/halo` for a service install). If you run
`halo report` as a different user than the service and see `$0.0000`, you're
reading a different, empty store -- not a metering failure.

Send one real request through your agent, then re-run `halo report`; the request
should show up with a cost.

## 5. OpenClaw only: re-verify the integration after any OpenClaw change

> **Do NOT rely on `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL` for OpenClaw.**
> The OpenClaw Gateway ignores the process environment on service installs
> (`env.shellEnv.enabled` is off by default), so setting those env vars does
> nothing and leaves you running unmetered while looking protected. The
> integration is a config patch in `openclaw.json` plus the virtual key in the
> agent's `auth-profiles.json` -- see [`OPENCLAW_INTEGRATION.md`](./OPENCLAW_INTEGRATION.md).

An OpenClaw upgrade or reconfigure can rewrite `openclaw.json` and/or the auth
profile and quietly revert the override. After **any** OpenClaw change, re-apply
the integration and re-verify with **both** checks:

1. `halo report` shows the request count rising (necessary, not sufficient), and
2. `lsof` on the running gateway shows a connection to `127.0.0.1:8787` and
   **no** direct `:443` to the provider during real agent traffic.

Only the `lsof` check proves you're actually in the loop. Full detail, including
the exact `lsof` command:
[`OPENCLAW_INTEGRATION.md`](./OPENCLAW_INTEGRATION.md).

## Moving an existing data dir into a service install

`halo service install` pins the store to `/usr/local/var/halo` and **does not
migrate** anything. If you've been running a hand-built LaunchDaemon (or plain
`halo serve`) whose store lives in the service user's `~/.halo`, switching to the
service install starts from an **empty** store unless you copy the old one across
first.

Do it while nothing is running:

```bash
sudo launchctl bootout system/ai.aperion.halo 2>/dev/null   # stop the service if already installed
sudo mkdir -p /usr/local/var/halo
# copy the ledger, caches, audit log, state (stop the process first so the
# redb files are quiescent):
sudo cp -a ~/.halo/. /usr/local/var/halo/
sudo chown -R <service-user> /usr/local/var/halo
sudo launchctl kickstart -k system/ai.aperion.halo
halo report   # confirm the ledger total carried over
```

One thing that does **not** carry over cleanly: the sealed provider key.
`halo service install` generates a **new** vault passphrase, so an encrypted-file
key (`cred-fallback.json`) sealed under your old passphrase won't decrypt. After
copying the store, **re-register the agent as the service user** with the new
passphrase (the `sudo -u <user> env HALO_HOME=... HALO_VAULT_PASSPHRASE=... halo
agent add ...` line the installer prints). The ledger, caches, and audit chain
are just files and copy fine; only the secret needs re-sealing.

## Version note: 0.x → 1.x carries your data

The jump from `0.2.x` to `1.3.x` is a **version-scheme change to align with the
public release number, not a data-format break.** The on-disk layout is the same
across it -- `ledger.redb`, `cache.redb`, `semantic_cache.redb`, `audit.jsonl`,
`state.json`, `vkeys.json` -- so your ledger, cache, and audit log carry forward.

Caveats:

- The append-only stores (`audit.jsonl`, `state.json`, `vkeys.json`) carry over
  unconditionally.
- The `*.redb` databases carry over as long as the embedded-DB file format
  matches across your specific old and new builds. If it ever doesn't, redb
  **refuses to open** the file with a clear error rather than corrupting it -- so
  the worst case is starting the ledger/cache fresh (both are regenerable; the
  cache rebuilds itself and the ledger is history, not config). Copy `*.redb`
  only while the process is **stopped**; they're live databases, not logs.
- **Back the whole dir up before you upgrade** (`cp -a` it somewhere) so a
  rollback is trivial regardless.

## Rolling back

Pin the previous version and reinstall, then restart the service:

```bash
HALO_VERSION=halo-v1.3.0 HALO_INSTALL_DIR=/usr/local/bin \
  sudo -E sh -c 'curl -fsSL https://halo-get.aperion.ai | sh'
sudo launchctl kickstart -k system/ai.aperion.halo
```

The 1.x on-disk layout is unchanged from 0.2.x (see the version note above), so a
rollback keeps your ledger, cache, and audit log intact. If you took the backup
above, you can also just restore it.
