// Behavioral inline-response-projection round-trip driver for the TypeScript
// target (Express).
//
// Committed source. The Rust harness
// (`roundtrip.rs::projection_typescript_roundtrip`) generates the small projection
// schema into ./generated-projection/ (separate from the other round-trips'
// generated dirs, so they never race), then runs this via `tsx`. It proves the
// generated `<Endpoint>Response` projected structs round-trip the wire — a bare
// projected response, a `List<…>` of them, and a `partial` projection (every field
// optional) carry their picked fields back — AND that the client's revival of the
// GENERATED projected struct turns `createdAt` back into a real `Date` (the runtime
// behavior compile-lint can't assert). The `partial` case additionally exercises the
// reviver's optional-field wrapping path (the projected `createdAt` is optional yet
// still revived when present). Exits nonzero on failure.

import express from "express";
import type { AddressInfo } from "node:net";

import { createRouter } from "./generated-projection/server";
import type { Handlers } from "./generated-projection/handlers";
import { api, setBaseUrl } from "./generated-projection/client";
import { parseUuid } from "./generated-projection/types";
import type {
  GetContactResponse,
  GetProfileResponse,
  GetSummaryResponse,
  ListProfilesResponse,
} from "./generated-projection/types";

function check(cond: boolean, msg: string): void {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

const profileCreated = new Date("2026-01-02T03:04:05.000Z");
const listCreated = new Date("2026-02-03T04:05:06.000Z");
const summaryCreated = new Date("2026-03-04T05:06:07.000Z");
const contactCreated = new Date("2026-04-05T06:07:08.000Z");

const stub: Handlers = {
  getProfile(id: string): Promise<GetProfileResponse> {
    return Promise.resolve({
      id: parseUuid("11111111-1111-1111-1111-111111111111"),
      displayName: id,
      createdAt: profileCreated,
    });
  },
  listProfiles(): Promise<ListProfilesResponse[]> {
    return Promise.resolve([
      {
        id: parseUuid("22222222-2222-2222-2222-222222222222"),
        displayName: "ada",
        createdAt: listCreated,
      },
    ]);
  },
  getSummary(id: string): Promise<GetSummaryResponse> {
    return Promise.resolve({
      id: parseUuid("33333333-3333-3333-3333-333333333333"),
      displayName: id,
      createdAt: summaryCreated,
    });
  },
  getContact(id: string): Promise<GetContactResponse> {
    return Promise.resolve({
      id: parseUuid("44444444-4444-4444-4444-444444444444"),
      displayName: id,
      email: "ada@example.com",
      createdAt: contactCreated,
    });
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

  // Bare projected response: the picked fields round-trip; `createdAt` is revived
  // to a real `Date` by the client's `revive<Endpoint>Response` pass.
  const p = await api.getProfile("grace");
  check(p.id === "11111111-1111-1111-1111-111111111111", "projected id");
  check(p.displayName === "grace", "projected displayName");
  check(p.createdAt instanceof Date, "projected createdAt is a Date (revived)");
  check(
    p.createdAt.getTime() === profileCreated.getTime(),
    "projected createdAt round-tripped",
  );

  // List of projected responses: each element round-trips, items revived.
  const list = await api.listProfiles();
  check(list.length === 1, "listProfiles length");
  check(list[0].id === "22222222-2222-2222-2222-222222222222", "list element id");
  check(list[0].displayName === "ada", "list element displayName");
  check(list[0].createdAt instanceof Date, "list element createdAt revived");
  check(
    list[0].createdAt.getTime() === listCreated.getTime(),
    "list element createdAt round-tripped",
  );

  // Partial projected response: every field optional, yet a present `createdAt` is
  // still revived to a real `Date` via the reviver's optional-field wrapping path.
  const s = await api.getSummary("turing");
  check(s.id === "33333333-3333-3333-3333-333333333333", "partial id");
  check(s.displayName === "turing", "partial displayName");
  check(s.createdAt instanceof Date, "partial createdAt is a Date (revived, optional)");
  check(
    s.createdAt!.getTime() === summaryCreated.getTime(),
    "partial createdAt round-tripped",
  );

  // Omit projection: the complementary selector (drops `passwordHash`); the kept
  // fields — incl. `email` — round-trip, with `createdAt` revived to a real `Date`.
  const c = await api.getContact("ada");
  check(c.id === "44444444-4444-4444-4444-444444444444", "omit id");
  check(c.displayName === "ada", "omit displayName");
  check(c.email === "ada@example.com", "omit email");
  check(c.createdAt instanceof Date, "omit createdAt is a Date (revived)");
  check(
    c.createdAt.getTime() === contactCreated.getTime(),
    "omit createdAt round-tripped",
  );

  await new Promise<void>((resolve, reject) =>
    server.close((err) => (err ? reject(err) : resolve())),
  );
  console.log("OK");
}

void main();
