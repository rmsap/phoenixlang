# Behavioral inline-response-projection round-trip driver for the Python target.
#
# Committed source. The Rust harness (`roundtrip.rs::projection_python_roundtrip`)
# generates the small projection schema into ./generated_projection/ (separate from
# the other round-trips' packages, so they never race), then runs this with the
# committed `.venv`. It drives the generated httpx client in-process against the
# generated FastAPI server via `httpx.ASGITransport` and proves the generated
# `<Endpoint>Response` projected models round-trip the wire: a bare projected
# response, a `list[…]` of them, and a `partial` projection (every field optional —
# `Optional[…] = None`) carry their picked `Uuid`/`DateTime` fields back as equal
# `uuid.UUID`/`datetime` values (pydantic decode of the projected model). Exits
# non-zero on failure.

from __future__ import annotations

import asyncio
import sys
from datetime import datetime, timezone
from uuid import UUID

import httpx
from fastapi import FastAPI

from generated_projection import models as m
from generated_projection.client import ApiClient
from generated_projection.server import create_router


def fail(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


PROFILE_CREATED = datetime(2026, 1, 2, 3, 4, 5, tzinfo=timezone.utc)
LIST_CREATED = datetime(2026, 2, 3, 4, 5, 6, tzinfo=timezone.utc)
SUMMARY_CREATED = datetime(2026, 3, 4, 5, 6, 7, tzinfo=timezone.utc)
CONTACT_CREATED = datetime(2026, 4, 5, 6, 7, 8, tzinfo=timezone.utc)


class Stub:
    async def get_profile(self, id: str) -> m.GetProfileResponse:
        return m.GetProfileResponse(
            id=UUID("11111111-1111-1111-1111-111111111111"),
            display_name=id,
            created_at=PROFILE_CREATED,
        )

    async def list_profiles(self) -> list[m.ListProfilesResponse]:
        return [
            m.ListProfilesResponse(
                id=UUID("22222222-2222-2222-2222-222222222222"),
                display_name="ada",
                created_at=LIST_CREATED,
            )
        ]

    async def get_summary(self, id: str) -> m.GetSummaryResponse:
        return m.GetSummaryResponse(
            id=UUID("33333333-3333-3333-3333-333333333333"),
            display_name=id,
            created_at=SUMMARY_CREATED,
        )

    async def get_contact(self, id: str) -> m.GetContactResponse:
        return m.GetContactResponse(
            id=UUID("44444444-4444-4444-4444-444444444444"),
            display_name=id,
            email="ada@example.com",
            created_at=CONTACT_CREATED,
        )


async def main() -> None:
    app = FastAPI()
    app.include_router(create_router(Stub()))
    client = ApiClient("http://test")
    client.client = httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    )

    # Bare projected response: the picked fields round-trip and decode by type.
    p = await client.get_profile("grace")
    if p.id != UUID("11111111-1111-1111-1111-111111111111"):
        fail(f"projected id: {p.id}")
    if p.display_name != "grace":
        fail(f"projected display_name: {p.display_name}")
    if p.created_at != PROFILE_CREATED:
        fail(f"projected created_at: {p.created_at}")

    # List of projected responses: each element round-trips.
    rows = await client.list_profiles()
    if len(rows) != 1:
        fail(f"listProfiles length: {len(rows)}")
    row = rows[0]
    if row.id != UUID("22222222-2222-2222-2222-222222222222") or row.display_name != "ada":
        fail(f"projected list element: {row}")
    if row.created_at != LIST_CREATED:
        fail(f"projected list created_at: {row.created_at}")

    # Partial projected response: every field optional (`Optional[…] = None`);
    # present values still round-trip and decode by type.
    summ = await client.get_summary("turing")
    if summ.id != UUID("33333333-3333-3333-3333-333333333333"):
        fail(f"partial id: {summ.id}")
    if summ.display_name != "turing":
        fail(f"partial display_name: {summ.display_name}")
    if summ.created_at != SUMMARY_CREATED:
        fail(f"partial created_at: {summ.created_at}")

    # Omit projection: the complementary selector (drops `passwordHash`); the
    # kept fields — incl. `email` — round-trip and decode by type.
    contact = await client.get_contact("ada")
    if contact.id != UUID("44444444-4444-4444-4444-444444444444"):
        fail(f"omit id: {contact.id}")
    if contact.display_name != "ada" or contact.email != "ada@example.com":
        fail(f"omit fields: {contact}")
    if contact.created_at != CONTACT_CREATED:
        fail(f"omit created_at: {contact.created_at}")

    print("OK")


asyncio.run(main())
