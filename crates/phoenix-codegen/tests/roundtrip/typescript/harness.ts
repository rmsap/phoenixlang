// Schema-agnostic round-trip boilerplate shared with the per-schema driver.ts.
//
// This module holds the parts of the TypeScript behavioral round-trip harness
// that do NOT depend on a particular generated schema: the contract.json case
// shape (`ContractCase` and friends), the `target` constant, and the generic
// assertions/helpers that compare decoded args, error statuses, etc. The
// schema-COUPLED pieces (the `Handlers` stub, the `api.*` client invocation, the
// express/createRouter server mount, and assertions that name generated types)
// live next to it in driver.ts, which imports what it needs from here.

import assert from "node:assert/strict";

import { ApiError } from "./generated/client";

export const target = "typescript" as const;

// ── contract.json schema (mirror of the language-agnostic format) ───────────

export interface ContractCase {
  name: string;
  endpoint: string;
  kind: "ok" | "error" | "constraint";
  call: {
    path_params?: Record<string, string>;
    query?: Record<string, unknown>;
    body?: Record<string, unknown>;
    headers?: Record<string, unknown>;
    // multipart/file-upload body (mutually exclusive with `body`): each file
    // part carries a UTF-8 `content` string the driver wraps in a Blob, plus
    // scalar form `fields`. The generated client builds its own FormData from
    // the typed UploadAvatarBody we construct from this.
    multipart?: {
      files: Record<string, { filename: string; content: string }>;
      fields: Record<string, unknown>;
    };
  };
  // When present, the driver answers every request itself with this canned
  // status (+ optional JSON body) INSTEAD of mounting the generated server —
  // the only way to put a status on the wire that the generated server's own
  // guard refuses (e.g. an undeclared 2xx for the client-leniency cases). The
  // stub handler is never invoked, so the ok-case hit assertion is skipped.
  raw_response?: { status: number; body?: unknown };
  handler: {
    expect_received?: Record<string, unknown>;
    returns?: unknown;
    returns_headers?: Record<string, unknown>;
    // binary download: the UTF-8 content the stub streams back as the raw
    // (non-JSON) response body (returned from the handler as a Buffer).
    returns_file?: string;
    // multi-status: the HTTP status the stub sets on the returned
    // <Endpoint>Response envelope; `returns` (when present) is the body.
    returns_status?: number;
    raises?: string;
    expect_not_called?: boolean;
  };
  expect_client: {
    ok?: unknown;
    ok_headers?: Record<string, unknown>;
    // multi-status: the status the client must observe on the envelope, and
    // (for a bodyless status like 204) that the envelope body is absent.
    status?: number;
    ok_absent?: boolean;
    // binary download: the UTF-8 content the client must read off the Blob
    // response body (present instead of `ok`).
    expect_download?: string;
    error?: {
      variant: string;
      status_per_target: Record<string, number>;
    };
  };
}

// State shared between the stub and the per-case assertions. `hit` records
// whether the relevant handler ran; `received` is the flattened map of decoded
// args the handler saw (compared against expect_received).
export interface CaseState {
  hit: boolean;
  received: Record<string, unknown>;
}

// errOrReturn signals the handler's response: throws an Error carrying the
// variant name (so the server maps it to a status) or returns the canned value.
export function signal<T>(c: ContractCase): T {
  if (c.handler.raises !== undefined) {
    throw new Error(c.handler.raises);
  }
  return c.handler.returns as T;
}

// ── assertions ──────────────────────────────────────────────────────────────

// assertReceived checks every key in expect_received against the decoded args
// the handler actually saw. Numbers compare numerically; only listed keys are
// checked. `null` in the contract means the optional arg was absent.
// UNOBSERVABLE_KEYS: keys present in the contract's expect_received that this
// target structurally cannot observe, so the driver skips their sub-assertion.
// `avatar_filename`: the generated TS server delivers multipart files as
// `Record<string,Blob>`, and a Blob carries no filename — unlike Go's
// multipart.FileHeader, the original filename is lost before it reaches the
// handler. The README explicitly permits a driver to omit checking a key it
// cannot observe ("only listed keys are checked"); the content + caption +
// thumbnail-absent assertions still fully exercise the multipart round-trip.
const UNOBSERVABLE_KEYS = new Set<string>([
  "avatar_filename",
  "thumbnail_filename",
]);

export function assertReceived(c: ContractCase, got: Record<string, unknown>): void {
  const want = c.handler.expect_received;
  if (want === undefined) return;
  for (const [k, w] of Object.entries(want)) {
    if (UNOBSERVABLE_KEYS.has(k)) continue;
    assert.ok(k in got, `[${c.name}] handler did not receive arg ${k}`);
    assert.deepStrictEqual(
      got[k],
      w,
      `[${c.name}] handler arg ${k}: got ${JSON.stringify(got[k])}, want ${JSON.stringify(w)}`,
    );
  }
}

// assertErrorStatus asserts the thrown error is an ApiError whose .status (and
// .code) match the expected per-target values.
export function assertErrorStatus(c: ContractCase, thrown: unknown): void {
  const expect = c.expect_client.error;
  assert.ok(expect !== undefined, `[${c.name}] case has no expect_client.error`);
  const wantStatus = expect.status_per_target[target];
  assert.ok(
    wantStatus !== undefined,
    `[${c.name}] no status_per_target.${target}`,
  );
  assert.ok(
    thrown instanceof ApiError,
    `[${c.name}] expected ApiError, got: ${String(thrown)}`,
  );
  assert.equal(
    thrown.status,
    wantStatus,
    `[${c.name}] error status: got ${String(thrown.status)}, want ${String(wantStatus)}`,
  );
  assert.equal(
    thrown.code,
    expect.variant,
    `[${c.name}] error code: got ${thrown.code}, want ${expect.variant}`,
  );
}
