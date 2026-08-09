# Running Halo on a headless box (no GUI login session)

Halo stores your real provider keys in the OS keychain by default. On a
headless or remotely-administered box that keychain is usually unreachable, so
Halo falls back to an encrypted file whose passphrase you supply. This page is
the missing manual for that path -- which is the normal path for the always-on
agent box Halo exists to protect.

## Why the keychain fails here

macOS scopes Keychain access to the **GUI login session**. A process reached
over SSH -- and especially one reached via `sudo -u <serviceuser>` -- is
outside that session, so the keychain refuses it regardless of whether the
target user has a desktop session open. You'll see this from the shell:

```
$ security show-keychain-info
User interaction is not allowed.
```

When `halo agent add` hits that, it tries the encrypted-file vault instead. If
`$HALO_VAULT_PASSPHRASE` isn't set, it stops with a clear error pointing here
rather than writing your key anywhere in the clear.

Linux behaves similarly for a service user: the kernel keyutils/Secret Service
backend often has no session keyring under a systemd service, so the same
encrypted-file fallback applies.

## The fix: set a vault passphrase

The encrypted-file vault seals each provider key with Argon2id + XChaCha20-
Poly1305, keyed off `$HALO_VAULT_PASSPHRASE`. Two things to know:

1. The passphrase is needed at **`agent add`** time (to seal the key) **and on
   every `halo serve` start** (to unseal it). It is not a one-time step.
2. The key that gets sealed is tied to the passphrase in effect when you ran
   `agent add`. If `halo serve` later runs with a different passphrase (or a
   different user reading a different `~/.halo`), it can't decrypt it. Use the
   same passphrase and the same data directory for both.

### Generate one

```bash
openssl rand -base64 32
```

### Store it where the service can read it

For a hand-rolled setup, put it in a root-owned (or service-user-owned) file
with tight permissions:

```bash
sudo mkdir -p /usr/local/etc/halo
umask 077
openssl rand -base64 32 | sudo tee /usr/local/etc/halo/vault-passphrase >/dev/null
sudo chmod 600 /usr/local/etc/halo/vault-passphrase
sudo chown <serviceuser> /usr/local/etc/halo/vault-passphrase
```

Then export it before `agent add` and before `serve`:

```bash
export HALO_VAULT_PASSPHRASE="$(sudo cat /usr/local/etc/halo/vault-passphrase)"
```

## Let Halo do it for you

On macOS, `halo service install` does all of the above -- it generates the
passphrase file, a wrapper that sources it, a fixed data directory, and a
LaunchDaemon so Halo survives reboot. See the service-install appendix in
[`CLAW_BOX_SETUP.md`](./CLAW_BOX_SETUP.md). That is the recommended path for a
real deployment; the manual steps here are for understanding what it does (and
for Linux, where you wire the same env var into your systemd unit).

## Register the key as the right user

If the proxy runs as a service user (e.g. `openclaw`), register the agent **as
that user**, with the passphrase and data directory the service will use, so
the sealed key is the one the service can read:

```bash
sudo -u openclaw \
  env HALO_HOME=/usr/local/var/halo \
      HALO_VAULT_PASSPHRASE="$(sudo cat /usr/local/etc/halo/vault-passphrase)" \
  halo agent add claw --provider anthropic
```

Registering as one user and serving as another is the most common headless
mistake -- see also the data-directory note in `halo report`/`halo status`
output, which now prints exactly which directory it read.
