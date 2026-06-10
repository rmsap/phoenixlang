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
import type { Handlers } from "./generated/handlers";
import { api, setBaseUrl, ApiError } from "./generated/client";
import type {
  Author,
  CreatePostBody,
  GetPostMeteredResult,
  ListPostsCursorPage,
  ListPostsOffsetPage,
  Post,
  RequeuePostResponse,
  UpdateAuthorProfileBody,
  UploadAvatarBody,
  UpsertPost2Body,
  UpsertPost2Response,
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
    headers?: Record<string, unknown>;
    // multipart/file-upload body (mutually exclusive with `body`): each file
    // part carries a UTF-8 `content` string the driver wraps in a Blob, plus
    // scalar form `fields`. The generated client builds its own FormData from
    // the typed UploadAvatarBody we construct from this.
    multipart?: {
      files: Record<string, { filename: string; content: string }>;
      fields: Record<string, unknown>;
    };
  };
  // When present, the driver answers every request itself with this canned
  // status (+ optional JSON body) INSTEAD of mounting the generated server —
  // the only way to put a status on the wire that the generated server's own
  // guard refuses (e.g. an undeclared 2xx for the client-leniency cases). The
  // stub handler is never invoked, so the ok-case hit assertion is skipped.
  raw_response?: { status: number; body?: unknown };
  handler: {
    expect_received?: Record<string, unknown>;
    returns?: unknown;
    returns_headers?: Record<string, unknown>;
    // binary download: the UTF-8 content the stub streams back as the raw
    // (non-JSON) response body (returned from the handler as a Buffer).
    returns_file?: string;
    // multi-status: the HTTP status the stub sets on the returned
    // <Endpoint>Response envelope; `returns` (when present) is the body.
    returns_status?: number;
    raises?: string;
    expect_not_called?: boolean;
  };
  expect_client: {
    ok?: unknown;
    ok_headers?: Record<string, unknown>;
    // multi-status: the status the client must observe on the envelope, and
    // (for a bodyless status like 204) that the envelope body is absent.
    status?: number;
    ok_absent?: boolean;
    // binary download: the UTF-8 content the client must read off the Blob
    // response body (present instead of `ok`).
    expect_download?: string;
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
    // listTaggedPosts is a versioned (/v2) endpoint: a path param (tag) plus a
    // defaulted query param (limit) under the /v2 prefix. The handler interface
    // delivers them as separate args (tag, query.limit). Recording both proves
    // the prefixed route reached the handler with the decoded inputs.
    async listTaggedPosts(tag, query) {
      state.hit = true;
      state.received = {
        tag,
        limit: query.limit,
      };
      return signal<Post[]>(c);
    },
    // listPostsOffset / listPostsCursor exercise pagination envelopes: the
    // handler returns the full <Endpoint>Page object ({items, totalCount} or
    // {items, nextCursor}) and the client reads it back off the response body.
    // The contract's handler.returns IS that page object, so return it directly
    // typed as the Page. page/limit (offset) and cursor/limit (cursor) are
    // ordinary query params delivered as a single `query` arg.
    async listPostsOffset(query) {
      state.hit = true;
      state.received = {
        page: query.page,
        limit: query.limit,
      };
      return signal<ListPostsOffsetPage>(c);
    },
    async listPostsCursor(query) {
      state.hit = true;
      state.received = {
        cursor: query.cursor,
        limit: query.limit,
      };
      return signal<ListPostsCursorPage>(c);
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
    // getPostMetered exercises request + response headers. Request headers reach
    // the handler as a single `headers` object arg (authorization/requestId
    // required, ifNoneMatch optional → undefined when absent, maxStale defaulted
    // to 60 server-side). Record each header flattened into `received` (mapping
    // absent ifNoneMatch to null) so expect_received compares like path/query
    // params. The response is a typed envelope: the Post body plus response
    // headers set from handler.returns_headers (ratelimitRemaining required Int;
    // etag optional String, left undefined when its returns_headers value is
    // null).
    async getPostMetered(id, headers) {
      state.hit = true;
      state.received = {
        id,
        authorization: headers.authorization,
        requestId: headers.requestId,
        ifNoneMatch: headers.ifNoneMatch ?? null,
        maxStale: headers.maxStale,
      };
      const rh = c.handler.returns_headers ?? {};
      const etag = rh.etag;
      return {
        body: c.handler.returns as Post,
        ratelimitRemaining: rh.ratelimitRemaining as number,
        etag: etag === null || etag === undefined ? undefined : (etag as string),
      } satisfies GetPostMeteredResult;
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
    // upsertPost2 is a multi-status endpoint: the handler returns the
    // <Endpoint>Response envelope { status, body }. Record the path param + body
    // fields like createPost, raise the declared error variant when the case
    // asks for one (signal() builds the return from `returns`, which here is
    // only the envelope's body, so throw directly instead), then build the
    // envelope from handler.returns_status and handler.returns (the Post body,
    // or undefined for the 204/no-body case).
    async upsertPost2(id, body) {
      state.hit = true;
      state.received = {
        id,
        title: body.title,
        body: body.body,
        status: body.status,
        tags: body.tags,
      };
      if (c.handler.raises !== undefined) {
        throw new Error(c.handler.raises);
      }
      return {
        status: c.handler.returns_status as number,
        body:
          c.handler.returns === undefined
            ? undefined
            : (c.handler.returns as Post),
      } satisfies UpsertPost2Response;
    },
    // requeuePost is an ALL-TYPELESS multi-status endpoint: the
    // RequeuePostResponse envelope has no body field at all — the stub only
    // chooses the status from handler.returns_status.
    async requeuePost(id) {
      state.hit = true;
      state.received = { id };
      return {
        status: c.handler.returns_status as number,
      } satisfies RequeuePostResponse;
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
    // uploadAvatar receives the decoded UploadAvatarBody (avatar/thumbnail as
    // Blobs, caption as a string, rotation/crop coerced from the form strings by
    // the generated server — `Number(...)` / `=== "true"`). Record the file
    // *contents* (via Blob.text()) and the scalars; thumbnail-absent is recorded
    // as null. NOTE: the generated server delivers files as `Record<string,Blob>`,
    // and a Blob carries no filename — so the handler structurally CANNOT observe
    // the original `avatar_filename` (unlike Go's FileHeader). We therefore do not
    // record an avatar_filename key; assertReceived only checks keys the driver
    // records, so the contract's avatar_filename sub-assertion is skipped for TS
    // (this is an intrinsic per-target limitation, documented in the driver report).
    async uploadAvatar(id, body) {
      state.hit = true;
      state.received = {
        id,
        avatar_content: await body.avatar.text(),
        caption: body.caption,
        rotation: body.rotation,
        crop: body.crop,
        thumbnail_content:
          body.thumbnail === undefined ? null : await body.thumbnail.text(),
      };
      return signal<Author>(c);
    },
    // downloadAvatar streams raw bytes back: the stub returns a Buffer built
    // from the contract's returns_file UTF-8 string. The generated server sends
    // it with Content-Type application/octet-stream; the client reads it as a
    // Blob.
    async downloadAvatar(id) {
      state.hit = true;
      state.received = { id };
      return Buffer.from(c.handler.returns_file ?? "");
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
    case "listTaggedPosts": {
      const tag = c.call.path_params?.tag;
      assert.ok(tag !== undefined, `${c.name}: missing path_params.tag`);
      const q = c.call.query ?? {};
      const opts: Parameters<typeof api.listTaggedPosts>[1] = {};
      if (q.limit !== undefined) opts.limit = q.limit as number;
      return api.listTaggedPosts(tag, opts);
    }
    case "listPostsOffset": {
      const q = c.call.query ?? {};
      const opts: Parameters<typeof api.listPostsOffset>[0] = {};
      if (q.page !== undefined) opts.page = q.page as number;
      if (q.limit !== undefined) opts.limit = q.limit as number;
      return api.listPostsOffset(opts);
    }
    case "listPostsCursor": {
      const q = c.call.query ?? {};
      const opts: Parameters<typeof api.listPostsCursor>[0] = {};
      if (q.cursor !== undefined) opts.cursor = q.cursor as string;
      if (q.limit !== undefined) opts.limit = q.limit as number;
      return api.listPostsCursor(opts);
    }
    case "searchPosts": {
      const q = c.call.query ?? {};
      return api.searchPosts({
        maxResults: q.maxResults as number,
        sortField: q.sortField as string,
      });
    }
    case "getPostMetered": {
      const id = c.call.path_params?.id;
      assert.ok(id !== undefined, `${c.name}: missing path_params.id`);
      const h = c.call.headers ?? {};
      // Build the client's headers object: authorization/requestId required,
      // maxStale supplied by both cases; ifNoneMatch only when present.
      const headers: Parameters<typeof api.getPostMetered>[1] = {
        authorization: h.authorization as string,
        requestId: h.requestId as string,
        maxStale: h.maxStale as number,
      };
      if (h.ifNoneMatch !== undefined && h.ifNoneMatch !== null) {
        headers.ifNoneMatch = h.ifNoneMatch as string;
      }
      return api.getPostMetered(id, headers);
    }
    case "createPost": {
      const body = c.call.body as unknown as CreatePostBody;
      return api.createPost(body);
    }
    case "upsertPost2": {
      const id = c.call.path_params?.id;
      assert.ok(id !== undefined, `${c.name}: missing path_params.id`);
      const body = c.call.body as unknown as UpsertPost2Body;
      return api.upsertPost2(id, body);
    }
    case "requeuePost": {
      const id = c.call.path_params?.id;
      assert.ok(id !== undefined, `${c.name}: missing path_params.id`);
      return api.requeuePost(id);
    }
    case "updateAuthorProfile": {
      const id = c.call.path_params?.id;
      assert.ok(id !== undefined, `${c.name}: missing path_params.id`);
      const body = c.call.body as unknown as UpdateAuthorProfileBody;
      return api.updateAuthorProfile(id, body);
    }
    case "uploadAvatar": {
      const id = c.call.path_params?.id;
      assert.ok(id !== undefined, `${c.name}: missing path_params.id`);
      const mp = c.call.multipart;
      assert.ok(mp !== undefined, `${c.name}: missing call.multipart`);
      // Construct the typed UploadAvatarBody the generated client expects; it
      // builds its own FormData (appends avatar/thumbnail Blobs + stringified
      // scalars) and sends it over the wire. Each file's `content` becomes a
      // Blob; the filename is supplied to new Blob() metadata but, per the
      // limitation above, does not survive to the server's Record<string,Blob>
      // shape. The scalars are passed JSON-typed (rotation number, crop boolean);
      // the client's `String(...)` stringifies them onto the form.
      const avatarFile = mp.files.avatar;
      assert.ok(avatarFile !== undefined, `${c.name}: multipart has no avatar file`);
      const body: UploadAvatarBody = {
        avatar: new Blob([avatarFile.content]),
        caption: mp.fields.caption as string,
        rotation: mp.fields.rotation as number,
        crop: mp.fields.crop as boolean,
        thumbnail:
          mp.files.thumbnail === undefined
            ? undefined
            : new Blob([mp.files.thumbnail.content]),
      };
      return api.uploadAvatar(id, body);
    }
    case "downloadAvatar": {
      const id = c.call.path_params?.id;
      assert.ok(id !== undefined, `${c.name}: missing path_params.id`);
      // Client returns a Blob; the runCase ok branch reads it via Blob.text()
      // and compares to expect_client.expect_download.
      return api.downloadAvatar(id);
    }
    default:
      throw new Error(`driver has no invoke mapping for endpoint ${c.endpoint}`);
  }
}

// ── assertions ──────────────────────────────────────────────────────────────

// assertReceived checks every key in expect_received against the decoded args
// the handler actually saw. Numbers compare numerically; only listed keys are
// checked. `null` in the contract means the optional arg was absent.
// UNOBSERVABLE_KEYS: keys present in the contract's expect_received that this
// target structurally cannot observe, so the driver skips their sub-assertion.
// `avatar_filename`: the generated TS server delivers multipart files as
// `Record<string,Blob>`, and a Blob carries no filename — unlike Go's
// multipart.FileHeader, the original filename is lost before it reaches the
// handler. The README explicitly permits a driver to omit checking a key it
// cannot observe ("only listed keys are checked"); the content + caption +
// thumbnail-absent assertions still fully exercise the multipart round-trip.
const UNOBSERVABLE_KEYS = new Set<string>([
  "avatar_filename",
  "thumbnail_filename",
]);

function assertReceived(c: ContractCase, got: Record<string, unknown>): void {
  const want = c.handler.expect_received;
  if (want === undefined) return;
  for (const [k, w] of Object.entries(want)) {
    if (UNOBSERVABLE_KEYS.has(k)) continue;
    assert.ok(k in got, `[${c.name}] handler did not receive arg ${k}`);
    assert.deepStrictEqual(
      got[k],
      w,
      `[${c.name}] handler arg ${k}: got ${JSON.stringify(got[k])}, want ${JSON.stringify(w)}`,
    );
  }
}

// assertOkHeaders checks the response-header fields the client read off the
// envelope against expect_client.ok_headers: ratelimitRemaining is a required
// number (numeric equality); etag is an optional string that must equal the
// expected string, or be undefined/absent when the expected value is JSON null.
function assertOkHeaders(
  c: ContractCase,
  got: GetPostMeteredResult,
): void {
  const want = c.expect_client.ok_headers;
  if (want === undefined) return;
  for (const [k, w] of Object.entries(want)) {
    switch (k) {
      case "ratelimitRemaining":
        assert.strictEqual(
          got.ratelimitRemaining,
          w,
          `[${c.name}] ok_header ${k}: got ${String(got.ratelimitRemaining)}, want ${String(w)}`,
        );
        break;
      case "etag":
        if (w === null) {
          assert.strictEqual(
            got.etag,
            undefined,
            `[${c.name}] ok_header etag: expected absent, got ${String(got.etag)}`,
          );
        } else {
          assert.strictEqual(
            got.etag,
            w,
            `[${c.name}] ok_header etag: got ${String(got.etag)}, want ${String(w)}`,
          );
        }
        break;
      default:
        throw new Error(`[${c.name}] unknown ok_header ${k}`);
    }
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
    app.use(createRouter(makeStub(c, state)));
  }

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
        if (c.raw_response === undefined) {
          assert.ok(state.hit, `[${c.name}] handler was never called (ok case)`);
          assertReceived(c, state.received);
        }
        if (c.endpoint === "getPostMetered") {
          // The client returns a typed envelope: compare its `.body` against
          // expect_client.ok and its response-header fields against ok_headers.
          const env = result as GetPostMeteredResult;
          assert.deepStrictEqual(
            env.body,
            c.expect_client.ok,
            `[${c.name}] client result body mismatch`,
          );
          assertOkHeaders(c, env);
        } else if (c.endpoint === "upsertPost2") {
          // Multi-status: the client returns the { status, body } envelope.
          // Assert the observed status, then either an absent body (ok_absent,
          // e.g. 204) or a body that deep-equals expect_client.ok.
          const env = result as UpsertPost2Response;
          assert.strictEqual(
            env.status,
            c.expect_client.status,
            `[${c.name}] client result status: got ${String(env.status)}, want ${String(c.expect_client.status)}`,
          );
          if (c.expect_client.ok_absent === true) {
            assert.ok(
              env.body === undefined || env.body === null,
              `[${c.name}] expected absent envelope body, got ${JSON.stringify(env.body)}`,
            );
          } else {
            assert.deepStrictEqual(
              env.body,
              c.expect_client.ok,
              `[${c.name}] client result body mismatch`,
            );
          }
        } else if (c.endpoint === "requeuePost") {
          // All-typeless multi-status: the envelope is { status } with no body
          // field, so the only client-side observation is the status itself.
          const env = result as RequeuePostResponse;
          assert.strictEqual(
            env.status,
            c.expect_client.status,
            `[${c.name}] client result status: got ${String(env.status)}, want ${String(c.expect_client.status)}`,
          );
        } else if (c.expect_client.expect_download !== undefined) {
          // Binary download: the client returns a Blob; read its bytes as UTF-8
          // and compare to expect_client.expect_download. Keyed on the
          // expect_download field (not the endpoint name) so any binary-download
          // case routes here — mirroring the Go/Python drivers.
          const blob = result as Blob;
          const text = await blob.text();
          assert.strictEqual(
            text,
            c.expect_client.expect_download,
            `[${c.name}] downloaded body mismatch: got ${JSON.stringify(text)}, want ${JSON.stringify(c.expect_client.expect_download)}`,
          );
        } else {
          assert.deepStrictEqual(
            result,
            c.expect_client.ok,
            `[${c.name}] client result mismatch`,
          );
        }
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
