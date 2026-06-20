// Behavioral Url/Bytes round-trip driver for the TypeScript target (Express).
//
// Committed source. The Rust harness (`roundtrip.rs::url_bytes_typescript_roundtrip`)
// generates the small Url/Bytes schema into ./generated-url-bytes/ (a separate dir
// from the other TS round-trips, so they never race), then runs this via `tsx`. It
// proves:
//   - `Bytes` is a first-class binary value (`Uint8Array`): a field set from raw
//     binary (including non-UTF-8 bytes 0x00/0xFF/0xFE/0x80) survives the base64
//     wire as a `Uint8Array` with the SAME bytes — proving `encodeBytes` on send +
//     `bytesFromBase64` revival, NOT a UTF-8 string — across a required field, a
//     present/absent `Option`, a `List`, and a `Map<String, Bytes>` (`Record`), in
//     request body and echoed response. The `Map` exercises the `encodeBytes`
//     deep-walk over a `Record` and the `Object.fromEntries` revival. The server
//     stub asserts the body arrived REVIVED (real `Uint8Array`), not the base64
//     string.
//   - `Url` is a branded validated string that round-trips byte-for-byte (never
//     normalized) through a body field / `Option` / `List`, a query param, a
//     `List<Url>` query param, and a request header — all echoed into the response.
//   - a MULTI-STATUS endpoint (`response { }` block) round-trips a Bytes-bearing
//     shared body through the `{ status, body }` envelope — exercising the
//     `encodeBytes` wrap on the server's `result.body` branch and the client's
//     revival of the envelope body.
//   - the reject path: a malformed `Url` query value fails the server's `parseUrl`
//     (ValidationError → 400).
// Exits nonzero on failure.

import express from "express";
import type { AddressInfo } from "node:net";

import { api, setBaseUrl } from "./generated-url-bytes/client";
import type { Handlers } from "./generated-url-bytes/handlers";
import { createRouter } from "./generated-url-bytes/server";
import type {
  Echo,
  Payload,
  ReplaceBody,
  ReplaceResponse,
  UploadBody,
  Url,
} from "./generated-url-bytes/types";

function check(cond: boolean, msg: string): void {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

const bytesEq = (a: Uint8Array, b: Uint8Array): boolean =>
  a.length === b.length && a.every((x, i) => x === b[i]);

// Compares two `Map<String, Bytes>` (`Record<string, Uint8Array>`) for equal keys
// and per-value bytes.
const tagsEq = (
  a: Record<string, Uint8Array>,
  b: Record<string, Uint8Array>,
): boolean => {
  const ak = Object.keys(a).sort();
  const bk = Object.keys(b).sort();
  return (
    ak.length === bk.length &&
    ak.every((k, i) => k === bk[i] && bytesEq(a[k], b[k]))
  );
};

// Raw binary with non-UTF-8 bytes: a wrong base64 round-trip would corrupt these.
const CHECKSUM = new Uint8Array([0x00, 0x01, 0xff, 0xfe, 0x80]);
const SIGNATURE = new Uint8Array([0xca, 0xfe, 0x00, 0xba, 0xbe]);
const CHUNK_A = new Uint8Array([0xde, 0xad]);
const CHUNK_B = new Uint8Array([0xbe, 0xef, 0x00]);
// A `Map<String, Bytes>` — every value must survive the base64 wire as raw binary.
const TAGS: Record<string, Uint8Array> = { a: CHUNK_A, b: CHUNK_B };

// URLs with query string + fragment + a non-lowercased host: validated, not
// normalized, so they must come back verbatim.
const SOURCE = "https://Example.com/a/b?x=1&y=2#frag" as Url;
const MIRROR = "ftp://mirror.example.org/pub/file.bin" as Url;
const THUMB_A = "https://t.example/1.png" as Url;
const THUMB_B = "https://t.example/2.png" as Url;
const ORIGIN = "https://origin.example.com/in" as Url;
const MIRROR_Q_A = "https://m1.example.com" as Url;
const MIRROR_Q_B = "https://m2.example.com" as Url;
const REFERER = "https://ref.example.com/page?from=test" as Url;

const stub: Handlers = {
  upload(
    body: UploadBody,
    query: { origin: Url; mirrors: Url[] },
    headers: { referer: Url },
  ): Promise<Echo> {
    // The body's `Bytes` fields must reach the handler REVIVED — a real
    // `Uint8Array`, not the base64 wire string. A missing revival would leave a
    // string here (a runtime type lie), so assert it.
    check(
      body.checksum instanceof Uint8Array,
      "server: body.checksum revived to Uint8Array",
    );
    check(
      body.signature === undefined || body.signature instanceof Uint8Array,
      "server: body.signature revived to Uint8Array",
    );
    check(
      body.chunks.every((c) => c instanceof Uint8Array),
      "server: body.chunks[] revived to Uint8Array",
    );
    check(
      Object.values(body.tags).every((v) => v instanceof Uint8Array),
      "server: body.tags values revived to Uint8Array",
    );
    return Promise.resolve({
      source: body.source,
      mirror: body.mirror,
      thumbnails: body.thumbnails,
      checksum: body.checksum,
      signature: body.signature,
      chunks: body.chunks,
      tags: body.tags,
      origin: query.origin,
      mirrors: query.mirrors,
      referer: headers.referer,
    });
  },
  // A MULTI-STATUS endpoint: the handler echoes the Bytes-bearing shared body into
  // the `{ status, body }` envelope and picks status 200. The generated server
  // wraps `result.body` in `encodeBytes` before sending, and the client revives the
  // envelope body — so the Bytes/Map round-trip is exercised through that path.
  replace(_id: string, body: ReplaceBody): Promise<ReplaceResponse> {
    check(
      body.checksum instanceof Uint8Array,
      "server(replace): body.checksum revived to Uint8Array",
    );
    const echoed: Payload = {
      source: body.source,
      mirror: body.mirror,
      thumbnails: body.thumbnails,
      checksum: body.checksum,
      signature: body.signature,
      chunks: body.chunks,
      tags: body.tags,
    };
    return Promise.resolve({ status: 200, body: echoed });
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
  const baseUrl = `http://127.0.0.1:${String(port)}`;
  setBaseUrl(baseUrl);

  try {
    // Call 1: optional fields present.
    const echo = await api.upload(
      {
        source: SOURCE,
        mirror: MIRROR,
        thumbnails: [THUMB_A, THUMB_B],
        checksum: CHECKSUM,
        signature: SIGNATURE,
        chunks: [CHUNK_A, CHUNK_B],
        tags: TAGS,
      },
      { origin: ORIGIN, mirrors: [MIRROR_Q_A, MIRROR_Q_B] },
      { referer: REFERER },
    );

    // Bytes round-trip as raw binary (Uint8Array, identical bytes).
    check(
      echo.checksum instanceof Uint8Array && bytesEq(echo.checksum, CHECKSUM),
      "checksum round-tripped as binary",
    );
    check(
      echo.signature instanceof Uint8Array && bytesEq(echo.signature, SIGNATURE),
      "optional signature round-tripped as binary",
    );
    check(
      echo.chunks.length === 2 &&
        bytesEq(echo.chunks[0], CHUNK_A) &&
        bytesEq(echo.chunks[1], CHUNK_B),
      "chunks list round-tripped as binary",
    );
    // Map<String, Bytes> round-tripped as binary per value.
    check(tagsEq(echo.tags, TAGS), "tags map round-tripped as binary");
    // Url round-trips byte-for-byte (no normalization).
    check(echo.source === SOURCE, "source url round-tripped verbatim");
    check(echo.mirror === MIRROR, "optional mirror url round-tripped verbatim");
    check(
      echo.thumbnails.length === 2 &&
        echo.thumbnails[0] === THUMB_A &&
        echo.thumbnails[1] === THUMB_B,
      "thumbnails url list round-tripped",
    );
    // Url query / List<Url> query / Url header round-trip.
    check(echo.origin === ORIGIN, "origin query url round-tripped");
    check(
      echo.mirrors.length === 2 &&
        echo.mirrors[0] === MIRROR_Q_A &&
        echo.mirrors[1] === MIRROR_Q_B,
      "mirrors List<Url> query round-tripped",
    );
    check(echo.referer === REFERER, "referer header url round-tripped");

    // Call 2: optional Bytes/Url absent, empty lists.
    const echo2 = await api.upload(
      {
        source: SOURCE,
        mirror: undefined,
        thumbnails: [],
        checksum: CHECKSUM,
        signature: undefined,
        chunks: [],
        tags: {},
      },
      { origin: ORIGIN, mirrors: [] },
      { referer: REFERER },
    );
    check(echo2.mirror === undefined, "absent mirror stays undefined");
    check(echo2.signature === undefined, "absent signature stays undefined");
    check(
      echo2.thumbnails.length === 0 &&
        echo2.chunks.length === 0 &&
        echo2.mirrors.length === 0 &&
        Object.keys(echo2.tags).length === 0,
      "empty lists/map round-tripped empty",
    );
    check(
      echo2.checksum instanceof Uint8Array && bytesEq(echo2.checksum, CHECKSUM),
      "checksum (call 2) round-tripped as binary",
    );

    // Multi-status endpoint: the shared `Payload` body (carrying Bytes + the
    // Map<String, Bytes>) round-trips through the `{ status, body }` envelope —
    // exercising the `encodeBytes` wrap on the server's `result.body` branch and
    // the client's revival of the envelope body.
    const rep = await api.replace("asset-1", {
      source: SOURCE,
      mirror: MIRROR,
      thumbnails: [THUMB_A, THUMB_B],
      checksum: CHECKSUM,
      signature: SIGNATURE,
      chunks: [CHUNK_A, CHUNK_B],
      tags: TAGS,
    });
    check(rep.status === 200, "replace envelope status is 200");
    check(rep.body !== undefined, "replace envelope body present");
    check(
      rep.body !== undefined &&
        rep.body.checksum instanceof Uint8Array &&
        bytesEq(rep.body.checksum, CHECKSUM),
      "replace body checksum round-tripped as binary",
    );
    check(
      rep.body !== undefined && tagsEq(rep.body.tags, TAGS),
      "replace body tags round-tripped as binary",
    );
    check(
      rep.body !== undefined && rep.body.source === SOURCE,
      "replace body source url round-tripped verbatim",
    );

    // Reject path: a malformed Url query value must fail the server's parseUrl
    // (ValidationError → 400). Issue a raw POST so the server-side parse is what
    // rejects it, and pin the exact status.
    const bad = await fetch(`${baseUrl}/assets?origin=not-a-url`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Referer": REFERER },
      body: JSON.stringify({
        source: SOURCE,
        thumbnails: [],
        checksum: "",
        chunks: [],
        tags: {},
      }),
    });
    check(bad.status === 400, "malformed url query rejected with 400");

    // Reject path (List<Url> element): a malformed element in the `mirrors`
    // `List<Url>` query must fail the per-element `parseUrl` (→ 400). Origin and the
    // body are valid here, so the bad element is the only thing that can be rejected.
    const badElem = await fetch(
      `${baseUrl}/assets?origin=${encodeURIComponent(ORIGIN)}` +
        `&mirrors=${encodeURIComponent(MIRROR_Q_A)}&mirrors=not-a-url`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-Referer": REFERER },
        body: JSON.stringify({
          source: SOURCE,
          thumbnails: [],
          checksum: "",
          chunks: [],
          tags: {},
        }),
      },
    );
    check(
      badElem.status === 400,
      "malformed List<Url> query element rejected with 400",
    );

    // Reject path (body): a malformed `Url` in a BODY field must fail the server's
    // `reviveBody` parseUrl (ValidationError → 400). The query is valid here, so the
    // body field is the only thing that can be rejected.
    const badBody = await fetch(
      `${baseUrl}/assets?origin=${encodeURIComponent(ORIGIN)}`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-Referer": REFERER },
        body: JSON.stringify({
          source: "not-a-url",
          thumbnails: [],
          checksum: "",
          chunks: [],
          tags: {},
        }),
      },
    );
    check(badBody.status === 400, "malformed url body field rejected with 400");
  } finally {
    await new Promise<void>((resolve, reject) =>
      server.close((err) => (err ? reject(err) : resolve())),
    );
  }

  console.log("OK");
}

void main();
