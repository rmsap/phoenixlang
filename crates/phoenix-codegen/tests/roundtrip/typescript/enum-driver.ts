// Behavioral enum query/header round-trip driver for the TypeScript target
// (Express).
//
// Committed source. The Rust harness (`roundtrip.rs::enum_typescript_roundtrip`)
// generates the small enum schema into ./generated-enum/ (separate from the other
// round-trips' generated dirs, so they never race), then runs this via `tsx`. It
// proves enum query/header values survive the wire as the bare variant string: a
// required enum query param and a defaulted one (`size = Medium`, applied
// server-side when omitted), a required and an `Option` request header, and a
// required and an `Option` response header all round-trip. The reject path drives
// an UNKNOWN variant through the query and through a header (smuggled past the
// typed signature with a cast) to prove the server's `parse<Enum>` validation
// fires (throws `ValidationError` → 400 → client throws on `!response.ok`). Exits
// nonzero on failure.

import express from "express";
import type { AddressInfo } from "node:net";

import { createRouter } from "./generated-enum/server";
import type { Handlers } from "./generated-enum/handlers";
import { api, setBaseUrl } from "./generated-enum/client";
import type { Color, PickItemResult, Size } from "./generated-enum/types";

function check(cond: boolean, msg: string): void {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

const stub: Handlers = {
  // Echo the query enums into the body and the header enums into the response
  // headers, so the client can assert each position round-tripped.
  pickItem(
    query: { color: Color; size: Size },
    headers: { preferred: Color; fallback?: Color | undefined },
  ): Promise<PickItemResult> {
    return Promise.resolve({
      body: { name: "picked", color: query.color, size: query.size },
      chosen: headers.preferred,
      alt: headers.fallback,
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

  // Required + Option enum query/header values all round-trip the wire.
  const r = await api.pickItem(
    { color: "Blue", size: "Large" },
    { preferred: "Red", fallback: "Green" },
  );
  check(r.body.color === "Blue", "query color round-tripped");
  check(r.body.size === "Large", "query size round-tripped");
  check(r.chosen === "Red", "required response-header enum arrived");
  check(r.alt === "Green", "optional response-header enum arrived");

  // Omitting the defaulted query enum lets the server apply `Medium`; the
  // optional request header is absent, so the optional response header is too.
  const r2 = await api.pickItem({ color: "Red" }, { preferred: "Blue" });
  check(r2.body.size === "Medium", "defaulted query enum applied server-side");
  check(r2.body.color === "Red", "required query enum round-tripped");
  check(r2.chosen === "Blue", "required header enum round-tripped");
  check(r2.alt === undefined, "absent optional response-header enum is undefined");

  // Reject path (query): an unknown variant must fail the server's parseColor
  // (throws ValidationError → 400 → client throws). The cast smuggles a bad value
  // past the typed signature so the server coercion is what rejects it.
  let queryRejected = false;
  try {
    await api.pickItem(
      { color: "Purple" as unknown as Color },
      { preferred: "Red" },
    );
  } catch {
    queryRejected = true;
  }
  check(queryRejected, "server rejected unknown query enum");

  // Reject path (header): an unknown header variant must likewise 400.
  let headerRejected = false;
  try {
    await api.pickItem(
      { color: "Red" },
      { preferred: "Mauve" as unknown as Color },
    );
  } catch {
    headerRejected = true;
  }
  check(headerRejected, "server rejected unknown header enum");

  await new Promise<void>((resolve, reject) =>
    server.close((err) => (err ? reject(err) : resolve())),
  );
  console.log("OK");
}

void main();
