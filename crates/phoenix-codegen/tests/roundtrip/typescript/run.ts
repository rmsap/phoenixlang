// Framework-independent run loop + result assertions for the round-trip.
//
// This module owns everything about driving the contract.json cases that does
// NOT depend on a particular HTTP server framework: the per-case run loop, the
// ok/error/constraint result assertions, and the top-level `main`. The only
// framework-specific dependency is injected as a `Mount` callback, so an Express
// or Fastify driver supplies just its server "mount" and reuses this loop.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { makeStub, invoke } from "./cases";
import { setBaseUrl } from "./generated/client";
import type { Handlers } from "./generated/handlers";
import type {
  GetPostMeteredResult,
  RequeuePostResponse,
  UpsertPost2Response,
} from "./generated/types";

import { assertErrorStatus, assertReceived } from "./harness";
import type { CaseState, ContractCase } from "./harness";

// A Mount starts a server for one case and returns its base URL plus a close
// hook. The framework-specific driver supplies this; everything else here is
// framework-independent.
export type Mount = (
  c: ContractCase,
  stub: Handlers,
) => Promise<{ baseUrl: string; close: () => Promise<void> }>;

// ── assertions ──────────────────────────────────────────────────────────────

// assertOkHeaders checks the response-header fields the client read off the
// envelope against expect_client.ok_headers: ratelimitRemaining is a required
// number (numeric equality); etag is an optional string that must equal the
// expected string, or be undefined/absent when the expected value is JSON null.
function assertOkHeaders(
  c: ContractCase,
  got: GetPostMeteredResult,
): void {
  const want = c.expect_client.ok_headers;
  if (want === undefined) return;
  for (const [k, w] of Object.entries(want)) {
    switch (k) {
      case "ratelimitRemaining":
        assert.strictEqual(
          got.ratelimitRemaining,
          w,
          `[${c.name}] ok_header ${k}: got ${String(got.ratelimitRemaining)}, want ${String(w)}`,
        );
        break;
      case "etag":
        if (w === null) {
          assert.strictEqual(
            got.etag,
            undefined,
            `[${c.name}] ok_header etag: expected absent, got ${String(got.etag)}`,
          );
        } else {
          assert.strictEqual(
            got.etag,
            w,
            `[${c.name}] ok_header etag: got ${String(got.etag)}, want ${String(w)}`,
          );
        }
        break;
      default:
        throw new Error(`[${c.name}] unknown ok_header ${k}`);
    }
  }
}

// ── driver ──────────────────────────────────────────────────────────────────

export async function runCase(c: ContractCase, mount: Mount): Promise<void> {
  const state: CaseState = { hit: false, received: {} };
  const { baseUrl, close } = await mount(c, makeStub(c, state));
  setBaseUrl(baseUrl);

  try {
    let thrown: unknown;
    let result: unknown;
    try {
      result = await invoke(c);
    } catch (e) {
      thrown = e;
    }

    switch (c.kind) {
      case "ok": {
        assert.ok(
          thrown === undefined,
          `[${c.name}] expected success, got error: ${String(thrown)}`,
        );
        if (c.raw_response === undefined) {
          assert.ok(state.hit, `[${c.name}] handler was never called (ok case)`);
          assertReceived(c, state.received);
        }
        if (c.endpoint === "getPostMetered") {
          // The client returns a typed envelope: compare its `.body` against
          // expect_client.ok and its response-header fields against ok_headers.
          const env = result as GetPostMeteredResult;
          assert.deepStrictEqual(
            env.body,
            c.expect_client.ok,
            `[${c.name}] client result body mismatch`,
          );
          assertOkHeaders(c, env);
        } else if (c.endpoint === "upsertPost2") {
          // Multi-status: the client returns the { status, body } envelope.
          // Assert the observed status, then either an absent body (ok_absent,
          // e.g. 204) or a body that deep-equals expect_client.ok.
          const env = result as UpsertPost2Response;
          assert.strictEqual(
            env.status,
            c.expect_client.status,
            `[${c.name}] client result status: got ${String(env.status)}, want ${String(c.expect_client.status)}`,
          );
          if (c.expect_client.ok_absent === true) {
            assert.ok(
              env.body === undefined || env.body === null,
              `[${c.name}] expected absent envelope body, got ${JSON.stringify(env.body)}`,
            );
          } else {
            assert.deepStrictEqual(
              env.body,
              c.expect_client.ok,
              `[${c.name}] client result body mismatch`,
            );
          }
        } else if (c.endpoint === "requeuePost") {
          // All-typeless multi-status: the envelope is { status } with no body
          // field, so the only client-side observation is the status itself.
          const env = result as RequeuePostResponse;
          assert.strictEqual(
            env.status,
            c.expect_client.status,
            `[${c.name}] client result status: got ${String(env.status)}, want ${String(c.expect_client.status)}`,
          );
        } else if (c.expect_client.expect_download !== undefined) {
          // Binary download: the client returns a Blob; read its bytes as UTF-8
          // and compare to expect_client.expect_download. Keyed on the
          // expect_download field (not the endpoint name) so any binary-download
          // case routes here — mirroring the Go/Python drivers.
          const blob = result as Blob;
          const text = await blob.text();
          assert.strictEqual(
            text,
            c.expect_client.expect_download,
            `[${c.name}] downloaded body mismatch: got ${JSON.stringify(text)}, want ${JSON.stringify(c.expect_client.expect_download)}`,
          );
        } else {
          assert.deepStrictEqual(
            result,
            c.expect_client.ok,
            `[${c.name}] client result mismatch`,
          );
        }
        break;
      }
      case "error": {
        assert.ok(state.hit, `[${c.name}] handler was never called (error case)`);
        assertReceived(c, state.received);
        assertErrorStatus(c, thrown);
        break;
      }
      case "constraint": {
        if (c.handler.expect_not_called === true) {
          assert.ok(
            !state.hit,
            `[${c.name}] constraint case: handler WAS called but should have been rejected server-side`,
          );
        }
        assertErrorStatus(c, thrown);
        break;
      }
      default: {
        const k: never = c.kind;
        throw new Error(`unknown case kind ${String(k)}`);
      }
    }
  } finally {
    await close();
  }
}

export async function main(mount: Mount): Promise<void> {
  const here = dirname(fileURLToPath(import.meta.url));
  const raw = readFileSync(join(here, "contract.json"), "utf8");
  const cases = JSON.parse(raw) as ContractCase[];
  assert.ok(cases.length > 0, "contract.json has no cases");

  let failures = 0;
  for (const c of cases) {
    try {
      await runCase(c, mount);
      console.log(`PASS ${c.name}`);
    } catch (e) {
      failures += 1;
      console.error(`FAIL ${c.name}: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  if (failures > 0) {
    console.error(`\n${String(failures)} case(s) failed`);
    process.exit(1);
  }
  console.log(`\nAll ${String(cases.length)} cases passed`);
}
