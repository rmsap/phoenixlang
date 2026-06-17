# Behavioral Money round-trip driver for the Python target.
#
# Committed source. The Rust harness (`roundtrip.rs::money_python_roundtrip`)
# generates the small Money schema into ./generated_money/, then runs this with
# the committed `.venv`. `Money` is a pydantic model (`{amount: Decimal, currency:
# str}` + a currency validator); the proof is it round-trips in a body (required /
# optional / nested in a list element) and as a bare response, and that the server
# rejects a malformed amount and an unknown currency (pydantic validates on parse).
# Exits non-zero on failure.

from __future__ import annotations

import asyncio
import sys
from decimal import Decimal

import httpx
from fastapi import FastAPI

from generated_money import models as m
from generated_money.client import ApiClient
from generated_money.server import create_router


def fail(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


class Stub:
    async def echo_invoice(self, body: m.EchoInvoiceBody) -> m.Invoice:
        return m.Invoice(
            id=body.id,
            total=body.total,
            tip=body.tip,
            items=body.items,
            charges=body.charges,
            by_category=body.by_category,
        )

    async def get_balance(self) -> m.Money:
        return m.Money(amount=Decimal("100.00"), currency="EUR")


async def main() -> None:
    app = FastAPI()
    app.include_router(create_router(Stub()))
    client = ApiClient("http://test")
    client.client = httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    )

    total = m.Money(amount=Decimal("19.99"), currency="USD")
    tip = m.Money(amount=Decimal("2.50"), currency="USD")
    charges = [
        m.Money(amount=Decimal("1.00"), currency="USD"),
        m.Money(amount=Decimal("3.00"), currency="EUR"),
    ]
    by_category = {"shipping": m.Money(amount=Decimal("4.50"), currency="USD")}
    resp = await client.echo_invoice(
        m.EchoInvoiceBody(
            id=7,
            total=total,
            tip=tip,
            items=[m.LineItem(label="widget", price=m.Money(amount=Decimal("9.99"), currency="USD"))],
            charges=charges,
            by_category=by_category,
        )
    )
    if resp.total != total:
        fail(f"echo total: {resp.total} != {total}")
    if resp.tip != tip:
        fail(f"echo tip: {resp.tip} != {tip}")
    if len(resp.items) != 1 or resp.items[0].price != m.Money(
        amount=Decimal("9.99"), currency="USD"
    ):
        fail(f"echo items: {resp.items}")
    # Direct `List<Money>` element round-trip.
    if resp.charges != charges:
        fail(f"echo charges: {resp.charges} != {charges}")
    # `Map<String, Money>` value round-trip.
    if resp.by_category != by_category:
        fail(f"echo by_category: {resp.by_category} != {by_category}")

    bal = await client.get_balance()
    if bal != m.Money(amount=Decimal("100.00"), currency="EUR"):
        fail(f"bare Money response: {bal}")

    # Reject path: a malformed amount is rejected by pydantic's Decimal parse on
    # the server. POST raw JSON to bypass the typed client's local validation.
    bad_amount = await client.client.post(
        "/invoices",
        json={
            "id": 1,
            "total": {"amount": "not-a-number", "currency": "USD"},
            "items": [],
            "charges": [],
            "by_category": {},
        },
    )
    if bad_amount.status_code < 400:
        fail(f"server accepted malformed Money amount: HTTP {bad_amount.status_code}")

    # Reject path: an unknown currency is rejected by the Money currency validator.
    bad_currency = await client.client.post(
        "/invoices",
        json={
            "id": 1,
            "total": {"amount": "1.00", "currency": "ZZZ"},
            "items": [],
            "charges": [],
            "by_category": {},
        },
    )
    if bad_currency.status_code < 400:
        fail(f"server accepted invalid ISO 4217 currency: HTTP {bad_currency.status_code}")

    print("OK")


asyncio.run(main())
