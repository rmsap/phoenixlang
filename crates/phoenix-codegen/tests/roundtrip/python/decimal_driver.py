# Behavioral Decimal round-trip driver for the Python target.
#
# Committed source. The Rust harness (`roundtrip.rs::decimal_python_roundtrip`)
# generates the small Decimal schema into ./generated_decimal/ (separate from the
# other round-trips' packages), then runs this with the committed `.venv`. It
# drives the generated httpx client in-process against the generated FastAPI
# server via `httpx.ASGITransport` and proves `Decimal` round-trips as exact
# decimal strings both ways: body decimals (required / optional / list / map) come
# back as equal `decimal.Decimal`s (proving `model_dump(mode="json")` → server
# parse → revalidate); a `Decimal` query param round-trips (echoed by the stub
# into the body); a required response-header decimal round-trips via
# `str()` / `Decimal(...)`; and a bare `Decimal` response decodes via
# `Decimal(response.json())`. pydantic validates every Decimal on parse, so the
# malformed body + query raw requests must be rejected. Exits non-zero on failure.

from __future__ import annotations

import asyncio
import sys
from decimal import Decimal

import httpx
from fastapi import FastAPI

from generated_decimal import models as m
from generated_decimal.client import ApiClient
from generated_decimal.server import create_router


def fail(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


COMPUTED_TAX = Decimal("8.25")


class Stub:
    async def echo_invoice(self, body: m.EchoInvoiceBody) -> m.Invoice:
        return m.Invoice(
            id=body.id,
            subtotal=body.subtotal,
            discount=body.discount,
            line_totals=body.line_totals,
            rates=body.rates,
        )

    async def get_quote(self, id: str, *, min_amount: Decimal) -> m.GetQuoteResult:
        return m.GetQuoteResult(
            body=m.Invoice(id=1, subtotal=min_amount, line_totals=[], rates={}),
            computed_tax=COMPUTED_TAX,
        )

    async def exchange_rate(self) -> Decimal:
        return Decimal("1.0825")


async def main() -> None:
    app = FastAPI()
    app.include_router(create_router(Stub()))
    client = ApiClient("http://test")
    client.client = httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    )

    subtotal = Decimal("19.99")
    discount = Decimal("-2.50")
    a = Decimal("10.00")
    b = Decimal("9.99")

    resp = await client.echo_invoice(
        m.EchoInvoiceBody(
            id=7,
            subtotal=subtotal,
            discount=discount,
            line_totals=[a, b],
            rates={"usd": Decimal("1.0"), "eur": Decimal("0.92")},
        )
    )
    if resp.subtotal != subtotal:
        fail(f"echo subtotal: {resp.subtotal} != {subtotal}")
    if resp.discount != discount:
        fail(f"echo discount: {resp.discount} != {discount}")
    if resp.line_totals != [a, b]:
        fail(f"echo line_totals: {resp.line_totals}")
    if resp.rates != {"usd": Decimal("1.0"), "eur": Decimal("0.92")}:
        fail(f"echo rates: {resp.rates}")

    min_amount = Decimal("5.00")
    r2 = await client.get_quote("inv-1", min_amount=min_amount)
    if r2.body.subtotal != min_amount:
        fail(f"query decimal not round-tripped: {r2.body.subtotal} != {min_amount}")
    if r2.computed_tax != COMPUTED_TAX:
        fail(f"computed_tax header: {r2.computed_tax}")

    rate = await client.exchange_rate()
    if rate != Decimal("1.0825"):
        fail(f"bare decimal response: {rate}")

    # Reject path: a malformed body decimal must be rejected by pydantic's Decimal
    # parse on the server. POST raw JSON to bypass the typed client (which would
    # refuse to construct the model locally), so the server-side decode fails.
    bad = await client.client.post(
        "/invoices",
        json={"id": 1, "subtotal": "not-a-number", "lineTotals": [], "rates": {}},
    )
    if bad.status_code < 400:
        fail(f"server accepted malformed body decimal: HTTP {bad.status_code}")

    # Reject path (query decimal): unlike Go, Python validates query decimals —
    # FastAPI coerces the `min_amount: Decimal` param and 422s on malformed input.
    bad_q = await client.client.get("/quote/inv-1", params={"minAmount": "not-a-number"})
    if bad_q.status_code < 400:
        fail(f"server accepted malformed query decimal: HTTP {bad_q.status_code}")

    print("OK")


asyncio.run(main())
