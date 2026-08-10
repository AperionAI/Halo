# Changelog

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

Re-verify P0 items end-to-end on the same Mac mini M1 / OpenClaw box that
produced the field report (headless keychain path, `halo service install`,
OpenClaw env injection).
