// Framework-independent round-trip case logic shared across server drivers.
//
// This module holds the schema-coupled-but-framework-AGNOSTIC pieces of the
// behavioral round-trip: `makeStub` builds the fixture-driven `Handlers`
// implementation for one case, and `invoke` dispatches the matching generated
// client method with the case inputs. Neither touches a particular HTTP server
// framework, so both the Express and Fastify mounts reuse them via run.ts.

import assert from "node:assert/strict";

import type { Handlers } from "./generated/handlers";
import { api } from "./generated/client";
import type {
  Author,
  Catalog,
  CreatePostBody,
  GetPostMeteredResult,
  ListPostsCursorPage,
  ListPostsOffsetPage,
  Post,
  RequeuePostResponse,
  SyncCatalogBody,
  UpdateAuthorProfileBody,
  UploadAvatarBody,
  UpsertPost2Body,
  UpsertPost2Response,
} from "./generated/types";

import { signal } from "./harness";
import type { CaseState, ContractCase } from "./harness";

// ── fixture-driven stub ─────────────────────────────────────────────────────

// Builds a Handlers implementation for one case. Only the exercised endpoints
// record args + respond; the rest throw so an unexpected route is loud.
export function makeStub(c: ContractCase, state: CaseState): Handlers {
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
    // syncCatalog exercises the three composite shapes (Map<string,string>,
    // List<enum>, List<struct> as a field). The stub echoes the decoded body
    // into the Catalog response, so the existing deep-equal assertOK proves the
    // shapes survive both legs — no per-field expect_received needed.
    async syncCatalog(body) {
      state.hit = true;
      return {
        id: body.id,
        labels: body.labels,
        allowedStatuses: body.allowedStatuses,
        entries: body.entries,
      } satisfies Catalog;
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
export async function invoke(c: ContractCase): Promise<unknown> {
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
    case "syncCatalog": {
      const body = c.call.body as unknown as SyncCatalogBody;
      return api.syncCatalog(body);
    }
    default:
      throw new Error(`driver has no invoke mapping for endpoint ${c.endpoint}`);
  }
}
