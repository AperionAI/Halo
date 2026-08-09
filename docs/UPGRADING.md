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
HALO_VERSION=halo-v1.3.1 HALO_INSTALL_DIR=/usr/local/bin \
  sudo -E sh -c 'curl -fsSL https://halo-get.aperion.ai | sh'
```

Confirm it took:

```bash
/usr/local/bin/halo --version
```

### If you run the Docker image

```bash
docker pull ghcr.io/aperionai/halo:latest      # or pin :1.3.1
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

## 5. OpenClaw only: re-check the env after any OpenClaw change

OpenClaw regenerates `service-env/ai.openclaw.gateway.env`, and an OpenClaw
upgrade (or reinstall) can silently drop the two Halo lines, sending traffic
straight back to Anthropic with no cap and no visibility. After **any** OpenClaw
change, confirm those lines are still present:

```
ANTHROPIC_API_KEY=sf_live_...        # your Halo virtual key
ANTHROPIC_BASE_URL=http://127.0.0.1:8787
```

Re-add them and restart the gateway if they're gone. Full detail:
[`OPENCLAW_INTEGRATION.md`](./OPENCLAW_INTEGRATION.md).

## Rolling back

Pin the previous version and reinstall, then restart the service:

```bash
HALO_VERSION=halo-v1.3.0 HALO_INSTALL_DIR=/usr/local/bin \
  sudo -E sh -c 'curl -fsSL https://halo-get.aperion.ai | sh'
sudo launchctl kickstart -k system/ai.aperion.halo
```

Data is forward-compatible within a major version, so a rollback keeps your
ledger, cache, and audit log intact.
