// Behavioral round-trip driver for the TypeScript **Fastify** server target.
//
// Sibling of driver.ts (the Express driver). The framework-independent core —
// the `Handlers` stub, the client `invoke` dispatch, the per-case run loop, and
// the result assertions — lives in cases.ts / run.ts / harness.ts and is shared
// verbatim. This file provides ONLY the Fastify `mount`: it stands up a Fastify
// instance, registers the generated `createRouter` plugin (a `FastifyPluginCallback`),
// wires multipart, and returns the base URL + a close handle. The Rust harness
// (`crates/phoenix-codegen/tests/roundtrip.rs`) writes the FASTIFY-generated
// server into ./generated/ before running this driver via tsx.

import multipart from "@fastify/multipart";
import Fastify from "fastify";
import type { AddressInfo } from "node:net";

import { createRouter } from "./generated/server";
import { main, type Mount } from "./run";

// Mounts the generated Fastify plugin on a fresh instance and listens on an
// ephemeral port. For a `raw_response` case the generated server is bypassed
// entirely: a catch-all route answers the canned status/body (the only way to
// put an undeclared 2xx on the wire — the generated server's guard would refuse).
const fastifyMount: Mount = async (c, stub) => {
  // Unlike Express (which auto-sets an `ETag` on `res.json()`, hence the
  // driver.ts `app.set("etag", false)`), Fastify core never generates an ETag,
  // so there is no equivalent to disable — the null/absent-etag contract case
  // works without intervention.
  const app = Fastify();

  if (c.raw_response !== undefined) {
    const raw = c.raw_response;
    // `.send(object)` serializes as application/json, matching the Express
    // driver's `res.json(raw.body)`. Both current raw_response bodies are
    // objects (or absent); if a future case uses a primitive body, switch to an
    // explicit JSON serialization here to stay aligned with `res.json()` (which
    // JSON-encodes primitives, whereas `.send("x")` would send text/plain).
    app.all("/*", async (_request, reply) => {
      if (raw.body !== undefined) {
        return reply.status(raw.status).send(raw.body);
      }
      return reply.status(raw.status).send();
    });
  } else {
    // Multipart: the generated server reads scalar fields off `request.body` and
    // uploaded files off `request.files` (as `Blob`s) via its
    // `request as unknown as MultipartRequest` cast. @fastify/multipart exposes
    // the parts as an async iterator; a preValidation hook drains it and builds
    // exactly that shape, so the generated handler sees the same contract the
    // Express driver's multer adapter produces. (The file's original name is not
    // carried on a Blob — matching the documented per-target limitation.)
    await app.register(multipart);
    app.addHook("preValidation", async (request) => {
      if (!request.isMultipart()) return;
      const body: Record<string, string> = {};
      const files: Record<string, Blob> = {};
      for await (const part of request.parts()) {
        if (part.type === "file") {
          const buf = await part.toBuffer();
          files[part.fieldname] = new Blob([buf]);
        } else {
          body[part.fieldname] = String(part.value);
        }
      }
      (request as unknown as { body: unknown }).body = body;
      (request as unknown as { files: unknown }).files = files;
    });
    await app.register(createRouter(stub));
  }

  await app.listen({ port: 0, host: "127.0.0.1" });
  const addr = app.server.address() as AddressInfo;
  const baseUrl = `http://127.0.0.1:${String(addr.port)}`;
  return { baseUrl, close: () => app.close() };
};

void main(fastifyMount);
