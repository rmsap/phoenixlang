// Behavioral DateTime round-trip driver for the TypeScript target (Express).
//
// Committed source. The Rust harness (`roundtrip.rs::datetime_typescript_roundtrip`)
// generates the small DateTime schema into ./generated-datetime/ (a separate dir
// from the contract-driven ./generated/, so the two TS round-trips never race),
// then runs this via `tsx`. It proves the generated `Date` revival pass and the
// `.toISOString()` (de)serialization actually round-trip RFC 3339 over the wire:
//   - request body Dates (required / optional / list) come back as real `Date`s
//     with identical instants (proving JSON.stringify → server → revive);
//   - a `DateTime` query param round-trips (echoed by the stub into the body);
//   - a required `DateTime` response header arrives as a `Date`.
// `instanceof Date` guards catch a missing revival (a plain string would fail it);
// `.getTime()` equality catches a wrong wire format. Exits nonzero on failure.

import express from "express";
import type { AddressInfo } from "node:net";

import { createRouter } from "./generated-datetime/server";
import type { Handlers } from "./generated-datetime/handlers";
import { api, setBaseUrl } from "./generated-datetime/client";
import type {
  EchoEventBody,
  EchoTaskBody,
  Event,
  GetEventResult,
  Task,
} from "./generated-datetime/types";

function check(cond: boolean, msg: string): void {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

const stub: Handlers = {
  // Echo the decoded body back unchanged. The server-side `instanceof Date`
  // checks prove the request body was REVIVED before reaching the handler — a
  // `JSON.parse`d body decodes these to strings, so without the server reviver
  // the handler's `Date`-typed fields would be raw strings (a runtime type lie).
  echoEvent(body: EchoEventBody): Promise<Event> {
    check(body.startsAt instanceof Date, "server: body.startsAt revived to Date");
    check(body.endsAt instanceof Date, "server: body.endsAt revived to Date");
    check(
      body.checkpoints[0] instanceof Date,
      "server: body.checkpoints[] revived to Date",
    );
    check(
      body.phases.kickoff instanceof Date,
      "server: body.phases value revived to Date",
    );
    return Promise.resolve(body as Event);
  },
  // Echo a body whose only Date is nested inside `reminder` (no direct Date),
  // exercising nested-struct revival on the way IN (server) and out (client).
  echoTask(body: EchoTaskBody): Promise<Task> {
    check(
      body.reminder.remindAt instanceof Date,
      "server: nested body.reminder.remindAt revived to Date",
    );
    return Promise.resolve(body as Task);
  },
  // Bare scalar / list / map DateTime responses, echoing the `at` query date.
  echoInstant(query: { at: Date }): Promise<Date> {
    return Promise.resolve(query.at);
  },
  echoInstants(query: { at: Date }): Promise<Date[]> {
    return Promise.resolve([query.at]);
  },
  echoInstantMap(query: { at: Date }): Promise<Record<string, Date>> {
    return Promise.resolve({ at: query.at });
  },
  // Echo the parsed query date into `startsAt` so the client can assert the
  // `since` query param survived encode → server-parse; set the required
  // `servedAt` and the optional `expiresAt` so the client can assert the optional
  // response-header read path round-trips.
  getEvent(id: string, query: { since: Date }): Promise<GetEventResult> {
    return Promise.resolve({
      body: { id: 1, name: id, startsAt: query.since, checkpoints: [], phases: {} },
      servedAt: new Date("2020-01-02T03:04:05Z"),
      expiresAt: new Date("2020-01-03T00:00:00Z"),
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

  const start = new Date("2026-06-16T12:30:00Z");
  const end = new Date("2026-06-17T08:00:00Z");
  const cp = new Date("2026-06-16T13:00:00Z");

  const resp = await api.echoEvent({
    id: 7,
    name: "launch",
    startsAt: start,
    endsAt: end,
    checkpoints: [cp],
    phases: { kickoff: start, wrap: end },
  });
  check(resp.startsAt instanceof Date, "echo startsAt is a Date (revived)");
  check(resp.startsAt.getTime() === start.getTime(), "echo startsAt instant");
  check(
    resp.endsAt instanceof Date && resp.endsAt.getTime() === end.getTime(),
    "echo optional endsAt instant",
  );
  check(
    resp.checkpoints.length === 1 &&
      resp.checkpoints[0] instanceof Date &&
      resp.checkpoints[0].getTime() === cp.getTime(),
    "echo list checkpoint instant",
  );
  // Map<String, DateTime> values must be revived to real `Date`s in place.
  check(
    resp.phases.kickoff instanceof Date &&
      resp.phases.kickoff.getTime() === start.getTime() &&
      resp.phases.wrap instanceof Date &&
      resp.phases.wrap.getTime() === end.getTime(),
    "echo map phase instants (revived)",
  );

  // Body whose only Date is nested inside a struct field: the returned `Task`
  // must have its nested `reminder.remindAt` revived to a real `Date`.
  const remindAt = new Date("2026-06-16T13:00:00Z");
  const task = await api.echoTask({
    id: 3,
    reminder: { note: "ping", remindAt },
  });
  check(
    task.reminder.remindAt instanceof Date &&
      task.reminder.remindAt.getTime() === remindAt.getTime(),
    "nested-struct DateTime revived",
  );

  const since = new Date("2025-12-31T23:59:59Z");
  const r2 = await api.getEvent("evt-9", { since });
  check(
    r2.body.startsAt instanceof Date && r2.body.startsAt.getTime() === since.getTime(),
    "query DateTime round-tripped through the server",
  );
  check(
    r2.servedAt instanceof Date &&
      r2.servedAt.getTime() === new Date("2020-01-02T03:04:05Z").getTime(),
    "response-header DateTime arrived as a Date",
  );
  check(
    r2.expiresAt instanceof Date &&
      r2.expiresAt.getTime() === new Date("2020-01-03T00:00:00Z").getTime(),
    "optional response-header DateTime arrived as a Date",
  );

  // Bare scalar / list / map DateTime responses: each must be revived to real
  // `Date`(s) and carry the same instant as the `at` query date.
  const inst = await api.echoInstant({ at: start });
  check(
    inst instanceof Date && inst.getTime() === start.getTime(),
    "bare scalar DateTime response revived",
  );
  const insts = await api.echoInstants({ at: start });
  check(
    insts.length === 1 &&
      insts[0] instanceof Date &&
      insts[0].getTime() === start.getTime(),
    "bare List<DateTime> response revived",
  );
  const instMap = await api.echoInstantMap({ at: start });
  check(
    instMap.at instanceof Date && instMap.at.getTime() === start.getTime(),
    "bare Map<String, DateTime> response revived",
  );

  await new Promise<void>((resolve, reject) =>
    server.close((err) => (err ? reject(err) : resolve())),
  );
  console.log("OK");
}

void main();
