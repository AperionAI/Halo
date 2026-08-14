import os
import unittest

import mapping


class PriceMapTests(unittest.TestCase):
    def test_csv_test_prices(self):
        self.assertEqual(
            mapping.tier_for_price_id("price_1U4QRRBy8SW7BDty1zt4pmAX"), "cut"
        )
        self.assertEqual(
            mapping.tier_for_price_id("price_1U4QS0By8SW7BDtySt9O1FQj"), "route"
        )
        self.assertEqual(
            mapping.tier_for_price_id("price_1U4QSOBy8SW7BDtymac0hDwM"), "govern"
        )

    def test_unknown_is_none(self):
        self.assertIsNone(mapping.tier_for_price_id("price_not_ours"))
        self.assertIsNone(mapping.tier_for_price_id(None))
        self.assertIsNone(mapping.tier_for_price_id(""))

    def test_env_override(self):
        os.environ["STRIPE_PRICE_CUT"] = "price_live_cut_example"
        try:
            self.assertEqual(mapping.tier_for_price_id("price_live_cut_example"), "cut")
            self.assertIsNone(
                mapping.tier_for_price_id("price_1U4QRRBy8SW7BDty1zt4pmAX")
            )
        finally:
            del os.environ["STRIPE_PRICE_CUT"]

    def test_session_expanded_line_items(self):
        session = {
            "line_items": {
                "data": [
                    {"price": {"id": "price_1U4QS0By8SW7BDtySt9O1FQj"}}
                ]
            }
        }
        pid = mapping.first_price_id_from_session(session)
        self.assertEqual(mapping.tier_for_price_id(pid), "route")

    def test_session_price_as_id_string(self):
        session = {"line_items": {"data": [{"price": "price_1U4QRRBy8SW7BDty1zt4pmAX"}]}}
        pid = mapping.first_price_id_from_session(session)
        self.assertEqual(mapping.tier_for_price_id(pid), "cut")

    def test_invoice_price_object(self):
        invoice = {
            "lines": {"data": [{"price": {"id": "price_1U4QRRBy8SW7BDty1zt4pmAX"}}]}
        }
        pid = mapping.first_price_id_from_invoice(invoice)
        self.assertEqual(mapping.tier_for_price_id(pid), "cut")

    def test_invoice_pricing_price_details(self):
        invoice = {
            "lines": {
                "data": [
                    {
                        "pricing": {
                            "price_details": {
                                "price": "price_1U4QS0By8SW7BDtySt9O1FQj"
                            }
                        }
                    }
                ]
            }
        }
        pid = mapping.first_price_id_from_invoice(invoice)
        self.assertEqual(mapping.tier_for_price_id(pid), "route")


if __name__ == "__main__":
    unittest.main()

