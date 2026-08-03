# Phase 5 — Windows keychain verification checklist

> Status: **partially covered by CI, manual verification still required.**
> The `windows` job in `.github/workflows/ci.yml` compiles the `keyring`
> Windows Credential Manager backend and runs the non-interactive test suite on
> a `windows-latest` runner. That catches build/logic regressions, but it does
> **not** exercise real interactive Credential Manager persistence — CI runs in
> a headless/service context, which can behave differently from a logged-in
> desktop session. Do not claim "Windows parity" in marketing until the manual
> checks below pass on a real Windows host.

## Background

Provider secrets are stored via the `keyring` crate. On Windows that's the
`windows-native` backend (Credential Manager). Halo's `keys.rs` falls back to
an Argon2id + XChaCha20-Poly1305 encrypted file (`$HALO_VAULT_PASSPHRASE`) when
no keychain is available — so a keychain failure degrades gracefully rather
than breaking, but we still want the native path to actually work on Windows.

## Manual checks (real Windows 10/11 desktop)

1. **Store + retrieve.**
   - `halo agent add researcher --provider openai --key sk-test-123`
   - Confirm the secret is NOT in `%USERPROFILE%\.halo\` in plaintext.
   - Open `Control Panel > Credential Manager > Windows Credentials` and
     confirm a `smartflow-halo` generic credential exists.
   - `halo serve`, send a request through the proxy, confirm it forwards with
     the real key (i.e. retrieval from Credential Manager works).
2. **Revoke.**
   - `halo agent revoke researcher` (or `halo kill researcher`).
   - Confirm the `smartflow-halo` credential is removed from Credential Manager.
3. **Persistence across reboot.**
   - Add an agent, reboot, `halo serve`, confirm retrieval still works (no
     re-prompt, secret survived).
4. **Service / headless context** (the risky one).
   - Run `halo serve` as a Windows Service or scheduled task under a
     non-interactive account. Confirm store/retrieve still works, OR that it
     cleanly falls back to the encrypted-file vault when `$HALO_VAULT_PASSPHRASE`
     is set. Document whichever behavior occurs.
5. **Install script parity.**
   - The `curl | sh` installer is Unix-only by design; confirm the Windows
     `.zip` release asset extracts and `halo.exe --version` runs.

## Exit criteria

All of 1–4 pass (or 4 documents a clean, expected fallback), and 5 confirms the
release asset is usable, on at least one real Windows 10 and one Windows 11
host. Only then update any "runs on Windows" marketing claim.
