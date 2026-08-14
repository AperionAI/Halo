#!/usr/bin/env python3
"""Halo Stripe fulfillment -- mint an offline Ed25519 license after Checkout.

Separate from the Smartflow droplet orchestrator on :3000. Stripe must point
the Halo webhook at /halo/stripe/webhook, never /webhook/stripe.

Secrets live in /opt/halo-license/.env (0600). This process never logs keys.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

import mapping

try:
    import stripe
except ImportError:
    stripe = None


def env_path() -> Path:
    return Path(os.environ.get("HALO_LICENSE_ENV", "/opt/halo-license/.env"))


def load_env_file(path: Path) -> None:
    if not path.is_file():
        return
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, _, v = line.partition("=")
        k, v = k.strip(), v.strip().strip('"').strip("'")
        os.environ.setdefault(k, v)


load_env_file(env_path())

PORT = int(os.environ.get("PORT", "3012"))
ISSUED_DIR = Path(os.environ.get("ISSUED_DIR", "/opt/halo-license/issued"))
HALO_BIN = os.environ.get("HALO_BIN", "/opt/halo-license/bin/halo")
SIGNING_KEY = os.environ.get("HALO_LICENSE_SIGNING_KEY", "file:/opt/halo-license/signing.seed")
LICENSE_DAYS = os.environ.get("LICENSE_DAYS", "35")


def stripe_ready() -> bool:
    key = os.environ.get("STRIPE_SECRET_KEY", "").strip()
    return bool(stripe) and key.startswith("sk_")


def webhook_ready() -> bool:
    return os.environ.get("STRIPE_WEBHOOK_SECRET", "").strip().startswith("whsec_")


def buy_url(tier: str) -> str:
    return os.environ.get(f"STRIPE_PAYMENT_LINK_{tier.upper()}", "").strip()


def signing_ready() -> bool:
    spec = SIGNING_KEY
    if spec.startswith("file:"):
        return Path(spec[5:]).is_file()
    if spec.startswith("env:"):
        return bool(os.environ.get(spec[4:], "").strip())
    return bool(spec.strip())


def halo_ready() -> bool:
    p = Path(HALO_BIN)
    return p.is_file() and os.access(p, os.X_OK)


def issued_path(session_id: str) -> Path:
    safe = "".join(c for c in session_id if c.isalnum() or c in "-_")
    return ISSUED_DIR / f"{safe}.json"


def mint_license(org: str, tier: str) -> str:
    if not halo_ready():
        raise RuntimeError(f"halo binary missing: {HALO_BIN}")
    if not signing_ready():
        raise RuntimeError("signing seed not configured")
    proc = subprocess.run(
        [
            HALO_BIN,
            "license",
            "issue",
            "--org",
            org,
            "--tier",
            tier,
            "--days",
            str(LICENSE_DAYS),
            "--signing-key",
            SIGNING_KEY,
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    key = proc.stdout.strip().splitlines()[-1].strip()
    if not key:
        raise RuntimeError(f"halo license issue produced no key: {proc.stderr}")
    return key


def fulfill_session(session: dict) -> dict:
    session_id = session["id"]
    existing = issued_path(session_id)
    if existing.is_file():
        return json.loads(existing.read_text())
    paid = session.get("payment_status") in ("paid", "no_payment_required")
    if session.get("status") == "complete" and session.get("payment_status") is None:
        paid = True
    if not paid and session.get("status") != "complete":
        raise RuntimeError("checkout session is not paid")
    price_id = mapping.first_price_id_from_session(session)
    tier = mapping.tier_for_price_id(price_id)
    if tier is None:
        raise RuntimeError(f"unknown Stripe price {price_id!r} (not a Halo SKU)")
    if tier == "govern":
        raise RuntimeError("Govern is not for sale yet")
    org = (
        (session.get("customer_details") or {}).get("email")
        or session.get("customer_email")
        or "halo-customer"
    )
    license_key = mint_license(org, tier)
    rec = {
        "session_id": session_id,
        "org": org,
        "tier": tier,
        "price_id": price_id,
        "license_key": license_key,
    }
    ISSUED_DIR.mkdir(parents=True, exist_ok=True)
    tmp = existing.with_suffix(".tmp")
    tmp.write_text(json.dumps(rec))
    tmp.chmod(0o600)
    tmp.replace(existing)
    existing.chmod(0o600)
    maybe_email_license(org, tier, license_key)
    return rec


def retrieve_session(session_id: str) -> dict:
    if not stripe_ready():
        raise RuntimeError("STRIPE_SECRET_KEY is not set")
    stripe.api_key = os.environ["STRIPE_SECRET_KEY"]
    sess = stripe.checkout.Session.retrieve(session_id, expand=["line_items"])
    return sess.to_dict() if hasattr(sess, "to_dict") else dict(sess)


def retrieve_invoice(invoice_id: str) -> dict:
    if not stripe_ready():
        raise RuntimeError("STRIPE_SECRET_KEY is not set")
    stripe.api_key = os.environ["STRIPE_SECRET_KEY"]
    inv = stripe.Invoice.retrieve(invoice_id, expand=["lines.data.price"])
    return inv.to_dict() if hasattr(inv, "to_dict") else dict(inv)


def fulfill_invoice(invoice: dict) -> dict | None:
    """Mint a fresh window on subscription renewals.

    First payment is `checkout.session.completed`. `invoice.paid` with
    `billing_reason=subscription_create` is the same charge — skip it so we
    don't mint twice. Only `subscription_cycle` (renewal) gets a new key.
    """
    reason = invoice.get("billing_reason") or ""
    if reason != "subscription_cycle":
        return None
    invoice_id = invoice.get("id") or ""
    if not invoice_id:
        raise RuntimeError("invoice has no id")
    existing = issued_path(invoice_id)
    if existing.is_file():
        return json.loads(existing.read_text())
    paid = invoice.get("status") in ("paid", "open")
    if invoice.get("paid") is True:
        paid = True
    if not paid:
        raise RuntimeError("invoice is not paid")
    price_id = mapping.first_price_id_from_invoice(invoice)
    tier = mapping.tier_for_price_id(price_id)
    if tier is None:
        raise RuntimeError(f"unknown Stripe price {price_id!r} (not a Halo SKU)")
    if tier == "govern":
        raise RuntimeError("Govern is not for sale yet")
    org = (
        invoice.get("customer_email")
        or (invoice.get("customer_details") or {}).get("email")
        or "halo-customer"
    )
    license_key = mint_license(org, tier)
    rec = {
        "invoice_id": invoice_id,
        "org": org,
        "tier": tier,
        "price_id": price_id,
        "license_key": license_key,
        "billing_reason": reason,
    }
    ISSUED_DIR.mkdir(parents=True, exist_ok=True)
    tmp = existing.with_suffix(".tmp")
    tmp.write_text(json.dumps(rec))
    tmp.chmod(0o600)
    tmp.replace(existing)
    existing.chmod(0o600)
    maybe_email_license(org, tier, license_key)
    return rec


def maybe_email_license(org: str, tier: str, key: str) -> None:
    token = os.environ.get("POSTMARK_SERVER_TOKEN", "").strip()
    from_addr = os.environ.get("POSTMARK_FROM", "").strip()
    if not token or not from_addr or "@" not in org:
        return
    import urllib.request

    body = {
        "From": from_addr,
        "To": org,
        "Subject": f"Your Halo {tier.title()} license",
        "TextBody": (
            f"Your Halo {tier} license is ready.\n\n"
            "If Halo is not installed yet:\n"
            "  curl -fsSL https://halo-get.aperion.ai | sh\n\n"
            "Then save the key (paste when prompted):\n"
            "  halo license apply\n\n"
            f"Token:\n{key}\n\n"
            f"One line:\nhalo license apply '{key}'\n\n"
            "Then: halo license show\n"
            "Restart halo serve if it is already running.\n"
        ),
        "MessageStream": os.environ.get("POSTMARK_STREAM", "outbound"),
    }
    req = urllib.request.Request(
        "https://api.postmarkapp.com/email",
        data=json.dumps(body).encode(),
        headers={
            "Accept": "application/json",
            "Content-Type": "application/json",
            "X-Postmark-Server-Token": token,
        },
        method="POST",
    )
    try:
        urllib.request.urlopen(req, timeout=10)
    except Exception:
        traceback.print_exc()


def email_ready() -> bool:
    return bool(
        os.environ.get("POSTMARK_SERVER_TOKEN", "").strip()
        and os.environ.get("POSTMARK_FROM", "").strip()
    )


# Docs-site tokens from docs.aperion.ai. No `$` in this CSS — thanks pages
# used to blow up because str.format treated `font:` as a placeholder.
PAGE_CSS = """
*, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
:root {
  --bg: #0f172a;
  --surface: #1e293b;
  --border: #334155;
  --ink: #f1f5f9;
  --ink2: #cbd5e1;
  --ink3: #94a3b8;
  --muted: #64748b;
  --purple: #7c3aed;
  --purple2: #6d28d9;
  --violet: #a78bfa;
  --green: #10b981;
}
html { background: var(--bg); }
body {
  font-family: 'Segoe UI', system-ui, -apple-system, sans-serif;
  background: var(--bg);
  color: var(--ink2);
  min-height: 100vh;
  font-size: 15px;
  line-height: 1.6;
}
a { color: var(--violet); text-decoration: none; }
a:hover { color: var(--ink); }
.topnav {
  position: sticky; top: 0; z-index: 100;
  background: rgba(15,23,42,0.92);
  backdrop-filter: blur(12px);
  border-bottom: 1px solid var(--border);
  padding: 0 32px; height: 58px;
  display: flex; align-items: center; gap: 24px;
}
.nav-brand { font-weight: 800; font-size: 1.05em; color: var(--ink); }
.nav-brand span { font-weight: 400; color: var(--muted); font-size: 0.8em; margin-left: 6px; }
.nav-links { display: flex; gap: 4px; flex: 1; }
.nav-links a {
  padding: 5px 12px; border-radius: 6px; font-size: 0.875em; color: var(--ink3);
}
.nav-links a:hover { color: var(--ink); background: var(--surface); }
.badge-version {
  background: var(--purple2); color: #e9d5ff;
  font-size: 0.72em; font-weight: 700; padding: 3px 9px; border-radius: 12px;
}
main { max-width: 42rem; margin: 48px auto; padding: 0 24px 64px; }
.eyebrow {
  color: var(--violet); font-size: 0.75em; font-weight: 700;
  letter-spacing: 0.08em; text-transform: uppercase; margin-bottom: 8px;
}
h1 { color: var(--ink); font-size: 1.85em; font-weight: 750; line-height: 1.2; margin-bottom: 12px; }
p { margin: 0 0 14px; }
.card {
  background: var(--surface); border: 1px solid var(--border);
  border-radius: 14px; padding: 22px; margin: 22px 0;
}
pre, textarea, code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
pre {
  background: #0a1020; border: 1px solid var(--border); border-radius: 8px;
  padding: 12px 14px; overflow-x: auto; color: var(--ink); font-size: 0.85em;
  margin: 8px 0 16px; white-space: pre-wrap; word-break: break-all;
}
textarea {
  width: 100%; min-height: 7.5rem; padding: 12px 14px;
  background: #0a1020; color: var(--ink); border: 1px solid var(--border);
  border-radius: 8px; font-size: 0.82em;
}
.muted { color: var(--muted); font-size: 0.92em; }
.row { display: flex; gap: 10px; flex-wrap: wrap; align-items: center; margin: 12px 0 4px; }
.btn {
  display: inline-block; background: var(--purple); color: #fff;
  font-weight: 700; font-size: 0.9em; padding: 10px 18px; border-radius: 8px;
  border: 0; cursor: pointer;
}
.btn:hover { background: var(--purple2); color: #fff; }
.btn.ghost { background: transparent; color: var(--ink2); border: 1px solid var(--border); }
.btn.ghost:hover { border-color: var(--violet); color: var(--ink); }
ol { margin: 0 0 16px 1.2rem; }
ol li { margin: 0 0 10px; }
"""


def render_page(title: str, body: str) -> str:
    return (
        "<!DOCTYPE html>\n<html lang=\"en\"><head>\n"
        "<meta charset=\"utf-8\"/>\n"
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"/>\n"
        f"<title>{html_escape(title)}</title>\n"
        f"<style>{PAGE_CSS}</style>\n"
        "</head><body>\n"
        "<header class=\"topnav\">"
        "<a class=\"nav-brand\" href=\"https://aperion.ai\">Aperion"
        "<span>Halo</span></a>"
        "<nav class=\"nav-links\">"
        "<a href=\"https://docs.aperion.ai\">Docs</a>"
        "<a href=\"https://halo-get.aperion.ai\">Install</a>"
        "<a href=\"https://deploy.langsmart.app/halo/buy/cut\">Buy Cut</a>"
        "<a href=\"https://deploy.langsmart.app/halo/buy/route\">Buy Route</a>"
        "</nav>"
        "<span class=\"badge-version\">1.5</span>"
        "</header>\n"
        f"<main>{body}</main>\n"
        "</body></html>\n"
    )


BUY_COPY = {
    "cut": {
        "price": "$50/mo",
        "blurb": "30-day history, alerting webhooks, and more than three agents. The local firewall (caps, kill switch, denylist) stays on Free.",
    },
    "route": {
        "price": "$100/mo",
        "blurb": "90-day history plus routing. Same checkout as Cut; Stripe handles the card.",
    },
}


def buy_page(tier: str, url: str) -> str:
    copy = BUY_COPY.get(tier, {})
    price = copy.get("price", "")
    blurb = copy.get("blurb", "")
    if url:
        cta = (
            f'<p><a class="btn" href="{html_escape(url)}">Continue to Stripe</a></p>'
            '<p class="muted">Opens Stripe Checkout. After you pay, you land back here with a license token and a one-line install command.</p>'
        )
    else:
        cta = (
            '<p class="muted">Checkout for this SKU is not wired yet. Cut is live.</p>'
            '<p><a class="btn" href="/halo/buy/cut">Buy Cut — $50/mo</a></p>'
        )
    body = (
        f'<p class="eyebrow">Halo {html_escape(tier.title())}</p>'
        f"<h1>{html_escape(price)} · local, offline-verified</h1>"
        f"<p>{html_escape(blurb)}</p>"
        '<div class="card">'
        "<p>Already have Halo? After Stripe you run:</p>"
        "<pre>halo license apply</pre>"
        "<p class=\"muted\">Need the binary first?</p>"
        "<pre>curl -fsSL https://halo-get.aperion.ai | sh</pre>"
        f"{cta}"
        "</div>"
    )
    return render_page(f"Halo {tier.title()}", body)


def thanks_page(tier: str, org: str, key: str, days: str) -> str:
    body = (
        '<p class="eyebrow">Checkout complete</p>'
        f"<h1>Halo {html_escape(tier.title())} is ready</h1>"
        f"<p>Issued for <strong>{html_escape(org)}</strong>. About {html_escape(str(days))} days. "
        "Halo verifies the key offline; this page does not phone home from your machine.</p>"
        '<div class="card">'
        "<ol>"
        "<li>Install Halo if you do not have it yet:"
        "<pre>curl -fsSL https://halo-get.aperion.ai | sh</pre></li>"
        "<li>Save the license. Copy the token, then:"
        "<pre>halo license apply</pre>"
        "or the one-liner:"
        f"<pre>halo license apply '{html_escape(key)}'</pre></li>"
        "<li>Confirm: <code>halo license show</code>. Restart <code>halo serve</code> if it is already running.</li>"
        "</ol>"
        f'<textarea id="key" readonly>{html_escape(key)}</textarea>'
        '<div class="row">'
        '<button class="btn" type="button" onclick="copyKey()">Copy token</button>'
        '<a class="btn ghost" href="https://docs.aperion.ai">Docs</a>'
        "</div>"
        "</div>"
        "<script>"
        "function copyKey(){"
        "var el=document.getElementById('key');"
        "el.select();"
        "if(navigator.clipboard){navigator.clipboard.writeText(el.value);}"
        "}"
        "</script>"
    )
    return render_page(f"Halo {tier.title()} license", body)


def error_page(msg: str) -> str:
    body = (
        '<p class="eyebrow">Halo</p>'
        "<h1>Could not issue a license</h1>"
        f"<p>{html_escape(msg)}</p>"
        '<p class="muted"><a href="https://deploy.langsmart.app/halo/buy/cut">Back to Cut</a></p>'
    )
    return render_page("Halo license", body)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))

    def _send(self, code: int, body: bytes, content_type: str) -> None:
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _json(self, code: int, obj: dict) -> None:
        self._send(code, json.dumps(obj).encode(), "application/json")

    def do_GET(self) -> None:
        parsed = urlparse(self.path)
        if parsed.path in ("/halo/health", "/health"):
            self._json(
                200,
                {
                    "ok": True,
                    "service": "halo-license",
                    "stripe_configured": stripe_ready(),
                    "webhook_configured": webhook_ready(),
                    "signing_configured": signing_ready(),
                    "halo_bin": halo_ready(),
                    "email_configured": email_ready(),
                    "buy": {
                        "cut": bool(buy_url("cut")),
                        "route": bool(buy_url("route")),
                    },
                    "prices": mapping.price_map(),
                },
            )
            return
        if parsed.path.startswith("/halo/buy/") or parsed.path.startswith("/buy/"):
            tier = parsed.path.rstrip("/").rsplit("/", 1)[-1].lower()
            qs = parse_qs(parsed.query)
            skip_landing = (qs.get("go") or [""])[0] == "1"
            if tier not in ("cut", "route"):
                self._send(
                    404,
                    error_page("Govern is not for sale yet." if tier == "govern" else f"Unknown SKU {tier}.").encode(),
                    "text/html; charset=utf-8",
                )
                return
            url = buy_url(tier)
            if skip_landing and url:
                self.send_response(302)
                self.send_header("Location", url)
                self.send_header("Content-Length", "0")
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                return
            self._send(200, buy_page(tier, url).encode(), "text/html; charset=utf-8")
            return
        if parsed.path in ("/halo/thanks", "/thanks"):
            qs = parse_qs(parsed.query)
            session_id = (qs.get("session_id") or [""])[0].strip()
            if not session_id:
                self._send(
                    400,
                    error_page("Missing session_id.").encode(),
                    "text/html; charset=utf-8",
                )
                return
            try:
                rec = fulfill_session(retrieve_session(session_id))
            except Exception as e:
                self._send(
                    400,
                    error_page(str(e)).encode(),
                    "text/html; charset=utf-8",
                )
                return
            html = thanks_page(rec["tier"], rec["org"], rec["license_key"], LICENSE_DAYS)
            self._send(200, html.encode(), "text/html; charset=utf-8")
            return
        self._json(404, {"error": "not found"})

    def do_HEAD(self) -> None:
        self.do_GET()

    def do_POST(self) -> None:
        parsed = urlparse(self.path)
        if parsed.path not in ("/halo/stripe/webhook", "/stripe/webhook", "/webhook"):
            self._json(404, {"error": "not found"})
            return
        length = int(self.headers.get("Content-Length", "0"))
        payload = self.rfile.read(length)
        sig = self.headers.get("Stripe-Signature", "")
        secret = os.environ.get("STRIPE_WEBHOOK_SECRET", "").strip()
        if not stripe_ready():
            self._json(503, {"error": "STRIPE_SECRET_KEY is not set"})
            return
        if not secret:
            self._json(503, {"error": "STRIPE_WEBHOOK_SECRET is not set"})
            return
        stripe.api_key = os.environ["STRIPE_SECRET_KEY"]
        try:
            event = stripe.Webhook.construct_event(payload, sig, secret)
        except Exception as e:
            self._json(400, {"error": f"bad signature: {e}"})
            return
        etype = event["type"] if isinstance(event, dict) else event.type
        obj = event["data"]["object"] if isinstance(event, dict) else event.data.object
        if hasattr(obj, "to_dict"):
            obj = obj.to_dict()
        try:
            if etype == "checkout.session.completed":
                sess = retrieve_session(obj["id"])
                fulfill_session(sess)
            elif etype == "invoice.paid":
                inv_id = obj.get("id")
                inv = retrieve_invoice(inv_id) if inv_id else obj
                fulfill_invoice(inv)
        except Exception:
            traceback.print_exc()
            self._json(500, {"error": "fulfillment failed"})
            return
        self._json(200, {"received": True, "type": etype})


def html_escape(s: str) -> str:
    return (
        s.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def main() -> None:
    ISSUED_DIR.mkdir(parents=True, exist_ok=True)
    httpd = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(
        f"halo-license listening on 127.0.0.1:{PORT} "
        f"stripe={stripe_ready()} signing={signing_ready()} halo={halo_ready()}",
        flush=True,
    )
    httpd.serve_forever()


if __name__ == "__main__":
    main()
