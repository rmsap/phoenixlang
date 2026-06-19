# Behavioral list-valued-param round-trip driver for the Python target.
#
# Committed source. The Rust harness (`roundtrip.rs::list_python_roundtrip`)
# generates the small list schema into ./generated_list/ (separate from the other
# round-trips' packages, so they never race), then runs this with the committed
# `.venv`. It drives the generated httpx client in-process against the generated
# FastAPI server via `httpx.ASGITransport` and proves list-valued params survive
# the wire: `List<String>`/`List<Int>`/`List<Uuid>`/`List<Status>` (a simple enum)
# query params (FastAPI native `list[T]`, repeated keys) and request headers
# covering every element type — `String`/`Int`/`Uuid`/`Float`/`Bool`/`DateTime`/
# `Decimal`/enum (comma-joined on send, split + coerced per element server-side) —
# query params and request headers, BOTH covering every element type —
# `String`/`Int`/`Uuid`/`Status` (a simple enum)/`Float`/`Bool`/`DateTime`/`Decimal`
# — echo back unchanged, including the empty list. Both positions carry every type
# because Python's two paths DIVERGE and must each be exercised end to end: a query
# `list[T]` is parsed natively by FastAPI from repeated keys (and sent by the
# `py_list_query_value` client encoders — `[x.value ...]`/`["true" if x ...]`/
# `[x.isoformat() ...]`/`[str(x) ...]`), whereas a comma header can't be split into
# `list[T]` natively, so the route body coerces each element manually
# (`int(...)`/`float(...)`/`UUID(...)`/`Decimal(...)`/`datetime.fromisoformat(...)`/
# `Status(...)`/`== "true"`). Only well-formed header input is asserted here; a
# malformed header element raises in that route-body coercion → 500, the documented
# query-vs-header divergence, so it is NOT asserted. The reject path is pinned via
# the query elements instead: a raw GET with an unknown enum variant and a malformed
# `Uuid` are each rejected by FastAPI's native `list[T]` coercion (422). Exits
# non-zero on failure.

from __future__ import annotations

import asyncio
import sys
from datetime import datetime, timezone
from decimal import Decimal
from uuid import UUID

import httpx
from fastapi import FastAPI

from generated_list import models as m
from generated_list.client import ApiClient
from generated_list.server import create_router

UUID_A = "11111111-1111-1111-1111-111111111111"
UUID_B = "22222222-2222-2222-2222-222222222222"


def fail(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


class Stub:
    async def search(
        self,
        *,
        ids: list[str],
        counts: list[int],
        uuids: list[UUID],
        statuses: list[m.Status],
        q_floats: list[float],
        q_flags: list[bool],
        q_times: list[datetime],
        q_amounts: list[Decimal],
        roles: list[str],
        limits: list[int],
        keys: list[UUID],
        ratios: list[float],
        flags: list[bool],
        times: list[datetime],
        amounts: list[Decimal],
        tags: list[m.Status],
    ) -> m.Echo:
        return m.Echo(
            ids=ids,
            counts=counts,
            uuids=uuids,
            statuses=statuses,
            q_floats=q_floats,
            q_flags=q_flags,
            q_times=q_times,
            q_amounts=q_amounts,
            roles=roles,
            limits=limits,
            keys=keys,
            ratios=ratios,
            flags=flags,
            times=times,
            amounts=amounts,
            tags=tags,
        )


async def main() -> None:
    app = FastAPI()
    app.include_router(create_router(Stub()))
    client = ApiClient("http://test")
    client.client = httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    )

    t1 = datetime(2024, 1, 15, 8, 30, 0, tzinfo=timezone.utc)
    t2 = datetime(2024, 2, 20, 16, 45, 0, tzinfo=timezone.utc)

    # Multiple elements in each position round-trip in order. Both query and header
    # cover every list element type (str/int/UUID/enum/float/bool/datetime/Decimal):
    # the query block exercises FastAPI's native `list[T]` parsing + the
    # `py_list_query_value` client encoders, the header block the route-body split →
    # per-element manual coerce path.
    echo = await client.search(
        ids=["a", "b", "c"],
        counts=[1, 2, 3],
        uuids=[UUID(UUID_A), UUID(UUID_B)],
        statuses=[m.Status.ACTIVE, m.Status.PENDING],
        q_floats=[0.5, 1.25],
        q_flags=[True, False],
        q_times=[t1, t2],
        q_amounts=[Decimal("7.75"), Decimal("8.00")],
        roles=["admin", "editor"],
        limits=[10, 20],
        keys=[UUID(UUID_A), UUID(UUID_B)],
        ratios=[1.5, 2.5],
        flags=[True, False],
        times=[t1, t2],
        amounts=[Decimal("10.50"), Decimal("3.25")],
        tags=[m.Status.ACTIVE, m.Status.INACTIVE],
    )
    # Query lists.
    if echo.ids != ["a", "b", "c"]:
        fail(f"ids: {echo.ids}")
    if echo.counts != [1, 2, 3]:
        fail(f"counts: {echo.counts}")
    if [str(u) for u in echo.uuids] != [UUID_A, UUID_B]:
        fail(f"uuids: {echo.uuids}")
    if echo.statuses != [m.Status.ACTIVE, m.Status.PENDING]:
        fail(f"statuses: {echo.statuses}")
    if echo.q_floats != [0.5, 1.25]:
        fail(f"qFloats query: {echo.q_floats}")
    if echo.q_flags != [True, False]:
        fail(f"qFlags query: {echo.q_flags}")
    if echo.q_times != [t1, t2]:
        fail(f"qTimes query: {echo.q_times}")
    # `Decimal` equality is numeric, so a dropped trailing zero never fails this.
    if echo.q_amounts != [Decimal("7.75"), Decimal("8.00")]:
        fail(f"qAmounts query: {echo.q_amounts}")
    # Header lists.
    if echo.roles != ["admin", "editor"]:
        fail(f"roles header: {echo.roles}")
    if echo.limits != [10, 20]:
        fail(f"limits header: {echo.limits}")
    if [str(u) for u in echo.keys] != [UUID_A, UUID_B]:
        fail(f"keys header: {echo.keys}")
    if echo.ratios != [1.5, 2.5]:
        fail(f"ratios header: {echo.ratios}")
    if echo.flags != [True, False]:
        fail(f"flags header: {echo.flags}")
    if echo.times != [t1, t2]:
        fail(f"times header: {echo.times}")
    # `Decimal` equality is numeric, so a dropped trailing zero never fails this.
    if echo.amounts != [Decimal("10.50"), Decimal("3.25")]:
        fail(f"amounts header: {echo.amounts}")
    if echo.tags != [m.Status.ACTIVE, m.Status.INACTIVE]:
        fail(f"tags header: {echo.tags}")

    # Empty lists round-trip as empty.
    empty = await client.search(
        ids=[],
        counts=[],
        uuids=[],
        statuses=[],
        q_floats=[],
        q_flags=[],
        q_times=[],
        q_amounts=[],
        roles=[],
        limits=[],
        keys=[],
        ratios=[],
        flags=[],
        times=[],
        amounts=[],
        tags=[],
    )
    if (
        empty.ids
        or empty.counts
        or empty.uuids
        or empty.statuses
        or empty.q_floats
        or empty.q_flags
        or empty.q_times
        or empty.q_amounts
        or empty.roles
        or empty.limits
        or empty.keys
        or empty.ratios
        or empty.flags
        or empty.times
        or empty.amounts
        or empty.tags
    ):
        fail(f"empty lists not all empty: {empty}")

    # Reject path: an unknown enum element must be rejected by FastAPI's enum
    # coercion (422). Issue the raw GET so the server-side parse is what fails (the
    # typed client would refuse to construct a bad enum locally).
    bad = await client.client.get("/search", params={"statuses": "Bogus"})
    if bad.status_code < 400:
        fail(f"server accepted unknown enum list element: HTTP {bad.status_code}")

    # Reject path: a malformed Uuid element must fail FastAPI's list[UUID]
    # coercion (422) — parallel to the Go/TS per-element format check.
    bad_uuid = await client.client.get("/search", params={"uuids": "not-a-uuid"})
    if bad_uuid.status_code < 400:
        fail(f"server accepted malformed uuid list element: HTTP {bad_uuid.status_code}")

    print("OK")


asyncio.run(main())
