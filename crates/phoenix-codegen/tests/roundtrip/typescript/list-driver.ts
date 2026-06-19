// Behavioral list-valued-param round-trip driver for the TypeScript **Express**
// server target.
//
// Sibling of list-driver-fastify.ts. The framework-independent core — the
// `Handlers` stub, the full assertions, and the reject-path checks — lives in
// list-run.ts and is shared verbatim. This file provides ONLY the Express mount:
// it stands up an Express app, mounts the generated `createRouter`, listens on an
// ephemeral port, and returns the base URL + a close handle. The Rust harness
// (`roundtrip.rs::list_typescript_roundtrip`) writes the EXPRESS-generated server
// into ./generated-list/ before running this driver via tsx.

import express from "express";
import type { AddressInfo } from "node:net";

import { createRouter } from "./generated-list/server";
import { runList, type ListMount } from "./list-run";

const expressMount: ListMount = async (stub) => {
  const app = express();
  app.set("etag", false);
  app.use(express.json());
  app.use(createRouter(stub));
  const server = app.listen(0);
  await new Promise<void>((resolve) => server.once("listening", resolve));
  const port = (server.address() as AddressInfo).port;
  return {
    baseUrl: `http://127.0.0.1:${String(port)}`,
    close: () =>
      new Promise<void>((resolve, reject) =>
        server.close((err) => (err ? reject(err) : resolve())),
      ),
  };
};

void runList(expressMount);
