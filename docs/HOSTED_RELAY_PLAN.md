# Phase 4 (scoping) — Aperion-hosted multi-tenant relay-as-a-service

> Status: **not started — deliberately deferred.** This is a scoping note, not
> an implementation spec. Phases 0–3 (entitlements, self-hosted gating,
> per-subject attribution, packaging) ship the sellable self-hosted paid tier
> without any new service to operate. Path B — a multi-tenant relay Aperion
> runs — is materially a **new product surface**, not an extension of the
> current single-tenant `halo-relay` binary, and should get its own dedicated
> planning cycle once there is real self-hosted-tier usage data to size it
> against.

## Why it's separate from the existing `halo-relay`

The shipped `halo-relay` is intentionally single-tenant: one shared bearer
token (or a licensed set of them), one SQLite file, an unauthenticated
read-only dashboard. That's the right shape for a customer running their own
relay. It is the wrong shape for a service that holds many unrelated orgs'
data behind one endpoint. Retrofitting multi-tenancy onto it would compromise
the "one binary, one file, zero-ops" property that makes the self-hosted relay
easy to adopt.

## What Path B actually needs (the gap, not the design)

1. **Tenant isolation.** An `org_id` on every row and every query, derived from
   the authenticated principal — never from a client-supplied field (same
   lesson as `insert_batch` already trusting the envelope's `device_id`). The
   single shared bearer token is replaced by per-org, per-device credentials
   that map to exactly one `org_id`, with query-level enforcement (not just
   application-level filtering) so a bug can't cross tenants.
2. **Accounts & multi-seat login.** A real auth system for the hosted
   dashboard (SSO/OIDC, sessions, roles) rather than a bearer token pasted into
   a browser. This is where "multi-seat" becomes seats-with-identities, not
   just multiple tokens.
3. **Billing.** Stripe subscription + usage metering, reconciled against the
   metadata already ingested (requests, tokens, savings). Dunning, plan
   changes, and the free→paid upgrade path.
4. **Operational commitment.** Uptime target, backups, data-retention and
   deletion (GDPR/CCPA) policy, incident response, and a status page — the
   things a customer paying $50–100/mo for a hosted service is entitled to and
   that Aperion is then on the hook for.
5. **A hardened data plane.** Rate limiting, request-size caps, abuse
   detection, and per-tenant quotas at the ingest edge.

## Trust-model guardrails (must not regress in Path B)

- The relay still receives **metadata only** — the Phase 2 `subject` field and
  everything else in `docs/TELEMETRY_SCHEMA.md`, never content. Multi-tenancy
  does not loosen that invariant; it raises the stakes on it.
- The **free tier stays whole and local.** Nothing in a hosted offering may
  paywall a self-hoster's ability to see their bill, cap spend, or kill an
  agent. Path B sells convenience (hosted dashboard, seats, alerting at scale),
  never safety.
- License verification stays **offline** for the self-hosted path even after
  Path B exists; the hosted control plane is an *addition*, not a replacement
  for the embedded-public-key model.

## Entry criteria (when to actually plan this)

- Enough self-hosted paid-tier installs to justify the operational burden.
- Concrete demand for a hosted relay from users who don't want to run their own
  (measured, e.g., via install telemetry opt-ins or sales conversations).
- A decision on the account/billing stack (build vs. off-the-shelf).

Revisit with a full plan at that point. Until then, the self-hosted relay +
license key is the paid tier.
