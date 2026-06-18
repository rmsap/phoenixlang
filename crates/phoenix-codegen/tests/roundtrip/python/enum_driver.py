# Behavioral enum query/header round-trip driver for the Python target.
#
# Committed source. The Rust harness (`roundtrip.rs::enum_python_roundtrip`)
# generates the small enum schema into ./generated_enum/ (separate from the other
# round-trips' packages, so they never race), then runs this with the committed
# `.venv`. It drives the generated httpx client in-process against the generated
# FastAPI server via `httpx.ASGITransport` and proves enum query/header values
# round-trip as the bare variant string: a required + Option request header, a
# required query param, and a required + Option response header all come back as
# the right `(str, Enum)` member (FastAPI coerces the wire string into the enum on
# receive; the client sends `.value` and reconstructs on read). A raw request
# omitting the defaulted `size` proves the SERVER applies `Medium`; raw requests
# with an unknown query/header variant prove FastAPI rejects them (422). Exits
# non-zero on failure.

from __future__ import annotations

import asyncio
import sys

import httpx
from fastapi import FastAPI

from generated_enum import models as m
from generated_enum.client import ApiClient
from generated_enum.server import create_router


def fail(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


class Stub:
    async def pick_item(
        self,
        *,
        color: m.Color,
        size: m.Size,
        preferred: m.Color,
        fallback: m.Color | None = None,
    ) -> m.PickItemResult:
        # Echo the query enums into the body and the header enums into the
        # response headers, so the client can assert each position round-tripped.
        return m.PickItemResult(
            body=m.Item(name="picked", color=color, size=size),
            chosen=preferred,
            alt=fallback,
        )


async def main() -> None:
    app = FastAPI()
    app.include_router(create_router(Stub()))
    client = ApiClient("http://test")
    client.client = httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    )

    # Required + Option enum query/header values all round-trip the wire.
    r = await client.pick_item(
        color=m.Color.BLUE,
        size=m.Size.LARGE,
        preferred=m.Color.RED,
        fallback=m.Color.GREEN,
    )
    if r.body.color != m.Color.BLUE:
        fail(f"query color: {r.body.color} != Blue")
    if r.body.size != m.Size.LARGE:
        fail(f"query size: {r.body.size} != Large")
    if r.chosen != m.Color.RED:
        fail(f"required response-header enum: {r.chosen} != Red")
    if r.alt != m.Color.GREEN:
        fail(f"optional response-header enum: {r.alt} != Green")

    # Server-side default: a raw GET omitting `size` must have the server apply
    # `Medium` (the typed client would otherwise send its own default).
    raw = await client.client.get(
        "/pick", params={"color": "Red"}, headers={"X-Preferred": "Blue"}
    )
    if raw.status_code != 200:
        fail(f"server-default request failed: HTTP {raw.status_code}")
    if raw.json()["size"] != "Medium":
        fail(f"defaulted query enum not applied server-side: {raw.json()['size']}")

    # Reject path (query): an unknown variant must be rejected by FastAPI's enum
    # coercion (422). Issue the raw GET so the server-side parse is what fails (the
    # typed client would refuse to construct a bad enum locally).
    bad_q = await client.client.get(
        "/pick", params={"color": "Purple", "size": "Small"}, headers={"X-Preferred": "Red"}
    )
    if bad_q.status_code < 400:
        fail(f"server accepted unknown query enum: HTTP {bad_q.status_code}")

    # Reject path (header): an unknown header variant must likewise 422.
    bad_h = await client.client.get(
        "/pick", params={"color": "Red", "size": "Small"}, headers={"X-Preferred": "Mauve"}
    )
    if bad_h.status_code < 400:
        fail(f"server accepted unknown header enum: HTTP {bad_h.status_code}")

    print("OK")


asyncio.run(main())
