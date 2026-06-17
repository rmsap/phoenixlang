# Behavioral UUID round-trip driver for the Python target.
#
# Committed source. The Rust harness (`roundtrip.rs::uuid_python_roundtrip`)
# generates the small UUID schema into ./generated_uuid/ (separate from the other
# round-trips' packages, so they never race), then runs this with the committed
# `.venv`. It drives the generated httpx client in-process against the generated
# FastAPI server via `httpx.ASGITransport` and proves `Uuid` round-trips as RFC
# 4122 strings both ways: body uuids (required / optional / list / map) come back
# as equal `uuid.UUID`s (proving `model_dump(mode="json")` → server parse → revalidate);
# a `Uuid` query param round-trips (echoed by the stub into the body); a required
# response-header uuid round-trips via `str()` / `UUID(...)`; and a bare `Uuid`
# response decodes via `UUID(response.json())`. pydantic validates every UUID on
# parse — sending valid ones exercises that accept path. Exits non-zero on failure.

from __future__ import annotations

import asyncio
import sys
from uuid import UUID

import httpx
from fastapi import FastAPI

from generated_uuid import models as m
from generated_uuid.client import ApiClient
from generated_uuid.server import create_router


def fail(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


REQ_ID = UUID("11111111-1111-1111-1111-111111111111")


class Stub:
    async def echo_account(self, body: m.EchoAccountBody) -> m.Account:
        return m.Account(
            id=body.id,
            owner_id=body.owner_id,
            members=body.members,
            index=body.index,
        )

    async def get_account(self, id: str, *, ref: UUID) -> m.GetAccountResult:
        # Echo the parsed query uuid into id so the client can assert it round-tripped.
        return m.GetAccountResult(
            body=m.Account(id=ref, members=[], index={}),
            request_id=REQ_ID,
        )

    async def new_id(self) -> UUID:
        return UUID("550e8400-e29b-41d4-a716-446655440000")


async def main() -> None:
    app = FastAPI()
    app.include_router(create_router(Stub()))
    client = ApiClient("http://test")
    client.client = httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    )

    id_a = UUID("550e8400-e29b-41d4-a716-446655440000")
    id_b = UUID("6ba7b810-9dad-11d1-80b4-00c04fd430c8")
    id_c = UUID("6ba7b811-9dad-11d1-80b4-00c04fd430c8")

    resp = await client.echo_account(
        m.EchoAccountBody(
            id=id_a, owner_id=id_b, members=[id_c], index={"primary": id_a}
        )
    )
    if resp.id != id_a:
        fail(f"echo id: {resp.id} != {id_a}")
    if resp.owner_id != id_b:
        fail(f"echo owner_id: {resp.owner_id} != {id_b}")
    if resp.members != [id_c]:
        fail(f"echo members: {resp.members} != [{id_c}]")
    if resp.index != {"primary": id_a}:
        fail(f"echo index: {resp.index} != {{primary:{id_a}}}")

    ref = UUID("00000000-0000-0000-0000-000000000000")
    r2 = await client.get_account("acct-1", ref=ref)
    if r2.body.id != ref:
        fail(f"query uuid not round-tripped: {r2.body.id} != {ref}")
    if r2.request_id != REQ_ID:
        fail(f"request_id header: {r2.request_id}")

    new = await client.new_id()
    if new != id_a:
        fail(f"bare uuid response: {new} != {id_a}")

    # Reject path: a malformed body uuid must be rejected by pydantic's UUID parse
    # on the server. Bypass the typed client (which would refuse to construct the
    # model locally) by POSTing raw JSON, so the server-side decode is what fails.
    bad = await client.client.post(
        "/accounts", json={"id": "not-a-uuid", "members": [], "index": {}}
    )
    if bad.status_code < 400:
        fail(f"server accepted malformed body uuid: HTTP {bad.status_code}")

    # Reject path (query uuid): unlike Go, Python validates query uuids — FastAPI
    # coerces the `ref: UUID` param and 422s on malformed input. Bypass the typed
    # client (which won't construct a bad UUID locally) by issuing the raw GET, so
    # the server-side parse is what rejects it. Pins the Go-accepts divergence.
    bad_q = await client.client.get("/accounts/acct-1", params={"ref": "not-a-uuid"})
    if bad_q.status_code < 400:
        fail(f"server accepted malformed query uuid: HTTP {bad_q.status_code}")

    print("OK")


asyncio.run(main())
