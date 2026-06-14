# Schema-agnostic round-trip boilerplate shared with the per-schema driver.py.
#
# driver.py holds the parts coupled to a specific generated package (the Stub
# implementing the generated Handlers Protocol, the `invoke` dispatch over the
# generated client, and the FastAPI/ASGI run loop). Everything here is generic:
# the target constant, the DriverError exception, the value/key normalization
# helpers, and the assertions that compare a contract case against an observed
# result purely by duck typing (`.model_dump()`, `.value`, `.status`, etc.) — so
# this module imports neither the `generated` package nor pydantic.

from __future__ import annotations

import json
from typing import Any

import httpx

TARGET = "python"


class DriverError(Exception):
    """A contract assertion failed."""


def enum_value(v: Any) -> Any:
    """The status field is a PostStatus enum; expose its string value so it can
    be compared numerically/stringly against the contract."""
    return v.value if hasattr(v, "value") else v


def to_snake(key: str) -> str:
    out = []
    for ch in key:
        if ch.isupper():
            out.append("_")
            out.append(ch.lower())
        else:
            out.append(ch)
    return "".join(out)


def normalize(value: Any) -> Any:
    """Recursively snake_case dict keys and coerce enums/bools so the contract's
    camelCase expectations compare equal to the Python generator's snake_case
    wire format. Numbers are left as-is and compared numerically below. Callers
    that compare two structures apply this to BOTH sides, so the key rewrite
    stays symmetric even for data-map keys this can't distinguish from fields."""
    if isinstance(value, dict):
        return {to_snake(k): normalize(v) for k, v in value.items()}
    if isinstance(value, list):
        return [normalize(v) for v in value]
    return enum_value(value)


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
    # Normalize BOTH sides identically. `normalize` snake_cases every dict key
    # — required for struct fields (contract camelCase → generator snake_case),
    # but it can't tell a struct payload from a Map<String, String> data field,
    # so it would also rewrite map keys. Applying it to `got` as well makes any
    # such rewrite symmetric: a map key like "envName" becomes "env_name" on
    # both sides and still compares equal, so the Map round-trip is validated
    # correctly regardless of the keys' case convention.
    want = normalize(case["expect_client"]["ok"])
    got = normalize(_result_to_plain(result))
    if not _values_equal(want, got):
        raise DriverError(
            f"[{case['name']}] client result mismatch:\n"
            f" got: {json.dumps(got)}\n"
            f"want: {json.dumps(want)}"
        )


def assert_download(case: dict[str, Any], result: Any) -> None:
    """Compare the bytes the client read off a binary response (decoded as
    UTF-8) against expect_client.expect_download."""
    want = case["expect_client"]["expect_download"]
    if not isinstance(result, (bytes, bytearray)):
        raise DriverError(
            f"[{case['name']}] expected bytes from client, got "
            f"{type(result).__name__}"
        )
    got = result.decode()
    if got != want:
        raise DriverError(
            f"[{case['name']}] download mismatch: got {got!r}, want {want!r}"
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


def assert_multi_status(case: dict[str, Any], result: Any) -> None:
    """Multi-status endpoints return an <Endpoint>Response envelope carrying the
    handler-chosen status plus an optional body. Assert the client observed the
    expected status (expect_client.status) and either an absent body
    (expect_client.ok_absent) or a body matching expect_client.ok (compared via
    the same camelCase→snake_case normalization as assert_ok)."""
    expect = case["expect_client"]
    want_status = expect["status"]
    got_status = result.status
    if got_status != want_status:
        raise DriverError(
            f"[{case['name']}] envelope status: got {got_status}, "
            f"want {want_status}"
        )
    # An ALL-TYPELESS envelope (e.g. RequeuePostResponse) has no `body`
    # attribute at all — getattr treats that the same as an absent body.
    result_body = getattr(result, "body", None)
    if expect.get("ok_absent"):
        if result_body is not None:
            raise DriverError(
                f"[{case['name']}] expected absent body, got {result_body!r}"
            )
        return
    assert_ok(case, result_body)


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
