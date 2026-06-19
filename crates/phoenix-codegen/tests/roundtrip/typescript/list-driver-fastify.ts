// Behavioral list-valued-param round-trip driver for the TypeScript **Fastify**
// server target.
//
// Sibling of list-driver.ts (the Express driver). The framework-independent core —
// the `Handlers` stub, the full assertions, and the reject-path checks — lives in
// list-run.ts and is shared verbatim. This file provides ONLY the Fastify mount: it
// stands up a Fastify instance, registers the generated `createRouter` plugin (a
// `FastifyPluginCallback`), listens on an ephemeral port, and returns the base URL +
// a close handle. The Rust harness (`roundtrip.rs::list_typescript_roundtrip`) writes
// the FASTIFY-generated server into ./generated-list/ before running this driver via
// tsx. The schema has no uploads, so (unlike driver-fastify.ts) no multipart hook is
// needed. This exists to prove Fastify's query parser delivers repeated keys
// (`?ids=a&ids=b`) as the array `toStringArray` normalizes, independent of Express.

import Fastify from "fastify";
import type { AddressInfo } from "node:net";

import { createRouter } from "./generated-list/server";
import { runList, type ListMount } from "./list-run";

const fastifyMount: ListMount = async (stub) => {
  const app = Fastify();
  await app.register(createRouter(stub));
  await app.listen({ port: 0, host: "127.0.0.1" });
  const addr = app.server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${String(addr.port)}`,
    close: () => app.close(),
  };
};

void runList(fastifyMount);
