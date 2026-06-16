# Behavioral DateTime round-trip driver for the Python target.
#
# Committed source. The Rust harness (`roundtrip.rs::datetime_python_roundtrip`)
# generates the small DateTime schema into ./generated_dt/ (a separate package
# from the contract-driven ./generated/, so the two Python round-trips never
# race), then runs this with the committed `.venv`. It drives the generated httpx
# client in-process against the generated FastAPI server via `httpx.ASGITransport`
# (no real port) and proves `DateTime` survives the RFC 3339 wire trip both ways:
#   - request body Dates (required / optional / list) echo back as equal instants
#     (proving `model_dump(mode="json")` → server parse → response → revalidate);
#   - a `DateTime` query param round-trips (echoed by the stub into the body);
#   - a required `DateTime` response header round-trips via `.isoformat()` /
#     `datetime.fromisoformat(...)`.
# Plain script (no pytest): exits non-zero on the first failure.

from __future__ import annotations

import asyncio
import sys
from datetime import datetime, timezone

import httpx
from fastapi import FastAPI

from generated_dt import models as m
from generated_dt.client import ApiClient
from generated_dt.server import create_router


def fail(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


class Stub:
    async def echo_event(self, body: m.EchoEventBody) -> m.Event:
        # Echo the decoded body back unchanged.
        return m.Event(
            id=body.id,
            name=body.name,
            starts_at=body.starts_at,
            ends_at=body.ends_at,
            checkpoints=body.checkpoints,
            phases=body.phases,
        )

    async def echo_task(self, body: m.EchoTaskBody) -> m.Task:
        # Echo a body whose only datetime is nested inside `Reminder`.
        return m.Task(id=body.id, reminder=body.reminder)

    async def echo_instant(self, *, at: datetime) -> datetime:
        # Bare scalar DateTime response.
        return at

    async def echo_instants(self, *, at: datetime) -> list[datetime]:
        # Bare List<DateTime> response.
        return [at]

    async def echo_instant_map(self, *, at: datetime) -> dict[str, datetime]:
        # Bare Map<String, DateTime> response.
        return {"at": at}

    async def get_event(self, id: str, *, since: datetime) -> m.GetEventResult:
        # Echo the parsed query date into starts_at; set the required served_at
        # and the optional expires_at so the client can assert the optional
        # response-header read path round-trips.
        return m.GetEventResult(
            body=m.Event(id=1, name=id, starts_at=since, checkpoints=[], phases={}),
            served_at=datetime(2020, 1, 2, 3, 4, 5, tzinfo=timezone.utc),
            expires_at=datetime(2020, 1, 3, 0, 0, 0, tzinfo=timezone.utc),
        )


async def main() -> None:
    app = FastAPI()
    app.include_router(create_router(Stub()))
    client = ApiClient("http://test")
    client.client = httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    )

    start = datetime(2026, 6, 16, 12, 30, 0, tzinfo=timezone.utc)
    end = datetime(2026, 6, 17, 8, 0, 0, tzinfo=timezone.utc)
    cp = datetime(2026, 6, 16, 13, 0, 0, tzinfo=timezone.utc)

    resp = await client.echo_event(
        m.EchoEventBody(
            id=7,
            name="launch",
            starts_at=start,
            ends_at=end,
            checkpoints=[cp],
            phases={"kickoff": start, "wrap": end},
        )
    )
    if resp.starts_at != start:
        fail(f"echo starts_at: {resp.starts_at} != {start}")
    if resp.ends_at != end:
        fail(f"echo ends_at: {resp.ends_at} != {end}")
    if resp.checkpoints != [cp]:
        fail(f"echo checkpoints: {resp.checkpoints} != [{cp}]")
    if resp.phases != {"kickoff": start, "wrap": end}:
        fail(f"echo phases: {resp.phases} != {{kickoff:{start}, wrap:{end}}}")

    # Body whose only datetime is nested in a struct field: regresses if the
    # client's `model_dump(mode="json")` gate isn't transitive (a raw nested
    # `datetime` then reaches httpx's `json.dumps` and raises).
    task = await client.echo_task(
        m.EchoTaskBody(id=3, reminder=m.Reminder(note="ping", remind_at=cp))
    )
    if task.reminder.remind_at != cp:
        fail(f"nested remind_at: {task.reminder.remind_at} != {cp}")

    since = datetime(2025, 12, 31, 23, 59, 59, tzinfo=timezone.utc)
    r2 = await client.get_event("evt-9", since=since)
    if r2.body.starts_at != since:
        fail(f"query date not round-tripped: {r2.body.starts_at} != {since}")
    if r2.served_at != datetime(2020, 1, 2, 3, 4, 5, tzinfo=timezone.utc):
        fail(f"served_at header: {r2.served_at}")
    if r2.expires_at != datetime(2020, 1, 3, 0, 0, 0, tzinfo=timezone.utc):
        fail(f"expires_at optional header: {r2.expires_at}")

    # Bare scalar / list / map DateTime responses: exercise the by-type client
    # decode (`datetime.fromisoformat(...)` / comprehensions), not the object-only
    # `Type(**response.json())` form that crashed on a scalar.
    inst = await client.echo_instant(at=start)
    if inst != start:
        fail(f"bare scalar instant: {inst} != {start}")
    insts = await client.echo_instants(at=start)
    if insts != [start]:
        fail(f"bare list instants: {insts} != [{start}]")
    inst_map = await client.echo_instant_map(at=start)
    if inst_map != {"at": start}:
        fail(f"bare map instants: {inst_map} != {{at:{start}}}")

    print("OK")


asyncio.run(main())
