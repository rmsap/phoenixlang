// Behavioral UUID round-trip driver for the TypeScript target (Express).
//
// Committed source. The Rust harness (`roundtrip.rs::uuid_typescript_roundtrip`)
// generates the small UUID schema into ./generated-uuid/ (separate from the
// other round-trips' generated dirs, so they never race), then runs this via
// `tsx`. It proves the branded `Uuid` alias and the `parseUuid` validate-on-decode
// pass round-trip RFC 4122 strings: body uuids (required / optional / list / map)
// come back equal and still typed `string` at runtime (the brand is compile-time
// only); a `Uuid` query param round-trips (echoed by the stub into the body); a
// required response-header uuid arrives; and a bare `Uuid` response decodes.
// The server-side body reviver runs `parseUuid` on the incoming body, so the
// `check`s also prove valid input passes that validation. Exits nonzero on
// failure.

import express from "express";
import type { AddressInfo } from "node:net";

import { createRouter } from "./generated-uuid/server";
import type { Handlers } from "./generated-uuid/handlers";
import { api, setBaseUrl } from "./generated-uuid/client";
import { parseUuid } from "./generated-uuid/types";
import type {
  Account,
  EchoAccountBody,
  GetAccountResult,
  Uuid,
} from "./generated-uuid/types";

function check(cond: boolean, msg: string): void {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

const reqId = parseUuid("11111111-1111-1111-1111-111111111111");

const stub: Handlers = {
  // Echo the decoded (and server-side reviver-validated) body back unchanged.
  echoAccount(body: EchoAccountBody): Promise<Account> {
    check(typeof body.id === "string", "server: body.id is a string");
    return Promise.resolve(body as Account);
  },
  // Echo the query uuid into `id` so the client can assert it round-tripped.
  getAccount(id: string, query: { ref: Uuid }): Promise<GetAccountResult> {
    return Promise.resolve({
      body: { id: query.ref, members: [], index: {} },
      requestId: reqId,
    });
  },
  newId(): Promise<Uuid> {
    return Promise.resolve(parseUuid("550e8400-e29b-41d4-a716-446655440000"));
  },
};

async function main(): Promise<void> {
  const app = express();
  app.set("etag", false);
  app.use(express.json());
  app.use(createRouter(stub));
  const server = app.listen(0);
  await new Promise<void>((resolve) => server.once("listening", resolve));
  const port = (server.address() as AddressInfo).port;
  setBaseUrl(`http://127.0.0.1:${String(port)}`);

  const idA = parseUuid("550e8400-e29b-41d4-a716-446655440000");
  const idB = parseUuid("6ba7b810-9dad-11d1-80b4-00c04fd430c8");
  const idC = parseUuid("6ba7b811-9dad-11d1-80b4-00c04fd430c8");

  const resp = await api.echoAccount({
    id: idA,
    ownerId: idB,
    members: [idC],
    index: { primary: idA },
  });
  // Branded `Uuid`s are strings at runtime; equality is string equality.
  check(typeof resp.id === "string" && resp.id === idA, "echo id");
  check(resp.ownerId === idB, "echo optional ownerId");
  check(resp.members.length === 1 && resp.members[0] === idC, "echo list member");
  check(resp.index.primary === idA, "echo map value");

  const ref = parseUuid("00000000-0000-0000-0000-000000000000");
  const r2 = await api.getAccount("acct-1", { ref });
  check(r2.body.id === ref, "query Uuid round-tripped through the server");
  check(r2.requestId === reqId, "response-header Uuid arrived");

  const id = await api.newId();
  check(typeof id === "string" && id === idA, "bare Uuid response decoded");

  // Reject path: a malformed body uuid must fail the server's body reviver
  // (`parseUuid` throws → caught as 500 → client throws on `!response.ok`). The
  // double cast smuggles a bad value past `parseUuid` so the wire decode — not
  // local construction — is what rejects it. Proves the validator actually fires.
  let rejected = false;
  try {
    await api.echoAccount({
      id: "not-a-uuid" as unknown as Uuid,
      members: [],
      index: {},
    });
  } catch {
    rejected = true;
  }
  check(rejected, "server rejected malformed body uuid");

  // Reject path (query param): unlike Go, TS validates query/request-header
  // `Uuid`s inline via `parseUuid` on the server, so a malformed `ref` must be
  // rejected (`parseUuid` throws → 500 → client throws on `!response.ok`). This
  // pins the documented TS-validates / Go-accepts divergence (see the Go
  // driver's accept assertion). The cast smuggles a bad value past the typed
  // client signature so the server coercion is what rejects it.
  let queryRejected = false;
  try {
    await api.getAccount("acct-1", { ref: "not-a-uuid" as unknown as Uuid });
  } catch {
    queryRejected = true;
  }
  check(queryRejected, "server rejected malformed query uuid");

  await new Promise<void>((resolve, reject) =>
    server.close((err) => (err ? reject(err) : resolve())),
  );
  console.log("OK");
}

void main();
