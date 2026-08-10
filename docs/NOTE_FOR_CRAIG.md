# Note for Craig — v1.3.3

Hey Craig,

v1.3.3 is up. No rush given Economist/Acrisure/IHG -- this is just so it's
captured and doesn't get lost.

## Grab it when you're ready

```bash
HALO_VERSION=halo-v1.3.3 HALO_INSTALL_DIR=/usr/local/bin sudo -E sh -c 'curl -fsSL https://halo-get.aperion.ai | sh'
```

Confirm with `halo --version` -- should say `1.3.3`. Release page with the
binaries and notes:
[halo-v1.3.3](https://github.com/AperionAI/Halo/releases/tag/halo-v1.3.3).

## Where to actually read, in order

1. [`CHANGELOG.md`](../CHANGELOG.md) -> the `1.3.3` section -- what changed and
   why, short version.
2. [`OPENCLAW_INTEGRATION.md`](./OPENCLAW_INTEGRATION.md) -- your recipe, now
   word for word: the `allowPrivateNetwork` nesting, the real
   `auth-profiles.json` shape, the `openclaw config patch --stdin` command, and
   your exact `lsof -r2` line. My first pass at this (1.3.2) had the JSON
   structure wrong -- this is corrected against your runbook, not my guess.
3. [`UPGRADING.md`](./UPGRADING.md) -- real answers to the two questions you
   asked: moving your existing `~/.halo` store into a service install (short
   version: it doesn't auto-migrate, you copy the files and re-register the key
   under the new passphrase), and confirmation that 0.2.x -> 1.3.x is the same
   on-disk format, not a break.
4. [`CLAW_BOX_SETUP.md`](./CLAW_BOX_SETUP.md) -- added a
   rotate-key / rotate-passphrase / reboot-survival / FileVault section, plus
   the honest caching-savings-expectations writeup, both straight from your
   runbook.

## One thing that isn't just docs

`halo service install` was missing `ThrottleInterval` on the generated
LaunchDaemon, which you'd flagged as deliberate. Fixed in the installer itself,
so you don't have to hand-patch it in.

Whenever you've got a slow afternoon, would still love your eyes on it again --
but genuinely, no clock on this.

-- Scott
