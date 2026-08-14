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
binary's embedded pubkey (`jvACr3xrphfIeojOqDud3ezYU0kMMCTllMVzopRijNY`) must
match it. Staging keys need `HALO_LICENSE_PUBKEY` on the client. For
production keys that verify out of the box, put the production seed at
`/opt/halo-license/signing.seed` (mode 0600) and restart `halo-license`.

After checkout the customer runs `halo license apply` (Halo 1.5+).

## Payment Link success URL

```
https://deploy.langsmart.app/halo/thanks?session_id={CHECKOUT_SESSION_ID}
```
