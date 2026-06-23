# Cross-language wire-conformance driver for the Python target.
#
# Committed source. The Rust harness (`roundtrip.rs::cross_lang_python_conformance`)
# generates the package into ./generated_cross_lang/ and runs this with the committed
# `.venv`. Unlike the other Python round-trips — which only prove Python's client and
# server agree with EACH OTHER — this asserts the actual bytes Python puts on the wire
# equal the single golden contract (../cross_lang/wire.json) every target is checked
# against. Conformance of all three targets to one wire ⟹ any client interoperates
# with any server, without cross-process pairing. It is the regression guard for the
# Python camelCase-wire fix.
#
# The generated httpx client is driven against the generated FastAPI server through a
# recording transport that captures the request the client sends and the response it
# receives; both are compared to the golden. Comparison is structural, except a
# `createdAt` datetime compares as an INSTANT (Python emits RFC 3339 with `+00:00`
# while Go emits `Z` and TS emits `.000Z` — all valid RFC 3339 and mutually
# parseable). Exits non-zero on failure.

from __future__ import annotations

import asyncio
import json
import re
import sys
from datetime import datetime, timezone
from decimal import Decimal
from pathlib import Path
from typing import NoReturn
from uuid import UUID

import httpx
from fastapi import FastAPI

from generated_cross_lang import models as m
from generated_cross_lang.client import ApiClient
from generated_cross_lang.server import create_router

GOLDEN = json.loads(
    (Path(__file__).resolve().parent.parent / "cross_lang" / "wire.json").read_text()
)

# Typed values matching golden `account` (constructed by the snake_case Python names;
# `populate_by_name` lets that work alongside the camelCase wire aliases).
ACCT = dict(
    id=UUID("11111111-1111-1111-1111-111111111111"),
    created_at=datetime(2026, 1, 15, 8, 30, 0, tzinfo=timezone.utc),
    balance=Decimal("19.99"),
    homepage="https://Example.com/u?x=1#f",
    avatar=bytes([0x00, 0x01, 0xFF]),
    wallet=m.Money(amount=Decimal("5.00"), currency="USD"),
    role=m.Role.ADMIN,
    profile=m.Profile(display_name="Ada", avatar_url=None),
    tags=["x", "y"],
    active=True,
)


def fail(msg: str) -> NoReturn:
    print(f"FAIL: {msg}")
    sys.exit(1)


# Matches an RFC-3339-ish prefix (`YYYY-MM-DDThh:mm:ss`). `fromisoformat` is more
# lenient than this, so we gate on the prefix first — keeping all three comparators
# (Go/Python/TS) equally strict, so a non-datetime string pair can't coerce to a
# match here only.
_RFC3339_PREFIX = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}")


def _norm_dt(s: str) -> str:
    return s[:-1] + "+00:00" if s.endswith("Z") else s


def json_equal(a: object, b: object) -> bool:
    if isinstance(a, dict) and isinstance(b, dict):
        # Union of keys, treating a missing key as `null`: an absent optional (TS
        # omits it) and an explicit `null` (Go/Python emit it) are equivalent for a
        # Phoenix `Option`; a present value vs a missing key still differs, so
        # dropped REQUIRED fields are still caught. Corollary: a RENAMED field is
        # caught only when its golden value is non-null (a renamed null optional
        # like `avatarUrl` — or, equivalently, any extra spurious null-valued field
        # — slips through; the non-null `displayName` exercises the nested-struct
        # rename).
        return all(json_equal(a.get(k), b.get(k)) for k in a.keys() | b.keys())
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(json_equal(x, y) for x, y in zip(a, b))
    if isinstance(a, str) and isinstance(b, str):
        if a == b:
            return True
        if not _RFC3339_PREFIX.match(a) or not _RFC3339_PREFIX.match(b):
            return False
        try:
            return datetime.fromisoformat(_norm_dt(a)) == datetime.fromisoformat(_norm_dt(b))
        except ValueError:
            return False
    # `bool` is a subclass of `int`; keep them distinct so `True` != `1`.
    if isinstance(a, bool) or isinstance(b, bool):
        return a is b
    return a == b


def assert_wire(label: str, got_bytes: bytes, want: object) -> None:
    try:
        got = json.loads(got_bytes)
    except ValueError:
        fail(f"{label}: invalid JSON: {got_bytes!r}")
    if not json_equal(got, want):
        fail(f"{label}: wire mismatch\n got:  {got_bytes!r}\n want: {want}")


def assert_query(label: str, req: httpx.Request, want: dict[str, list[str]]) -> None:
    got: dict[str, list[str]] = {}
    for k, v in req.url.params.multi_items():
        got.setdefault(k, []).append(v)
    if got != {k: list(v) for k, v in want.items()}:
        fail(f"{label} query: {got} != {want}")


class Recording(httpx.AsyncBaseTransport):
    """Wraps the ASGI transport, capturing the request sent + response received."""

    def __init__(self, inner: httpx.AsyncBaseTransport) -> None:
        self.inner = inner
        self.req: httpx.Request | None = None
        self.resp_body = b""

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        self.req = request
        resp = await self.inner.handle_async_request(request)
        body = await resp.aread()
        self.resp_body = body
        return httpx.Response(
            resp.status_code, headers=resp.headers, content=body, request=request
        )


class Stub:
    async def create_account(self, body: m.CreateAccountBody) -> m.Account:
        # Echo the DECODED body (not the constant `ACCT`): if the server dropped or
        # renamed a field on decode, the echoed response wire would diverge from the
        # golden. This is what exercises server-side request-body decode here.
        return m.Account(
            id=body.id,
            created_at=body.created_at,
            balance=body.balance,
            homepage=body.homepage,
            avatar=body.avatar,
            wallet=body.wallet,
            role=body.role,
            profile=body.profile,
            tags=body.tags,
            active=body.active,
        )

    async def get_account(
        self,
        account_id: str,
        *,
        include_archived: bool,
        roles: list[m.Role],
        request_id: str,
    ) -> m.Account:
        return m.Account(**ACCT)

    async def list_accounts(self, *, page: int) -> m.ListAccountsPage:
        return m.ListAccountsPage(items=[m.Account(**ACCT)], total_count=3)


def _rename_key(d: dict[str, object], frm: str, to: str) -> dict[str, object]:
    return {(to if k == frm else k): v for k, v in d.items()}


async def main() -> None:
    # Meta-guard: the comparator MUST reject a snake_cased rename of a non-null
    # field — exactly the shape of the snake-wire bug this whole test exists to
    # catch. Without this, a future change that weakened `json_equal` (e.g.
    # intersecting keys instead of unioning) would make every assertion below pass
    # vacuously.
    if json_equal(_rename_key(GOLDEN["account"], "createdAt", "created_at"), GOLDEN["account"]):
        fail("comparator accepted a snake_cased rename; conformance assertions would be vacuous")
    # Meta-guard for the OTHER load-bearing rule, the datetime-instant path: it must
    # not collapse two DIFFERENT instants, and must not leak into non-datetime strings
    # (the RFC-3339 prefix gate). Either weakening would let an over-lenient comparator
    # pass the conformance assertions vacuously, the same way a weakened key rule would.
    if json_equal("2026-01-15T08:30:00Z", "2026-01-15T09:30:00Z"):
        fail("comparator treated two different instants as equal")
    if json_equal("admin", "guest"):
        fail("comparator treated two different non-datetime strings as equal")

    app = FastAPI()
    app.include_router(create_router(Stub()))
    rec = Recording(httpx.ASGITransport(app=app))
    client = ApiClient("http://test")
    # Close the default client created in `ApiClient.__init__` before swapping in the
    # recording one, so neither leaks (would emit a ResourceWarning otherwise).
    await client.client.aclose()
    client.client = httpx.AsyncClient(transport=rec, base_url="http://test")
    try:
        # createAccount: request body the client sends + response body the server emits.
        await client.create_account(m.CreateAccountBody(**ACCT))
        if rec.req is None:
            fail("createAccount: no request captured")
        create_spec = GOLDEN["createAccountRequest"]
        if rec.req.method != create_spec["method"]:
            fail(f"createAccount method: {rec.req.method!r} != {create_spec['method']!r}")
        if rec.req.url.path != create_spec["path"]:
            fail(f"createAccount path: {rec.req.url.path!r} != {create_spec['path']!r}")
        assert_wire("createAccount request body", rec.req.content, GOLDEN["account"])
        assert_wire("createAccount response body", rec.resp_body, GOLDEN["account"])

        # getAccount: param wire (path / repeated-key query / aliased header) + response.
        await client.get_account(
            "acc-7", include_archived=True, roles=[m.Role.ADMIN, m.Role.GUEST], request_id="req-1"
        )
        if rec.req is None:
            fail("getAccount: no request captured")
        spec = GOLDEN["getAccountRequest"]
        if rec.req.method != spec["method"]:
            fail(f"getAccount method: {rec.req.method!r} != {spec['method']!r}")
        if rec.req.url.path != spec["path"]:
            fail(f"getAccount path: {rec.req.url.path!r} != {spec['path']!r}")
        assert_query("getAccount", rec.req, spec["query"])
        for k, v in spec["headers"].items():
            if rec.req.headers.get(k) != v:
                fail(f"getAccount header {k}: {rec.req.headers.get(k)!r} != {v!r}")
        assert_wire("getAccount response body", rec.resp_body, GOLDEN["account"])

        # listAccounts: the request query (`page`) plus the pagination envelope wire
        # ({ items, totalCount }).
        await client.list_accounts(page=2)
        if rec.req is None:
            fail("listAccounts: no request captured")
        list_spec = GOLDEN["listAccountsRequest"]
        if rec.req.method != list_spec["method"]:
            fail(f"listAccounts method: {rec.req.method!r} != {list_spec['method']!r}")
        if rec.req.url.path != list_spec["path"]:
            fail(f"listAccounts path: {rec.req.url.path!r} != {list_spec['path']!r}")
        assert_query("listAccounts", rec.req, list_spec["query"])
        assert_wire("listAccounts response body", rec.resp_body, GOLDEN["page"])
    finally:
        await client.client.aclose()

    print("OK")


asyncio.run(main())
