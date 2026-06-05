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
from fastapi import FastAPI

from generated import models as m
from generated.client import ApiClient
from generated.server import create_router

# The constraint case builds an invalid body via `model_construct` (bypassing
# pydantic validation on purpose); dumping that unvalidated model emits a benign
# "Expected enum but got str" serializer warning. Silence it so the only output
# is contract pass/fail signal.
import warnings  # noqa: E402

warnings.filterwarnings("ignore", message="Pydantic serializer warnings")

TARGET = "python"


class DriverError(Exception):
    """A contract assertion failed."""


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
            "status": _enum_value(body.status),
            "tags": body.tags,
        }
        self._maybe_raise()
        return m.Post(**self._returns())

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


def _enum_value(v: Any) -> Any:
    """The status field is a PostStatus enum; expose its string value so it can
    be compared numerically/stringly against the contract."""
    return v.value if hasattr(v, "value") else v


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

    if endpoint == "updateAuthorProfile":
        raw_body = call["body"]
        # Generated Python models use snake_case field names (no camelCase
        # alias), so map the contract's camelCase body keys before constructing.
        snake_body = {_to_snake(k): v for k, v in raw_body.items()}
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

    raise DriverError(f"driver has no invoke mapping for endpoint {endpoint!r}")


# ── assertions ───────────────────────────────────────────────────────────────


def _to_snake(key: str) -> str:
    out = []
    for ch in key:
        if ch.isupper():
            out.append("_")
            out.append(ch.lower())
        else:
            out.append(ch)
    return "".join(out)


def _normalize(value: Any) -> Any:
    """Recursively snake_case dict keys and coerce enums/bools so the contract's
    camelCase expectations compare equal to the Python generator's snake_case
    wire format. Numbers are left as-is and compared numerically below."""
    if isinstance(value, dict):
        return {_to_snake(k): _normalize(v) for k, v in value.items()}
    if isinstance(value, list):
        return [_normalize(v) for v in value]
    return _enum_value(value)


def _values_equal(want: Any, got: Any) -> bool:
    """Compare a contract value against an observed value. Numbers compare
    numerically (int vs float agnostic); None matches None; containers recurse."""
    if want is None:
        return got is None
    if isinstance(want, bool) or isinstance(got, bool):
        return want == got
    if isinstance(want, (int, float)) and isinstance(got, (int, float)):
        return float(want) == float(got)
    if isinstance(want, dict) and isinstance(got, dict):
        if set(want.keys()) != set(got.keys()):
            return False
        return all(_values_equal(want[k], got[k]) for k in want)
    if isinstance(want, list) and isinstance(got, list):
        return len(want) == len(got) and all(
            _values_equal(w, g) for w, g in zip(want, got)
        )
    return want == got


def assert_received(case: dict[str, Any], got: dict[str, Any] | None) -> None:
    expected = case["handler"].get("expect_received")
    if expected is None:
        return
    if got is None:
        raise DriverError(f"[{case['name']}] handler recorded no args")
    for key, want in expected.items():
        if key not in got:
            raise DriverError(
                f"[{case['name']}] handler did not receive arg {key!r}"
            )
        if not _values_equal(want, got[key]):
            raise DriverError(
                f"[{case['name']}] handler arg {key!r}: got {got[key]!r}, "
                f"want {want!r}"
            )


def _result_to_plain(result: Any) -> Any:
    """Convert the client's typed result (pydantic model or list of models) to
    plain JSON-able data with snake_case keys."""
    if isinstance(result, list):
        return [item.model_dump(mode="json") for item in result]
    return result.model_dump(mode="json")


def assert_ok(case: dict[str, Any], result: Any) -> None:
    want = _normalize(case["expect_client"]["ok"])
    got = _result_to_plain(result)
    if not _values_equal(want, got):
        raise DriverError(
            f"[{case['name']}] client result mismatch:\n"
            f" got: {json.dumps(got)}\n"
            f"want: {json.dumps(want)}"
        )


def assert_ok_headers(case: dict[str, Any], result: Any) -> None:
    """Compare the response-header fields the client read off the typed envelope
    against expect_client.ok_headers. The contract uses camelCase header keys
    (ratelimitRemaining, etag); the generated Python envelope exposes snake_case
    attributes (ratelimit_remaining, etag). ratelimitRemaining is a required int
    compared numerically; etag is an optional string that must equal the expected
    value or be None when the contract value is JSON null."""
    want = case["expect_client"].get("ok_headers")
    if not want:
        return
    for key, expected in want.items():
        if key == "ratelimitRemaining":
            got = result.ratelimit_remaining
        elif key == "etag":
            got = result.etag
        else:
            raise DriverError(f"[{case['name']}] unknown ok_header {key!r}")
        if not _values_equal(expected, got):
            raise DriverError(
                f"[{case['name']}] ok_header {key!r}: got {got!r}, "
                f"want {expected!r}"
            )


def assert_error_status(case: dict[str, Any], err: Exception | None) -> None:
    if err is None:
        raise DriverError(f"[{case['name']}] expected an error, got success")
    if not isinstance(err, httpx.HTTPStatusError):
        raise DriverError(
            f"[{case['name']}] expected httpx.HTTPStatusError, got "
            f"{type(err).__name__}: {err}"
        )
    expect_error = case["expect_client"].get("error")
    if expect_error is None:
        raise DriverError(f"[{case['name']}] case has no expect_client.error")
    want_status = expect_error["status_per_target"].get(TARGET)
    if want_status is None:
        raise DriverError(
            f"[{case['name']}] no status_per_target[{TARGET!r}]"
        )
    got_status = err.response.status_code
    if got_status != want_status:
        raise DriverError(
            f"[{case['name']}] error status: got {got_status}, "
            f"want {want_status}"
        )


# ── driver ───────────────────────────────────────────────────────────────────


async def run_case(case: dict[str, Any]) -> None:
    stub = Stub(case)
    app = FastAPI()
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
        if not stub.hit:
            raise DriverError(
                f"[{case['name']}] handler was never called for ok case"
            )
        # Header endpoints return a typed envelope (body + response headers);
        # compare the body against expect_client.ok and the response headers
        # against expect_client.ok_headers separately.
        if "ok_headers" in case["expect_client"]:
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
