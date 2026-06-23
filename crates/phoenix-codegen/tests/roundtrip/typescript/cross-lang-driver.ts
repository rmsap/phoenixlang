// Cross-language wire-conformance driver for the TypeScript (Express) target.
//
// Committed source. The Rust harness (`roundtrip.rs::cross_lang_typescript_conformance`)
// generates the Express server + client into ./generated-cross-lang/ and runs this via
// tsx. Unlike the other TS round-trips — which only prove TS's client and server agree
// with EACH OTHER — this asserts the actual bytes TS puts on the wire equal the single
// golden contract (../cross_lang/wire.json) every target is checked against.
// Conformance of all three targets to one wire ⟹ any client interoperates with any
// server, without cross-process pairing.
//
// The generated client is driven against the generated server through a `fetch`
// wrapper that records the request sent and the response received; both are compared
// to the golden. Comparison is structural, except a `createdAt` datetime compares as
// an INSTANT (TS emits RFC 3339 with `.000Z` while Go emits `Z` and Python `+00:00` —
// all valid RFC 3339 and mutually parseable). Exits nonzero on failure.

import { readFileSync } from "node:fs";
import { join } from "node:path";
import express from "express";
import type { AddressInfo } from "node:net";

import { api, setBaseUrl } from "./generated-cross-lang/client";
import { createRouter } from "./generated-cross-lang/server";
import type { Handlers } from "./generated-cross-lang/handlers";
import type {
  Account,
  CreateAccountBody,
  Decimal,
  Url,
  Uuid,
} from "./generated-cross-lang/types";

const golden = JSON.parse(
  readFileSync(join(import.meta.dirname, "..", "cross_lang", "wire.json"), "utf8"),
) as Record<string, unknown>;

// Typed value matching golden `account`.
const ACCT: Account = {
  id: "11111111-1111-1111-1111-111111111111" as Uuid,
  createdAt: new Date("2026-01-15T08:30:00Z"),
  balance: "19.99" as Decimal,
  homepage: "https://Example.com/u?x=1#f" as Url,
  avatar: new Uint8Array([0x00, 0x01, 0xff]),
  wallet: { amount: "5.00" as Decimal, currency: "USD" },
  role: "admin",
  profile: { displayName: "Ada", avatarUrl: undefined },
  tags: ["x", "y"],
  active: true,
};

const stub: Handlers = {
  // Echo the DECODED body (not the constant `ACCT`): if the server dropped or
  // renamed a field on decode, the echoed response wire would diverge from the
  // golden. This is what exercises server-side request-body decode here.
  createAccount: (body: CreateAccountBody): Promise<Account> => Promise.resolve(body as Account),
  getAccount: (): Promise<Account> => Promise.resolve(ACCT),
  listAccounts: () => Promise.resolve({ items: [ACCT], totalCount: 3 }),
};

function check(cond: boolean, msg: string): void {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

const RFC3339_PREFIX = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}/;

// Structural JSON equality; two RFC 3339 datetime strings compare as instants.
function jsonEqual(a: unknown, b: unknown): boolean {
  if (Array.isArray(a) && Array.isArray(b)) {
    return a.length === b.length && a.every((x, i) => jsonEqual(x, b[i]));
  }
  if (
    a !== null &&
    b !== null &&
    typeof a === "object" &&
    typeof b === "object" &&
    !Array.isArray(a) &&
    !Array.isArray(b)
  ) {
    const ao = a as Record<string, unknown>;
    const bo = b as Record<string, unknown>;
    // Union of keys, treating a missing key as `null`: an absent optional (TS omits
    // it) and an explicit `null` (Go/Python emit it) are equivalent for a Phoenix
    // `Option`; a present value vs a missing key still differs, so dropped REQUIRED
    // fields are still caught. `?? null` maps both `undefined` and `null` to `null`
    // while leaving `false`/`0`/`""` intact. Corollary: a RENAMED field is caught
    // only when its golden value is non-null (a renamed null optional like
    // `avatarUrl` — or, equivalently, any extra spurious null-valued field — slips
    // through; the non-null `displayName` exercises the nested-struct rename).
    const keys = new Set([...Object.keys(ao), ...Object.keys(bo)]);
    for (const k of keys) {
      if (!jsonEqual(ao[k] ?? null, bo[k] ?? null)) return false;
    }
    return true;
  }
  if (typeof a === "string" && typeof b === "string") {
    if (a === b) return true;
    // Only treat datetime-shaped strings as instants. `Date.parse` is far more
    // lenient than Go's `time.Parse(RFC3339)` / Python's `fromisoformat`, so gate
    // on an RFC-3339-ish prefix first to keep the three comparators equally strict
    // (otherwise a non-datetime string pair could coerce to a match here only).
    if (!RFC3339_PREFIX.test(a) || !RFC3339_PREFIX.test(b)) return false;
    const da = Date.parse(a);
    const db = Date.parse(b);
    return !Number.isNaN(da) && !Number.isNaN(db) && da === db;
  }
  return a === b;
}

function assertWire(label: string, gotText: string, want: unknown): void {
  let got: unknown;
  try {
    got = JSON.parse(gotText);
  } catch {
    check(false, `${label}: invalid JSON: ${gotText}`);
    return;
  }
  check(jsonEqual(got, want), `${label}: wire mismatch\n got:  ${gotText}\n want: ${JSON.stringify(want)}`);
}

// Parses a URL's query string into a repeated-key multimap and asserts it equals
// the golden `{key: [values...]}` map. Query values compare EXACTLY (plain string
// equality), not via `jsonEqual` — the latter's datetime-instant leniency has no
// place in param wire, and exact comparison matches the Go/Python drivers.
function assertQuery(label: string, urlStr: string, want: Record<string, string[]>): void {
  const got: Record<string, string[]> = {};
  for (const [k, v] of new URL(urlStr).searchParams) (got[k] ??= []).push(v);
  const gotKeys = Object.keys(got);
  const wantKeys = Object.keys(want);
  const equal =
    gotKeys.length === wantKeys.length &&
    wantKeys.every((k) => {
      const gv = got[k];
      const wv = want[k];
      return gv !== undefined && gv.length === wv.length && gv.every((x, i) => x === wv[i]);
    });
  check(equal, `${label} query: ${JSON.stringify(got)} != ${JSON.stringify(want)}`);
}

// Shallow-copies an object, renaming top-level key `from` to `to`.
function renameKey(o: Record<string, unknown>, from: string, to: string): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(o)) out[k === from ? to : k] = v;
  return out;
}

// Records the most recent request + response at the fetch boundary.
let lastUrl = "";
let lastReqMethod = "";
let lastReqBody: string | undefined;
let lastReqHeaders = new Headers();
let lastRespBody = "";
const realFetch = globalThis.fetch;
globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
  lastUrl = typeof input === "string" ? input : input.toString();
  lastReqMethod = (init?.method ?? "GET").toUpperCase();
  lastReqBody = typeof init?.body === "string" ? init.body : undefined;
  lastReqHeaders = new Headers(init?.headers);
  const resp = await realFetch(input, init);
  lastRespBody = await resp.clone().text();
  return resp;
}) as typeof fetch;

async function main(): Promise<void> {
  const app = express();
  app.set("etag", false);
  app.use(express.json());
  app.use(createRouter(stub));
  const server = app.listen(0);
  await new Promise<void>((resolve) => server.once("listening", resolve));
  const port = (server.address() as AddressInfo).port;
  setBaseUrl(`http://127.0.0.1:${String(port)}`);

  // Meta-guard: the comparator MUST reject a snake_cased rename of a non-null
  // field — exactly the shape of the snake-wire bug this whole test exists to
  // catch. Without this, a future change that weakened `jsonEqual` (e.g.
  // intersecting keys instead of unioning) would make every assertion below pass
  // vacuously.
  check(
    !jsonEqual(
      renameKey(golden.account as Record<string, unknown>, "createdAt", "created_at"),
      golden.account,
    ),
    "comparator accepted a snake_cased rename; conformance assertions would be vacuous",
  );
  // Meta-guard for the OTHER load-bearing rule, the datetime-instant path: it must
  // not collapse two DIFFERENT instants, and must not leak into non-datetime strings
  // (the RFC-3339 prefix gate). Either weakening would let an over-lenient comparator
  // pass the conformance assertions vacuously, the same way a weakened key rule would.
  check(
    !jsonEqual("2026-01-15T08:30:00Z", "2026-01-15T09:30:00Z"),
    "comparator treated two different instants as equal",
  );
  check(
    !jsonEqual("admin", "guest"),
    "comparator treated two different non-datetime strings as equal",
  );

  try {
    // createAccount: request line (method / path) + body sent + response received.
    await api.createAccount(ACCT as CreateAccountBody);
    const createSpec = golden.createAccountRequest as { method: string; path: string };
    check(
      lastReqMethod === createSpec.method,
      `createAccount method: ${lastReqMethod} != ${createSpec.method}`,
    );
    check(
      new URL(lastUrl).pathname === createSpec.path,
      `createAccount path: ${new URL(lastUrl).pathname} != ${createSpec.path}`,
    );
    check(lastReqBody !== undefined, "createAccount: no request body captured");
    assertWire("createAccount request body", lastReqBody ?? "", golden.account);
    assertWire("createAccount response body", lastRespBody, golden.account);

    // getAccount: param wire (path / repeated-key query / aliased header) + response.
    await api.getAccount(
      "acc-7",
      { includeArchived: true, roles: ["admin", "guest"] },
      { requestId: "req-1" },
    );
    const spec = golden.getAccountRequest as {
      method: string;
      path: string;
      query: Record<string, string[]>;
      headers: Record<string, string>;
    };
    check(
      lastReqMethod === spec.method,
      `getAccount method: ${lastReqMethod} != ${spec.method}`,
    );
    check(
      new URL(lastUrl).pathname === spec.path,
      `getAccount path: ${new URL(lastUrl).pathname} != ${spec.path}`,
    );
    assertQuery("getAccount", lastUrl, spec.query);
    for (const [k, v] of Object.entries(spec.headers)) {
      check(
        lastReqHeaders.get(k) === v,
        `getAccount header ${k}: ${String(lastReqHeaders.get(k))} != ${v}`,
      );
    }
    assertWire("getAccount response body", lastRespBody, golden.account);

    // listAccounts: the request line + query (`page`) plus the pagination
    // envelope wire ({ items, totalCount }).
    await api.listAccounts({ page: 2 });
    const listSpec = golden.listAccountsRequest as {
      method: string;
      path: string;
      query: Record<string, string[]>;
    };
    check(
      lastReqMethod === listSpec.method,
      `listAccounts method: ${lastReqMethod} != ${listSpec.method}`,
    );
    check(
      new URL(lastUrl).pathname === listSpec.path,
      `listAccounts path: ${new URL(lastUrl).pathname} != ${listSpec.path}`,
    );
    assertQuery("listAccounts", lastUrl, listSpec.query);
    assertWire("listAccounts response body", lastRespBody, golden.page);
  } finally {
    globalThis.fetch = realFetch;
    await new Promise<void>((resolve, reject) =>
      server.close((err) => (err ? reject(err) : resolve())),
    );
  }

  console.log("OK");
}

void main();
