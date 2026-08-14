# Halo license fulfillment on deploy.langsmart.app

Tiny service on **:3012**, nginx path `/halo/`. It is **not** the Smartflow
droplet orchestrator (`/webhook/stripe` on :3000). Point Stripe here only.

## URLs

- Health: `https://deploy.langsmart.app/halo/health`
- Buy Cut: `https://deploy.langsmart.app/halo/buy/cut` (branded landing, then Stripe)
- Buy Route: `https://deploy.langsmart.app/halo/buy/route`
- Thanks (Payment Link success): `https://deploy.langsmart.app/halo/thanks?session_id={CHECKOUT_SESSION_ID}`
- Webhook: `https://deploy.langsmart.app/halo/stripe/webhook`

Add `?go=1` to a buy URL to skip the landing and 302 to Stripe.

## Secrets (on the box, never git)

`/opt/halo-license/.env` mode 0600:

- `STRIPE_SECRET_KEY` — restricted `sk_test_...` (then `sk_live_...`)
- `STRIPE_WEBHOOK_SECRET` — `whsec_...` after the webhook is created in Stripe
- `HALO_LICENSE_SIGNING_KEY=file:/opt/halo-license/signing.seed`
- Optional: `POSTMARK_SERVER_TOKEN` + `POSTMARK_FROM` to email the token

Signing seed is a 32-byte file with an explicit `hex:` prefix. The Halo
binary's embedded pubkey (`lQbv4rTnKo-hn1b1sv6nlM04QF2dh4jwacpSW59SwkY`) must
match it. Staging seed is backed up as `signing.seed.staging.bak` on the
license box. 1.5.1+ verifies production keys with no `HALO_LICENSE_PUBKEY`.

After checkout the customer runs `halo license apply` (Halo 1.5+).

## Payment Link success URL

```
https://deploy.langsmart.app/halo/thanks?session_id={CHECKOUT_SESSION_ID}
```
