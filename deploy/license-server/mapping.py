"""Stripe price_id -> Halo ladder tier.

Test-mode IDs from the 2026-08-14 prices.csv. Override with env in production.
"""

from __future__ import annotations

import os

# Test catalog (Stripe Test mode). Live mode uses different price_ IDs.
DEFAULT_PRICES = {
    "cut": "price_1U4QRRBy8SW7BDty1zt4pmAX",
    "route": "price_1U4QS0By8SW7BDtySt9O1FQj",
    "govern": "price_1U4QSOBy8SW7BDtymac0hDwM",
}


def price_map() -> dict[str, str]:
    """price_id -> tier name."""
    out = {}
    for tier, default in DEFAULT_PRICES.items():
        pid = os.environ.get(f"STRIPE_PRICE_{tier.upper()}", default).strip()
        if pid:
            out[pid] = tier
    return out


def tier_for_price_id(price_id: str | None) -> str | None:
    if not price_id:
        return None
    return price_map().get(price_id.strip())


def first_price_id_from_session(session: dict) -> str | None:
    """Checkout Session: line_items.data[0].price.id, or metadata.price_id."""
    items = (session.get("line_items") or {}).get("data") or []
    if items:
        price = items[0].get("price") or {}
        if isinstance(price, str):
            return price
        pid = price.get("id")
        if pid:
            return pid
    meta = session.get("metadata") or {}
    return meta.get("price_id") or meta.get("halo_tier")


def first_price_id_from_invoice(invoice: dict) -> str | None:
    """Invoice: lines.data[0].price.id, or the 2024 pricing.price_details shape."""
    items = (invoice.get("lines") or {}).get("data") or []
    if not items:
        meta = invoice.get("metadata") or {}
        return meta.get("price_id")
    line = items[0]
    price = line.get("price")
    if isinstance(price, str) and price:
        return price
    if isinstance(price, dict):
        pid = price.get("id")
        if pid:
            return pid
    details = ((line.get("pricing") or {}).get("price_details") or {})
    pid = details.get("price")
    if pid:
        return pid
    meta = invoice.get("metadata") or {}
    return meta.get("price_id")
