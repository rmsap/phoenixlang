// Behavioral round-trip driver for the TypeScript target — Express mount.
//
// This program is committed source. The Rust harness
// (`crates/phoenix-codegen/tests/roundtrip.rs`) assembles a runnable project at
// test time:
//   - the generated client/server/types/handlers in ./generated/,
//   - this file (driver.ts) + contract.json next to it,
//   - the committed package.json + lockfile + node_modules (express + tsx).
//
// The framework-INDEPENDENT round-trip logic lives in cases.ts (the stub +
// client dispatch) and run.ts (the per-case run loop + result assertions). This
// file provides only the Express-specific server "mount" (`expressMount`) and
// hands it to run.ts's `main`, so a sibling Fastify driver can reuse the same
// core by supplying its own mount. The mount, for each case:
//   - stands up an express app (with express.json() body parsing and multer
//     multipart parsing for the avatar upload route) mounting
//     `createRouter(stub)` — or, for a raw_response case, answers with the
//     canned status/body instead — and `app.listen(0)` for an ephemeral port;
// run.ts then points the generated fetch client at it and asserts the
// round-trip (see run.ts / cases.ts for the per-case behavior).
//
// Exits nonzero on the first failure; the Rust harness asserts exit 0.

import express from "express";
// `multer` (memory storage) is the standard Express multipart/form-data parser.
// The generated server reads the upload off a `MultipartRequest` shape
// (`req.body: Record<string,string>` for scalar fields, `req.files:
// Record<string,Blob>` for file parts) — it casts `req as unknown as
// MultipartRequest` and assumes some middleware populated those. multer gives us
// `req.body` (scalars) for free and `req.files` as arrays of `{ buffer: Buffer,
// originalname, ... }`; the `multipartToBlobFiles` middleware below flattens
// that into the `Record<string,Blob>` the generated server expects (wrapping
// each Buffer as a Blob — note the original filename is therefore dropped, see
// the avatar_filename note in the upload stub). This makes the round-trip
// exercise real multipart over the wire rather than a hand-rolled stand-in.
import multer from "multer";
import type { Request as ExpressRequest, Response as ExpressResponse, NextFunction } from "express";
import type { AddressInfo } from "node:net";

import { createRouter } from "./generated/server";

import { main, type Mount } from "./run";

// ── express mount ─────────────────────────────────────────────────────────────

const expressMount: Mount = async (c, stub) => {
  const app = express();
  // Disable Express's automatic ETag generation: it would set a response
  // `ETag` header on `res.json()` even when the generated server leaves the
  // typed `etag` response header unset, which would leak into the client's
  // observed `etag` envelope field and break the null/absent-etag contract case.
  app.set("etag", false);
  app.use(express.json());
  // Multipart parsing for the upload route. multer (memory storage) populates
  // `req.body` (scalar fields) and `req.files` (file parts as arrays of
  // { buffer, originalname, ... }). The generated server, however, expects
  // `req.files` to be a Record<string,Blob>, so `multipartToBlobFiles` flattens
  // multer's per-field arrays into single Blobs keyed by field name (each
  // Buffer wrapped as a Blob). The filename is intentionally dropped here — the
  // generated server's Blob shape has no place for it (see upload-stub note).
  // We only enable multer for routes that actually carry multipart, so the
  // JSON/header routes are unaffected.
  const upload = multer({ storage: multer.memoryStorage() }).fields([
    { name: "avatar", maxCount: 1 },
    { name: "thumbnail", maxCount: 1 },
  ]);
  const multipartToBlobFiles = (
    req: ExpressRequest,
    _res: ExpressResponse,
    next: NextFunction,
  ): void => {
    const multerFiles = req.files as
      | Record<string, Express.Multer.File[]>
      | undefined;
    const blobFiles: Record<string, Blob> = {};
    if (multerFiles) {
      for (const [field, parts] of Object.entries(multerFiles)) {
        const part = parts[0];
        if (part) blobFiles[field] = new Blob([part.buffer]);
      }
    }
    // Overwrite req.files with the Record<string,Blob> the generated server
    // casts to. (`req.body` already holds the scalar fields from multer.)
    (req as unknown as { files: Record<string, Blob> }).files = blobFiles;
    next();
  };
  app.post("/api/authors/:id/avatar", upload, multipartToBlobFiles);
  if (c.raw_response !== undefined) {
    // raw_response case: bypass the generated server entirely and answer with
    // the canned status/body (see the ContractCase field comment).
    const raw = c.raw_response;
    app.use((_req: ExpressRequest, res: ExpressResponse) => {
      if (raw.body !== undefined) {
        res.status(raw.status).json(raw.body);
      } else {
        res.status(raw.status).end();
      }
    });
  } else {
    app.use(createRouter(stub));
  }

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

void main(expressMount);
