import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import server


class FulfillTests(unittest.TestCase):
    def test_rejects_unknown_price(self):
        session = {
            "id": "cs_test_unknown",
            "status": "complete",
            "payment_status": "paid",
            "customer_email": "a@b.com",
            "line_items": {"data": [{"price": {"id": "price_not_halo"}}]},
        }
        with self.assertRaisesRegex(RuntimeError, "unknown Stripe price"):
            server.fulfill_session(session)

    def test_rejects_govern_for_now(self):
        session = {
            "id": "cs_test_govern",
            "status": "complete",
            "payment_status": "paid",
            "customer_email": "a@b.com",
            "line_items": {
                "data": [{"price": {"id": "price_1U4QSOBy8SW7BDtymac0hDwM"}}]
            },
        }
        with self.assertRaisesRegex(RuntimeError, "Govern is not for sale"):
            server.fulfill_session(session)

    def test_mints_cut_and_is_idempotent(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        server.ISSUED_DIR = Path(tmp.name)
        session = {
            "id": "cs_test_cut",
            "status": "complete",
            "payment_status": "paid",
            "customer_email": "buyer@example.com",
            "line_items": {
                "data": [{"price": {"id": "price_1U4QRRBy8SW7BDty1zt4pmAX"}}]
            },
        }
        with patch.object(server, "mint_license", return_value="LIC_CUT_1") as mint:
            rec = server.fulfill_session(session)
            rec2 = server.fulfill_session(session)
        self.assertEqual(rec["tier"], "cut")
        self.assertEqual(rec["license_key"], "LIC_CUT_1")
        self.assertEqual(rec2["license_key"], "LIC_CUT_1")
        self.assertEqual(mint.call_count, 1)
        on_disk = json.loads((Path(tmp.name) / "cs_test_cut.json").read_text())
        self.assertEqual(on_disk["org"], "buyer@example.com")

    def test_invoice_create_does_not_mint(self):
        invoice = {
            "id": "in_create",
            "billing_reason": "subscription_create",
            "status": "paid",
            "paid": True,
            "customer_email": "a@b.com",
            "lines": {
                "data": [{"price": {"id": "price_1U4QRRBy8SW7BDty1zt4pmAX"}}]
            },
        }
        with patch.object(server, "mint_license") as mint:
            self.assertIsNone(server.fulfill_invoice(invoice))
        mint.assert_not_called()

    def test_invoice_cycle_mints_and_is_idempotent(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        server.ISSUED_DIR = Path(tmp.name)
        invoice = {
            "id": "in_cycle_1",
            "billing_reason": "subscription_cycle",
            "status": "paid",
            "paid": True,
            "customer_email": "renew@example.com",
            "lines": {
                "data": [{"price": {"id": "price_1U4QS0By8SW7BDtySt9O1FQj"}}]
            },
        }
        with patch.object(server, "mint_license", return_value="LIC_ROUTE_R") as mint:
            rec = server.fulfill_invoice(invoice)
            rec2 = server.fulfill_invoice(invoice)
        self.assertEqual(rec["tier"], "route")
        self.assertEqual(rec["license_key"], "LIC_ROUTE_R")
        self.assertEqual(rec2["license_key"], "LIC_ROUTE_R")
        self.assertEqual(mint.call_count, 1)

    def test_thanks_page_uses_apply_and_docs_chrome(self):
        html = server.thanks_page("cut", "a@b.com", "TOKEN_VALUE", "35")
        self.assertIn("halo license apply", html)
        self.assertIn("#0f172a", html)
        self.assertIn("docs.aperion.ai", html)
        self.assertIn("TOKEN_VALUE", html)
        self.assertNotIn("config.yaml", html)

    def test_buy_page_without_link_still_renders(self):
        html = server.buy_page("route", "")
        self.assertIn("Halo Route", html)
        self.assertIn("/halo/buy/cut", html)
        self.assertIn("#0f172a", html)


if __name__ == "__main__":
    unittest.main()

