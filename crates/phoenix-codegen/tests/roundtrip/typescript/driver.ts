// Behavioral round-trip driver for the TypeScript target.
//
// This program is committed source. The Rust harness
// (`crates/phoenix-codegen/tests/roundtrip.rs`) assembles a runnable project at
// test time:
//   - the generated client/server/types/handlers in ./generated/,
//   - this file (driver.ts) + contract.json next to it,
//   - the committed package.json + lockfile + node_modules (express + tsx).
//
// For each case in contract.json it:
//   1. builds a fixture-driven stub implementing the generated `Handlers`
//      interface that records the decoded args it received and either returns
//      the canned success value or throws `new Error("<variant>")` (so the
//      generated server's `error.message === "X"` mapping fires);
//   2. mounts `createRouter(stub)` on an express app (with express.json() body
//      parsing) and `app.listen(0)` for an ephemeral port;
//   3. points the generated fetch client at `http://127.0.0.1:<port>` and
//      invokes the matching client method with the case inputs;
//   4. asserts:
//      (a) the handler received exactly the expected decoded inputs (catches
//          query-coercion / path-substitution / body-decode bugs), and
//      (b) for ok cases the client's typed result deep-equals expect_client.ok;
//          for error/constraint cases the client threw `ApiError` whose .status
//          equals the expected per-target status (and .code equals the variant).
//          Constraint cases additionally assert the handler was NOT called
//          (server rejected via validateCreatePostBody before the handler).
//
// Exits nonzero on the first failure; the Rust harness asserts exit 0.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import express from "express";
import type { AddressInfo } from "node:net";

import { createRouter } from "./generated/server";
import type { Handlers } from "./generated/handlers";
import { api, setBaseUrl, ApiError } from "./generated/client";
import type {
  Author,
  CreatePostBody,
  Post,
  UpdateAuthorProfileBody,
} from "./generated/types";

const target = "typescript" as const;

// ── contract.json schema (mirror of the language-agnostic format) ───────────

interface ContractCase {
  name: string;
  endpoint: string;
  kind: "ok" | "error" | "constraint";
  call: {
    path_params?: Record<string, string>;
    query?: Record<string, unknown>;
    body?: Record<string, unknown>;
  };
  handler: {
    expect_received?: Record<string, unknown>;
    returns?: unknown;
    raises?: string;
    expect_not_called?: boolean;
  };
  expect_client: {
    ok?: unknown;
    error?: {
      variant: string;
      status_per_target: Record<string, number>;
    };
  };
}

// ── fixture-driven stub ─────────────────────────────────────────────────────

// State shared between the stub and the per-case assertions. `hit` records
// whether the relevant handler ran; `received` is the flattened map of decoded
// args the handler saw (compared against expect_received).
interface CaseState {
  hit: boolean;
  received: Record<string, unknown>;
}

// errOrReturn signals the handler's response: throws an Error carrying the
// variant name (so the server maps it to a status) or returns the canned value.
function signal<T>(c: ContractCase): T {
  if (c.handler.raises !== undefined) {
    throw new Error(c.handler.raises);
  }
  return c.handler.returns as T;
}

// Builds a Handlers implementation for one case. Only the exercised endpoints
// record args + respond; the rest throw so an unexpected route is loud.
function makeStub(c: ContractCase, state: CaseState): Handlers {
  const unexpected = (name: string): never => {
    throw new Error(`unexpected call to ${name}`);
  };
  return {
    async listPosts(query) {
      state.hit = true;
      state.received = {
        page: query.page,
        limit: query.limit,
        tag: query.tag ?? null,
        search: query.search ?? null,
        featured: query.featured,
        minScore: query.minScore,
        maxScore: query.maxScore ?? null,
      };
      return signal<Post[]>(c);
    },
    async searchPosts(query) {
      state.hit = true;
      state.received = {
        maxResults: query.maxResults,
        sortField: query.sortField,
      };
      return signal<Post[]>(c);
    },
    async getPost(id) {
      state.hit = true;
      state.received = { id };
      return signal<Post>(c);
    },
    async createPost(body) {
      state.hit = true;
      state.received = {
        title: body.title,
        body: body.body,
        status: body.status,
        tags: body.tags ?? null,
      };
      return signal<Post>(c);
    },
    async updateAuthorProfile(id, body) {
      // Body carries a constrained Option<string> field (avatarUrl) also
      // `partial`-applied; the server rejects the empty-avatarUrl constraint
      // case before this runs — args recorded for completeness only.
      state.hit = true;
      state.received = {
        id,
        name: body.name,
        avatarUrl: body.avatarUrl ?? null,
      };
      return signal<Author>(c);
    },
    updatePost: () => unexpected("updatePost"),
    patchPost: () => unexpected("patchPost"),
    deletePost: () => unexpected("deletePost"),
    listComments: () => unexpected("listComments"),
    createComment: () => unexpected("createComment"),
    getAuthorProfile: () => unexpected("getAuthorProfile"),
  };
}

// ── client invocation ───────────────────────────────────────────────────────

// invoke calls the matching client method with the case inputs and returns the
// typed result. Throws (ApiError or Error) on a non-ok response — the caller
// distinguishes ok vs error/constraint by whether this throws.
async function invoke(c: ContractCase): Promise<unknown> {
  switch (c.endpoint) {
    case "getPost": {
      const id = c.call.path_params?.id;
      assert.ok(id !== undefined, `${c.name}: missing path_params.id`);
      return api.getPost(id);
    }
    case "listPosts": {
      const q = c.call.query ?? {};
      // Pass each query value with the client's declared type; omit absent
      // optionals so the client falls back to its default / leaves them unset.
      const opts: Parameters<typeof api.listPosts>[0] = {};
      if (q.page !== undefined) opts.page = q.page as number;
      if (q.limit !== undefined) opts.limit = q.limit as number;
      if (q.tag !== undefined) opts.tag = q.tag as string;
      if (q.search !== undefined) opts.search = q.search as string;
      if (q.featured !== undefined) opts.featured = q.featured as boolean;
      if (q.minScore !== undefined) opts.minScore = q.minScore as number;
      if (q.maxScore !== undefined) opts.maxScore = q.maxScore as number;
      return api.listPosts(opts);
    }
    case "searchPosts": {
      const q = c.call.query ?? {};
      return api.searchPosts({
        maxResults: q.maxResults as number,
        sortField: q.sortField as string,
      });
    }
    case "createPost": {
      const body = c.call.body as unknown as CreatePostBody;
      return api.createPost(body);
    }
    case "updateAuthorProfile": {
      const id = c.call.path_params?.id;
      assert.ok(id !== undefined, `${c.name}: missing path_params.id`);
      const body = c.call.body as unknown as UpdateAuthorProfileBody;
      return api.updateAuthorProfile(id, body);
    }
    default:
      throw new Error(`driver has no invoke mapping for endpoint ${c.endpoint}`);
  }
}

// ── assertions ──────────────────────────────────────────────────────────────

// assertReceived checks every key in expect_received against the decoded args
// the handler actually saw. Numbers compare numerically; only listed keys are
// checked. `null` in the contract means the optional arg was absent.
function assertReceived(c: ContractCase, got: Record<string, unknown>): void {
  const want = c.handler.expect_received;
  if (want === undefined) return;
  for (const [k, w] of Object.entries(want)) {
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
function assertErrorStatus(c: ContractCase, thrown: unknown): void {
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

// ── driver ──────────────────────────────────────────────────────────────────

async function runCase(c: ContractCase): Promise<void> {
  const state: CaseState = { hit: false, received: {} };
  const app = express();
  app.use(express.json());
  app.use(createRouter(makeStub(c, state)));

  const server = app.listen(0);
  await new Promise<void>((resolve) => server.once("listening", resolve));
  const port = (server.address() as AddressInfo).port;
  setBaseUrl(`http://127.0.0.1:${String(port)}`);

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
        assert.ok(state.hit, `[${c.name}] handler was never called (ok case)`);
        assertReceived(c, state.received);
        assert.deepStrictEqual(
          result,
          c.expect_client.ok,
          `[${c.name}] client result mismatch`,
        );
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
    await new Promise<void>((resolve, reject) =>
      server.close((err) => (err ? reject(err) : resolve())),
    );
  }
}

async function main(): Promise<void> {
  const here = dirname(fileURLToPath(import.meta.url));
  const raw = readFileSync(join(here, "contract.json"), "utf8");
  const cases = JSON.parse(raw) as ContractCase[];
  assert.ok(cases.length > 0, "contract.json has no cases");

  let failures = 0;
  for (const c of cases) {
    try {
      await runCase(c);
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

void main();
