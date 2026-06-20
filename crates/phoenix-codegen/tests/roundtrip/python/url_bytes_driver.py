# Behavioral Url/Bytes round-trip driver for the Python target.
#
# Committed source. The Rust harness (`roundtrip.rs::url_bytes_python_roundtrip`)
# generates the small Url/Bytes schema into ./generated_url_bytes/ (separate from
# the other round-trips' packages, so they never race), then runs this with the
# committed `.venv`. It drives the generated httpx client in-process against the
# generated FastAPI server via `httpx.ASGITransport` and proves:
#   - `Bytes` is a first-class binary value: a `Bytes` field constructed from raw
#     `bytes` (NOT base64) survives the wire as the SAME raw bytes — including
#     non-UTF-8 bytes — across a required field, a present/absent `Option`, a
#     `List`, and a `Map<String, Bytes>` (the alias applied to dict values), in both
#     the request body and the echoed response. The `Bytes` alias base64-encodes on
#     send (`model_dump(mode="json")` → `_bytes_to_b64`) and decodes on receive
#     (`_bytes_from_b64`); the handler asserts it sees real `bytes`, never the
#     base64 string.
#   - `Url` is a validated string that round-trips byte-for-byte (never normalized):
#     through a body field / `Option` / `List`, a query param, a `List<Url>` query
#     param, and a request header — all echoed into the response.
#   - a MULTI-STATUS endpoint (`response { }` block) round-trips a Bytes-bearing
#     shared body through the { status, body } envelope.
#   - the reject path: a malformed `Url` query value fails FastAPI's `BeforeValidator`
#     (422).
# Exits non-zero on failure.

from __future__ import annotations

import asyncio
import sys

import httpx
from fastapi import FastAPI

from generated_url_bytes import models as m
from generated_url_bytes.client import ApiClient
from generated_url_bytes.server import create_router

# Raw binary with non-UTF-8 bytes (0x00, 0xFF, 0xFE, 0x80): a base64 wire that
# decoded wrong, or a UTF-8 round-trip, would corrupt these.
CHECKSUM = bytes([0x00, 0x01, 0xFF, 0xFE, 0x80])
SIGNATURE = bytes([0xCA, 0xFE, 0x00, 0xBA, 0xBE])
CHUNK_A = bytes([0xDE, 0xAD])
CHUNK_B = bytes([0xBE, 0xEF, 0x00])
# A Map<String, Bytes> — each value must survive the base64 wire as raw bytes.
TAGS = {"a": CHUNK_A, "b": CHUNK_B}

# URLs with query string + fragment + a non-lowercased host, to prove the value is
# preserved verbatim (validated, not normalized).
SOURCE = "https://Example.com/a/b?x=1&y=2#frag"
MIRROR = "ftp://mirror.example.org/pub/file.bin"
THUMB_A = "https://t.example/1.png"
THUMB_B = "https://t.example/2.png"
ORIGIN = "https://origin.example.com/in"
MIRROR_Q_A = "https://m1.example.com"
MIRROR_Q_B = "https://m2.example.com"
REFERER = "https://ref.example.com/page?from=test"


def fail(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


class Stub:
    async def upload(
        self,
        body: m.UploadBody,
        *,
        origin: m.Url,
        mirrors: list[m.Url],
        referer: m.Url,
    ) -> m.Echo:
        # The body's `Bytes` fields must reach the handler as real `bytes` (decoded
        # from base64 server-side), never the wire string — that is the whole point
        # of a first-class binary value.
        if not isinstance(body.checksum, bytes):
            fail(f"server: body.checksum is {type(body.checksum)}, not bytes")
        if body.signature is not None and not isinstance(body.signature, bytes):
            fail(f"server: body.signature is {type(body.signature)}, not bytes")
        for c in body.chunks:
            if not isinstance(c, bytes):
                fail(f"server: chunk is {type(c)}, not bytes")
        for v in body.tags.values():
            if not isinstance(v, bytes):
                fail(f"server: tag value is {type(v)}, not bytes")
        return m.Echo(
            source=body.source,
            mirror=body.mirror,
            thumbnails=body.thumbnails,
            checksum=body.checksum,
            signature=body.signature,
            chunks=body.chunks,
            tags=body.tags,
            origin=origin,
            mirrors=mirrors,
            referer=referer,
        )

    async def replace(self, id: str, body: m.ReplaceBody) -> m.ReplaceResponse:
        # MULTI-STATUS endpoint: the handler echoes the Bytes-bearing shared body
        # into the ReplaceResponse envelope { status, body } and picks status 200.
        # The generated server writes that status and the client reads the envelope
        # back — proving the Bytes/Map round-trip survives the multi-status path.
        if not isinstance(body.checksum, bytes):
            fail(f"server(replace): body.checksum is {type(body.checksum)}, not bytes")
        return m.ReplaceResponse(
            status=200,
            body=m.Payload(
                source=body.source,
                mirror=body.mirror,
                thumbnails=body.thumbnails,
                checksum=body.checksum,
                signature=body.signature,
                chunks=body.chunks,
                tags=body.tags,
            ),
        )


async def main() -> None:
    app = FastAPI()
    app.include_router(create_router(Stub()))
    client = ApiClient("http://test")
    client.client = httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    )

    # Call 1: optional fields present.
    echo = await client.upload(
        m.UploadBody(
            source=SOURCE,
            mirror=MIRROR,
            thumbnails=[THUMB_A, THUMB_B],
            checksum=CHECKSUM,
            signature=SIGNATURE,
            chunks=[CHUNK_A, CHUNK_B],
            tags=TAGS,
        ),
        origin=ORIGIN,
        mirrors=[MIRROR_Q_A, MIRROR_Q_B],
        referer=REFERER,
    )

    # Bytes round-trip as raw binary (identical bytes, non-UTF-8 intact).
    if echo.checksum != CHECKSUM:
        fail(f"checksum: {echo.checksum!r} != {CHECKSUM!r}")
    if echo.signature != SIGNATURE:
        fail(f"signature: {echo.signature!r} != {SIGNATURE!r}")
    if echo.chunks != [CHUNK_A, CHUNK_B]:
        fail(f"chunks: {echo.chunks!r}")
    # Map<String, Bytes> round-trips as raw binary per value.
    if echo.tags != TAGS:
        fail(f"tags: {echo.tags!r} != {TAGS!r}")
    # Url round-trips byte-for-byte (no normalization).
    if echo.source != SOURCE:
        fail(f"source: {echo.source!r} != {SOURCE!r}")
    if echo.mirror != MIRROR:
        fail(f"mirror: {echo.mirror!r} != {MIRROR!r}")
    if echo.thumbnails != [THUMB_A, THUMB_B]:
        fail(f"thumbnails: {echo.thumbnails!r}")
    # Url query / List<Url> query / Url header round-trip.
    if echo.origin != ORIGIN:
        fail(f"origin query: {echo.origin!r} != {ORIGIN!r}")
    if echo.mirrors != [MIRROR_Q_A, MIRROR_Q_B]:
        fail(f"mirrors query: {echo.mirrors!r}")
    if echo.referer != REFERER:
        fail(f"referer header: {echo.referer!r} != {REFERER!r}")

    # Call 2: optional Bytes/Url absent, empty lists.
    echo2 = await client.upload(
        m.UploadBody(
            source=SOURCE,
            mirror=None,
            thumbnails=[],
            checksum=CHECKSUM,
            signature=None,
            chunks=[],
            tags={},
        ),
        origin=ORIGIN,
        mirrors=[],
        referer=REFERER,
    )
    if echo2.mirror is not None:
        fail(f"absent mirror came back as {echo2.mirror!r}")
    if echo2.signature is not None:
        fail(f"absent signature came back as {echo2.signature!r}")
    if echo2.thumbnails or echo2.chunks or echo2.mirrors or echo2.tags:
        fail(f"empty lists/map not empty: {echo2}")
    if echo2.checksum != CHECKSUM:
        fail(f"checksum (call 2): {echo2.checksum!r}")

    # Multi-status endpoint: the shared Payload body (carrying Bytes + the
    # Map<String, Bytes>) round-trips through the { status, body } envelope. The
    # server writes the chosen status and the client reads the envelope back, so the
    # binary must survive identically here too.
    rep = await client.replace(
        "asset-1",
        m.ReplaceBody(
            source=SOURCE,
            mirror=MIRROR,
            thumbnails=[THUMB_A, THUMB_B],
            checksum=CHECKSUM,
            signature=SIGNATURE,
            chunks=[CHUNK_A, CHUNK_B],
            tags=TAGS,
        ),
    )
    if rep.status != 200:
        fail(f"replace status: {rep.status} != 200")
    if rep.body is None:
        fail("replace envelope body is None")
    elif rep.body.checksum != CHECKSUM:
        fail(f"replace checksum: {rep.body.checksum!r} != {CHECKSUM!r}")
    elif rep.body.tags != TAGS:
        fail(f"replace tags: {rep.body.tags!r} != {TAGS!r}")
    elif rep.body.source != SOURCE:
        fail(f"replace source: {rep.body.source!r} != {SOURCE!r}")

    # Reject path: a malformed Url query value must fail FastAPI's BeforeValidator
    # with exactly 422 (pin the code so a 500 regression can't pass). Issue a raw
    # POST so the server-side parse is what fails.
    bad = await client.client.post(
        "/assets",
        params={"origin": "not-a-url", "mirrors": []},
        headers={"X-Referer": REFERER},
        json={
            "source": SOURCE,
            "thumbnails": [],
            "checksum": "",
            "chunks": [],
            "tags": {},
        },
    )
    if bad.status_code != 422:
        fail(f"malformed url query: HTTP {bad.status_code}, want 422")

    # Reject path (List<Url> element): a malformed element in the `mirrors`
    # `List<Url>` query must fail the per-element BeforeValidator (422). Origin is
    # valid here, so the bad element is the only thing that can be rejected.
    bad_elem = await client.client.post(
        "/assets",
        params={"origin": ORIGIN, "mirrors": [MIRROR_Q_A, "not-a-url"]},
        headers={"X-Referer": REFERER},
        json={
            "source": SOURCE,
            "thumbnails": [],
            "checksum": "",
            "chunks": [],
            "tags": {},
        },
    )
    if bad_elem.status_code != 422:
        fail(f"malformed List<Url> query element: HTTP {bad_elem.status_code}, want 422")

    # Reject path (body): a malformed Url in a BODY field must fail FastAPI's
    # BeforeValidator on the body model (422). The query is valid here, so the body
    # field is the only thing that can be rejected.
    bad_body = await client.client.post(
        "/assets",
        params={"origin": ORIGIN, "mirrors": []},
        headers={"X-Referer": REFERER},
        json={
            "source": "not-a-url",
            "thumbnails": [],
            "checksum": "",
            "chunks": [],
            "tags": {},
        },
    )
    if bad_body.status_code != 422:
        fail(f"malformed url body field: HTTP {bad_body.status_code}, want 422")

    print("OK")


asyncio.run(main())
