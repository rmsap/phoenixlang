# Behavioral round-trip driver for the Python target.
#
# This script is committed source. The Rust harness
# (`crates/phoenix-codegen/tests/roundtrip.rs`) assembles a working dir at test
# time containing:
#   - the generated package in ./generated/ (an importable package:
#     generated/{__init__,models,client,handlers,server}.py — the generated
#     files import each other relatively, e.g. `from .models import ...`),
#   - this file (driver.py),
#   - contract.json (copied in next to this file).
# It then runs `.venv/bin/python driver.py` from that dir and asserts exit 0.
#
# It is a plain script (NOT pytest) that exits non-zero on the first failure —
# this avoids a pytest / pytest-asyncio dependency. It drives the generated
# httpx client IN-PROCESS against the generated FastAPI server via
# `httpx.ASGITransport` (no real port): the generated `ApiClient` builds its own
# `httpx.AsyncClient`, so after constructing it we swap in an AsyncClient bound
# to the ASGI transport.
#
# For each case in contract.json it:
#   1. builds a fixture-driven stub implementing the generated `Handlers`
#      Protocol (records the decoded args it received, returns the canned
#      success value, or raises `Exception("<variant>")` so the server's
#      `str(e) == "<variant>"` mapping fires);
#   2. mounts `create_router(stub)` on a fresh `FastAPI()` app and points the
#      generated `ApiClient` at it over the ASGI transport;
#   3. invokes the matching client method with the case inputs and asserts:
#      (a) the handler received exactly the expected decoded inputs, and
#      (b) for ok cases the client's observed result equals expect_client.ok;
#          for error/constraint cases the client raised an
#          `httpx.HTTPStatusError` whose `.response.status_code` equals the
#          expected per-target status. Constraint cases additionally assert the
#          handler was NOT called (server rejected the invalid body at parse
#          time → FastAPI 422).
#
# ── Surface notes (verified against the generated code) ─────────────────────
#   * Handler methods are async, snake_case, with query params keyword-only.
#   * Generated models use snake_case field names with NO camelCase alias, so
#     the wire/JSON representation is snake_case (`avatar_url`, not
#     `avatarUrl`). The shared contract is camelCase. assert_ok therefore
#     normalizes BOTH sides to snake_case keys before comparing — a documented
#     divergence between the Python generator's wire format and the contract's
#     camelCase, not a round-trip bug (the Python client and server agree).
#   * The constraint case's invalid body cannot be built via the normal pydantic
#     constructor (it would raise client-side before any request). We use
#     `Model.model_construct(...)` to bypass client-side validation so the
#     invalid body actually travels through the generated client to the server,
#     which rejects it with 422 (FastAPI's pydantic body-validation default).

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path
from typing import Any

import httpx
from fastapi import FastAPI, Response

from generated import models as m
from generated.client import ApiClient
from generated.server import create_router

from harness import (
    DriverError,
    assert_download,
    assert_error_status,
    assert_multi_status,
    assert_ok,
    assert_ok_headers,
    assert_received,
    enum_value,
    normalize,
    to_snake,
)

# The constraint case builds an invalid body via `model_construct` (bypassing
# pydantic validation on purpose); dumping that unvalidated model emits a benign
# "Expected enum but got str" serializer warning. Silence it so the only output
# is contract pass/fail signal.
import warnings  # noqa: E402

warnings.filterwarnings("ignore", message="Pydantic serializer warnings")


# ── fixture-driven stub ──────────────────────────────────────────────────────


class Stub:
    """Implements the generated `Handlers` Protocol. Only the endpoints the
    contract exercises are wired with assertions; any other route raises so an
    unexpected call is loud. The active case is set per-iteration."""

    def __init__(self, case: dict[str, Any]) -> None:
        self.case = case
        self.hit = False
        self.received: dict[str, Any] | None = None

    def _maybe_raise(self) -> None:
        raises = self.case["handler"].get("raises")
        if raises:
            raise Exception(raises)

    def _returns(self) -> Any:
        return self.case["handler"]["returns"]

    async def list_posts(
        self,
        *,
        page: int,
        limit: int,
        tag: str | None = None,
        search: str | None = None,
        featured: bool,
        min_score: float,
        max_score: float | None = None,
    ) -> list[m.Post]:
        self.hit = True
        self.received = {
            "page": page,
            "limit": limit,
            "tag": tag,
            "search": search,
            "featured": featured,
            "minScore": min_score,
            "maxScore": max_score,
        }
        self._maybe_raise()
        return [m.Post(**item) for item in self._returns()]

    async def search_posts(
        self, *, max_results: int, sort_field: str
    ) -> list[m.Post]:
        self.hit = True
        self.received = {
            "maxResults": max_results,
            "sortField": sort_field,
        }
        self._maybe_raise()
        return [m.Post(**item) for item in self._returns()]

    async def list_tagged_posts(self, tag: str, *, limit: int) -> list[m.Post]:
        # Versioned endpoint (/v2/api/posts/tagged/{tag}). The contract's
        # expect_received keys are already snake_case (tag/limit).
        self.hit = True
        self.received = {
            "tag": tag,
            "limit": limit,
        }
        self._maybe_raise()
        return [m.Post(**item) for item in self._returns()]

    async def list_posts_offset(
        self, *, page: int, limit: int
    ) -> m.ListPostsOffsetPage:
        # Offset pagination: the response is the ListPostsOffsetPage envelope
        # ({ items, total_count }), not a bare list. The contract's
        # expect_received keys (page/limit) are already snake_case. The contract's
        # returns is the full page object using camelCase wire keys (items /
        # totalCount, with nested Post.avatarUrl); the generated model uses
        # snake_case fields with no alias, so recursively snake_case the returns
        # dict (same divergence handled in upload_avatar / update_author_profile)
        # before model_validate builds the page model.
        self.hit = True
        self.received = {"page": page, "limit": limit}
        self._maybe_raise()
        return m.ListPostsOffsetPage.model_validate(normalize(self._returns()))

    async def list_posts_cursor(
        self, *, cursor: str | None = None, limit: int
    ) -> m.ListPostsCursorPage:
        # Cursor pagination: the response is the ListPostsCursorPage envelope
        # ({ items, next_cursor? }). Same camelCase→snake_case normalization of
        # the contract's returns as list_posts_offset (nextCursor → next_cursor).
        self.hit = True
        self.received = {"cursor": cursor, "limit": limit}
        self._maybe_raise()
        return m.ListPostsCursorPage.model_validate(normalize(self._returns()))

    async def get_post(self, id: str) -> m.Post:
        self.hit = True
        self.received = {"id": id}
        self._maybe_raise()
        return m.Post(**self._returns())

    async def create_post(self, body: m.CreatePostBody) -> m.Post:
        self.hit = True
        self.received = {
            "title": body.title,
            "body": body.body,
            "status": enum_value(body.status),
            "tags": body.tags,
        }
        self._maybe_raise()
        return m.Post(**self._returns())

    async def upsert_post2(
        self, id: str, body: m.UpsertPost2Body
    ) -> m.UpsertPost2Response:
        # Multi-status endpoint: the handler returns an UpsertPost2Response
        # envelope { status, body }. The stub sets the status from
        # handler.returns_status; the generated server writes that status to the
        # wire. body is a Post built from the contract's camelCase `returns` when
        # present (REUSE normalize to map camelCase→snake_case, since the Post
        # model uses snake_case fields with no alias), else None (the 204 case).
        self.hit = True
        self.received = {
            "id": id,
            "title": body.title,
            "body": body.body,
            "status": enum_value(body.status),
            "tags": body.tags,
        }
        self._maybe_raise()
        returns_status = self.case["handler"]["returns_status"]
        returns = self.case["handler"].get("returns")
        post = m.Post.model_validate(normalize(returns)) if returns is not None else None
        return m.UpsertPost2Response(status=returns_status, body=post)

    async def requeue_post(self, id: str) -> m.RequeuePostResponse:
        # ALL-TYPELESS multi-status endpoint: the RequeuePostResponse envelope
        # has no body field at all — the stub only chooses the status from
        # handler.returns_status.
        self.hit = True
        self.received = {"id": id}
        self._maybe_raise()
        return m.RequeuePostResponse(status=self.case["handler"]["returns_status"])

    async def update_author_profile(
        self, id: str, body: m.UpdateAuthorProfileBody
    ) -> m.Author:
        # Body carries a constrained Option<String> field (avatar_url) that is
        # also `partial`-applied. The constraint case sends an empty avatarUrl,
        # so pydantic rejects it server-side before this runs — args recorded
        # for completeness only.
        self.hit = True
        self.received = {
            "id": id,
            "name": body.name,
            "avatarUrl": body.avatar_url,
        }
        self._maybe_raise()
        return m.Author(**self._returns())

    async def get_post_metered(
        self,
        id: str,
        *,
        authorization: str,
        request_id: str,
        if_none_match: str | None = None,
        max_stale: int,
    ) -> m.GetPostMeteredResult:
        # Request headers reach the handler as ordinary keyword args, asserted
        # via expect_received exactly like path/query params. The contract keys
        # are camelCase (authorization, requestId, ifNoneMatch, maxStale); record
        # them under those names so the existing snake-agnostic comparison lines
        # up — _values_equal compares the contract's camelCase keys against the
        # keys recorded here, so we record using the SAME camelCase the contract
        # uses (requestId/ifNoneMatch/maxStale) rather than the Python snake_case
        # param names.
        self.hit = True
        self.received = {
            "id": id,
            "authorization": authorization,
            "requestId": request_id,
            "ifNoneMatch": if_none_match,
            "maxStale": max_stale,
        }
        self._maybe_raise()
        returns_headers = self.case["handler"].get("returns_headers", {})
        return m.GetPostMeteredResult(
            body=m.Post(**self._returns()),
            ratelimit_remaining=returns_headers["ratelimitRemaining"],
            etag=returns_headers.get("etag"),
        )

    async def upload_avatar(
        self,
        id: str,
        avatar: Any,
        caption: str,
        rotation: int,
        crop: bool,
        thumbnail: Any | None = None,
    ) -> m.Author:
        # avatar / thumbnail arrive as FastAPI UploadFile objects (the server
        # parses the multipart body). Read each file's bytes asynchronously and
        # decode to UTF-8 for the contract comparison; record the filename too.
        # The scalar fields (caption/rotation/crop) arrive already coerced to
        # their declared types by FastAPI's Form(...) binding — rotation as int,
        # crop as bool — which is exactly what we assert against.
        # The contract's expect_received keys for files are already snake_case
        # (avatar_content / avatar_filename / thumbnail_content), matched
        # literally by the existing snake-agnostic comparison.
        self.hit = True
        avatar_bytes = await avatar.read()
        thumbnail_content = None
        thumbnail_filename = None
        if thumbnail is not None:
            thumbnail_content = (await thumbnail.read()).decode()
            thumbnail_filename = thumbnail.filename
        self.received = {
            "id": id,
            "avatar_content": avatar_bytes.decode(),
            "avatar_filename": avatar.filename,
            "caption": caption,
            "rotation": rotation,
            "crop": crop,
            "thumbnail_content": thumbnail_content,
            "thumbnail_filename": thumbnail_filename,
        }
        self._maybe_raise()
        # The contract's returns uses camelCase keys (avatarUrl); the generated
        # Author model uses snake_case fields with no alias, so map keys before
        # constructing (same divergence handled in updateAuthorProfile's invoke).
        returns = {to_snake(k): v for k, v in self._returns().items()}
        return m.Author(**returns)

    async def download_avatar(self, id: str) -> bytes:
        # Binary download: stream back the contract's returns_file as raw bytes.
        self.hit = True
        self.received = {"id": id}
        self._maybe_raise()
        return self.case["handler"]["returns_file"].encode()

    # sync_catalog exercises the composite shapes (Map / List<enum> / nested
    # List<struct>). The stub echoes the decoded body into the Catalog response,
    # so assert_ok's deep compare validates the full round-trip — no per-field
    # expect_received needed. body is already a parsed SyncCatalogBody, so its
    # fields copy straight across (labels: dict, allowed_statuses: list[enum],
    # entries: list[CatalogEntry]).
    async def sync_catalog(self, body: m.SyncCatalogBody) -> m.Catalog:
        self.hit = True
        self._maybe_raise()
        return m.Catalog(
            id=body.id,
            labels=body.labels,
            allowed_statuses=body.allowed_statuses,
            entries=body.entries,
        )

    # Unused endpoints — present to satisfy the Protocol; loud if ever routed.
    async def update_post(self, id: str, body: m.UpdatePostBody) -> m.Post:
        raise AssertionError("unexpected call to update_post")

    async def patch_post(self, id: str, body: m.PatchPostBody) -> m.Post:
        raise AssertionError("unexpected call to patch_post")

    async def delete_post(self, id: str) -> None:
        raise AssertionError("unexpected call to delete_post")

    async def list_comments(
        self, post_id: str, *, page: int, limit: int
    ) -> list[m.Comment]:
        raise AssertionError("unexpected call to list_comments")

    async def create_comment(
        self, post_id: str, body: m.CreateCommentBody
    ) -> m.Comment:
        raise AssertionError("unexpected call to create_comment")

    async def get_author_profile(self, id: str) -> m.Author:
        raise AssertionError("unexpected call to get_author_profile")


# ── invoke (mirror of the Go driver's `invoke`) ──────────────────────────────


async def invoke(client: ApiClient, case: dict[str, Any]) -> Any:
    """Calls the matching client method, returning its decoded result.
    Raises httpx.HTTPStatusError on a non-2xx response (the generated client
    calls response.raise_for_status())."""
    endpoint = case["endpoint"]
    call = case.get("call", {})

    if endpoint == "getPost":
        return await client.get_post(call["path_params"]["id"])

    if endpoint == "searchPosts":
        q = call.get("query", {})
        return await client.search_posts(
            max_results=q["maxResults"], sort_field=q["sortField"]
        )

    if endpoint == "listPosts":
        q = call.get("query", {})
        kwargs: dict[str, Any] = {}
        # Pass only the params present in the fixture; the client supplies its
        # own declared defaults for the rest (matching the Go driver, which
        # uses the client's defaults for omitted query params).
        for name in ("page", "limit", "tag", "search", "featured"):
            if name in q and q[name] is not None:
                kwargs[name] = q[name]
        if "minScore" in q and q["minScore"] is not None:
            kwargs["min_score"] = q["minScore"]
        if "maxScore" in q and q["maxScore"] is not None:
            kwargs["max_score"] = q["maxScore"]
        return await client.list_posts(**kwargs)

    if endpoint == "createPost":
        raw_body = call["body"]
        if case["kind"] == "constraint":
            # Bypass client-side pydantic validation so the invalid body reaches
            # the server, which rejects it with 422 before the handler runs.
            body = m.CreatePostBody.model_construct(**raw_body)
        else:
            body = m.CreatePostBody(**raw_body)
        return await client.create_post(body)

    if endpoint == "upsertPost2":
        # Multi-status endpoint. Build the body model from the contract's body
        # (its keys — title/body/status/tags — are already snake_case-compatible,
        # like createPost), call the client, then assert the dynamic status the
        # client read off the envelope plus the optional body.
        raw_body = call["body"]
        body = m.UpsertPost2Body(**raw_body)
        return await client.upsert_post2(call["path_params"]["id"], body)

    if endpoint == "requeuePost":
        # All-typeless multi-status endpoint: no request body, just the path
        # param; the client returns the status-only envelope.
        return await client.requeue_post(call["path_params"]["id"])

    if endpoint == "updateAuthorProfile":
        raw_body = call["body"]
        # Generated Python models use snake_case field names (no camelCase
        # alias), so map the contract's camelCase body keys before constructing.
        snake_body = {to_snake(k): v for k, v in raw_body.items()}
        if case["kind"] == "constraint":
            # Bypass client-side pydantic validation so the invalid body reaches
            # the server, which rejects it with 422 before the handler runs.
            body = m.UpdateAuthorProfileBody.model_construct(**snake_body)
        else:
            body = m.UpdateAuthorProfileBody(**snake_body)
        return await client.update_author_profile(
            call["path_params"]["id"], body
        )

    if endpoint == "getPostMetered":
        headers = call.get("headers", {})
        kwargs: dict[str, Any] = {
            "authorization": headers["authorization"],
            "request_id": headers["requestId"],
            # maxStale is a defaulted request header; both cases supply it.
            "max_stale": headers["maxStale"],
        }
        # ifNoneMatch is optional — pass it only when present (else the client's
        # `| None = None` default applies and the header is omitted on the wire).
        if "ifNoneMatch" in headers and headers["ifNoneMatch"] is not None:
            kwargs["if_none_match"] = headers["ifNoneMatch"]
        return await client.get_post_metered(call["path_params"]["id"], **kwargs)

    if endpoint == "uploadAvatar":
        multipart = call["multipart"]
        files = multipart.get("files", {})
        fields = multipart.get("fields", {})
        # The generated client's file params are typed `m.FileUpload` (filename +
        # content bytes); it forwards `(upload.filename, upload.content)` into
        # httpx's `files=` mapping, so the contract filename travels on the wire
        # and reaches the handler as avatar.filename. Scalar form fields are
        # passed as plain kwargs.
        # Scalar fields arrive JSON-typed (caption str, rotation int, crop bool)
        # and are passed straight through to the typed client params; the client
        # stringifies them into httpx's `data=` form mapping.
        kwargs: dict[str, Any] = {
            "caption": fields["caption"],
            "rotation": fields["rotation"],
            "crop": fields["crop"],
        }
        avatar = files["avatar"]
        kwargs["avatar"] = m.FileUpload(
            filename=avatar["filename"], content=avatar["content"].encode()
        )
        if "thumbnail" in files:
            thumb = files["thumbnail"]
            kwargs["thumbnail"] = m.FileUpload(
                filename=thumb["filename"], content=thumb["content"].encode()
            )
        return await client.upload_avatar(call["path_params"]["id"], **kwargs)

    if endpoint == "listTaggedPosts":
        q = call.get("query", {})
        kwargs: dict[str, Any] = {}
        if "limit" in q and q["limit"] is not None:
            kwargs["limit"] = q["limit"]
        return await client.list_tagged_posts(call["path_params"]["tag"], **kwargs)

    if endpoint == "listPostsOffset":
        q = call.get("query", {})
        # Offset pagination query params (page/limit). The client returns a typed
        # ListPostsOffsetPage envelope; assert_ok normalizes camel/snake and
        # deep-compares the whole page (incl. total_count) against expect_client.ok.
        return await client.list_posts_offset(page=q["page"], limit=q["limit"])

    if endpoint == "listPostsCursor":
        q = call.get("query", {})
        kwargs: dict[str, Any] = {"limit": q["limit"]}
        # cursor is optional (Option<String>); pass it only when present so the
        # client's `| None = None` default applies otherwise.
        if "cursor" in q and q["cursor"] is not None:
            kwargs["cursor"] = q["cursor"]
        return await client.list_posts_cursor(**kwargs)

    if endpoint == "syncCatalog":
        # Composite-shape body: Map (labels), List<enum> (allowedStatuses), and
        # List<struct> as a field (entries). Map only the TOP-LEVEL struct field
        # names camelCase→snake_case (allowedStatuses→allowed_statuses); the
        # nested entry fields (key/status) are single-word and the `labels` VALUE
        # is a data map whose keys must NOT be touched. pydantic coerces the enum
        # strings and nested dicts into the typed model.
        raw_body = {to_snake(k): v for k, v in call["body"].items()}
        body = m.SyncCatalogBody(**raw_body)
        return await client.sync_catalog(body)

    if endpoint == "downloadAvatar":
        return await client.download_avatar(call["path_params"]["id"])

    raise DriverError(f"driver has no invoke mapping for endpoint {endpoint!r}")


# ── driver ───────────────────────────────────────────────────────────────────


async def run_case(case: dict[str, Any]) -> None:
    stub = Stub(case)
    app = FastAPI()
    raw = case.get("raw_response")
    if raw is not None:
        # raw_response case: bypass the generated server entirely and answer
        # every request with the canned status (+ optional JSON body) — the only
        # way to put a status on the wire that the generated server's own guard
        # refuses (e.g. an undeclared 2xx for the client-leniency cases). The
        # stub is never invoked. The body is snake_case-normalized because the
        # Python generator's wire format is snake_case (the same documented
        # divergence the stubs handle for handler.returns).
        async def _raw_catch_all(path: str) -> Response:
            body = raw.get("body")
            if body is None:
                return Response(status_code=raw["status"])
            return Response(
                content=json.dumps(normalize(body)),
                status_code=raw["status"],
                media_type="application/json",
            )

        app.api_route(
            "/{path:path}",
            methods=["GET", "POST", "PUT", "PATCH", "DELETE"],
        )(_raw_catch_all)
    else:
        app.include_router(create_router(stub))

    client = ApiClient("http://test")
    # Swap the generated client's AsyncClient for one bound to the in-process
    # ASGI transport so no real port is needed.
    transport = httpx.ASGITransport(app=app)
    client.client = httpx.AsyncClient(transport=transport, base_url="http://test")

    call_err: Exception | None = None
    result: Any = None
    try:
        result = await invoke(client, case)
    except httpx.HTTPStatusError as e:
        call_err = e
    finally:
        await client.client.aclose()

    # expect_received is checked whenever the handler actually ran.
    if stub.hit:
        assert_received(case, stub.received)

    kind = case["kind"]
    if kind == "ok":
        if call_err is not None:
            raise DriverError(
                f"[{case['name']}] expected success, got error: {call_err}"
            )
        if not stub.hit and raw is None:
            raise DriverError(
                f"[{case['name']}] handler was never called for ok case"
            )
        # Each response-shape key selects a different assertion path, so a
        # contract case carrying more than one would silently assert only the
        # first match — reject the ambiguity loudly instead.
        shape_keys = [
            k
            for k in ("status", "expect_download", "ok_headers")
            if k in case["expect_client"]
        ]
        if len(shape_keys) > 1:
            raise DriverError(
                f"[{case['name']}] expect_client mixes response shapes: "
                f"{shape_keys}"
            )
        # Binary download endpoints return raw bytes (no JSON model); compare
        # the decoded bytes against expect_client.expect_download.
        if "status" in case["expect_client"]:
            assert_multi_status(case, result)
        elif "expect_download" in case["expect_client"]:
            assert_download(case, result)
        # Header endpoints return a typed envelope (body + response headers);
        # compare the body against expect_client.ok and the response headers
        # against expect_client.ok_headers separately.
        elif "ok_headers" in case["expect_client"]:
            assert_ok(case, result.body)
            assert_ok_headers(case, result)
        else:
            assert_ok(case, result)
    elif kind == "error":
        if not stub.hit:
            raise DriverError(
                f"[{case['name']}] handler was never called for error case"
            )
        assert_error_status(case, call_err)
    elif kind == "constraint":
        if case["handler"].get("expect_not_called") and stub.hit:
            raise DriverError(
                f"[{case['name']}] constraint case: handler WAS called but "
                f"should have been rejected server-side"
            )
        assert_error_status(case, call_err)
    else:
        raise DriverError(f"[{case['name']}] unknown case kind {kind!r}")


async def main() -> int:
    contract_path = Path(__file__).parent / "contract.json"
    cases = json.loads(contract_path.read_text())
    if not cases:
        print("contract.json has no cases", file=sys.stderr)
        return 1

    failures = 0
    for case in cases:
        name = case["name"]
        try:
            await run_case(case)
        except DriverError as e:
            failures += 1
            print(f"FAIL {name}: {e}", file=sys.stderr)
        except Exception as e:  # noqa: BLE001 - surface unexpected driver errors
            failures += 1
            print(f"ERROR {name}: {type(e).__name__}: {e}", file=sys.stderr)
        else:
            print(f"ok   {name}")

    if failures:
        print(f"\n{failures} case(s) failed", file=sys.stderr)
        return 1
    print(f"\nall {len(cases)} cases passed")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
