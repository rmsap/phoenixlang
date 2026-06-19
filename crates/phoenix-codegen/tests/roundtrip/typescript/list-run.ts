// Framework-independent list-valued-param round-trip core (TypeScript target).
//
// Holds everything about the list round-trip that does NOT depend on a particular
// HTTP server framework: the `Handlers` stub (echoes each list back), the full set
// of assertions, and the raw-`fetch` reject-path checks. The only framework-specific
// dependency — standing up Express vs Fastify — is injected as a `ListMount`, so
// `list-driver.ts` (Express) and `list-driver-fastify.ts` (Fastify) each supply just
// their mount and reuse this loop. This matters for the query path specifically: the
// generated coercion is the same `toStringArray(...).map(...)` for both frameworks,
// but Express and Fastify deliver repeated query keys (`?ids=a&ids=b`) as an array
// via DIFFERENT parsers, so both must be driven behaviorally to prove the array
// shape `toStringArray` normalizes actually arrives.
//
// It proves list-valued params survive the wire: query params (repeated keys,
// normalized server-side by `toStringArray`) and request headers (comma-joined on
// send, split + coerced per element server-side via `splitHeaderList(...).map(...)`),
// BOTH covering every element type — `String`/`Int`/`Uuid`/`Status` (a simple
// enum)/`Float`/`Bool`/`DateTime`/`Decimal` — echo back unchanged, including the
// empty list. The reject path is pinned via the query elements: the enum query
// element (an unknown variant smuggled past the typed signature with a cast) must
// fail the server's per-element `parseStatus`, and a malformed `Uuid` query element
// its `parseUuid` (each throws `ValidationError` → 400).

import { api, setBaseUrl } from "./generated-list/client";
import type { Handlers } from "./generated-list/handlers";
import type { Decimal, Echo, Status, Uuid } from "./generated-list/types";

// A ListMount stands up the generated server for the list schema and returns its
// base URL plus a close hook. The framework-specific driver supplies this.
export type ListMount = (
  stub: Handlers,
) => Promise<{ baseUrl: string; close: () => Promise<void> }>;

function check(cond: boolean, msg: string): void {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

const eq = (a: unknown, b: unknown): boolean =>
  JSON.stringify(a) === JSON.stringify(b);

const uuidA = "11111111-1111-1111-1111-111111111111";
const uuidB = "22222222-2222-2222-2222-222222222222";

const stub: Handlers = {
  search(
    query: {
      ids: string[];
      counts: number[];
      uuids: Uuid[];
      statuses: Status[];
      qFloats: number[];
      qFlags: boolean[];
      qTimes: Date[];
      qAmounts: Decimal[];
    },
    headers: {
      roles: string[];
      limits: number[];
      keys: Uuid[];
      ratios: number[];
      flags: boolean[];
      times: Date[];
      amounts: Decimal[];
      tags: Status[];
    },
  ): Promise<Echo> {
    return Promise.resolve({
      ids: query.ids,
      counts: query.counts,
      uuids: query.uuids,
      statuses: query.statuses,
      qFloats: query.qFloats,
      qFlags: query.qFlags,
      qTimes: query.qTimes,
      qAmounts: query.qAmounts,
      roles: headers.roles,
      limits: headers.limits,
      keys: headers.keys,
      ratios: headers.ratios,
      flags: headers.flags,
      times: headers.times,
      amounts: headers.amounts,
      tags: headers.tags,
    });
  },
};

export async function runList(mount: ListMount): Promise<void> {
  const { baseUrl, close } = await mount(stub);
  setBaseUrl(baseUrl);

  try {
    const t1 = new Date("2024-01-15T08:30:00Z");
    const t2 = new Date("2024-02-20T16:45:00Z");

    // Multiple elements in each position round-trip in order. Both query and
    // header cover every list element type (String/Int/Uuid/enum/Float/Bool/
    // DateTime/Decimal): the query block exercises the repeated-key →
    // `toStringArray` coerce path, the header block the comma-split → per-element
    // coerce path.
    const echo = await api.search(
      {
        ids: ["a", "b", "c"],
        counts: [1, 2, 3],
        uuids: [uuidA, uuidB] as Uuid[],
        statuses: ["Active", "Pending"],
        qFloats: [0.5, 1.25],
        qFlags: [true, false],
        qTimes: [t1, t2],
        qAmounts: ["7.75", "8.00"] as Decimal[],
      },
      {
        roles: ["admin", "editor"],
        limits: [10, 20],
        keys: [uuidA, uuidB] as Uuid[],
        ratios: [1.5, 2.5],
        flags: [true, false],
        times: [t1, t2],
        amounts: ["10.50", "3.25"] as Decimal[],
        tags: ["Active", "Inactive"],
      },
    );
    // Query lists.
    check(eq(echo.ids, ["a", "b", "c"]), "ids round-tripped");
    check(eq(echo.counts, [1, 2, 3]), "counts round-tripped");
    check(eq(echo.uuids, [uuidA, uuidB]), "uuids round-tripped");
    check(eq(echo.statuses, ["Active", "Pending"]), "statuses round-tripped");
    check(eq(echo.qFloats, [0.5, 1.25]), "qFloats query round-tripped");
    check(eq(echo.qFlags, [true, false]), "qFlags query round-tripped");
    // `qTimes` revive to `Date`; compare instants (see `times` below).
    check(
      echo.qTimes.length === 2 &&
        echo.qTimes[0].getTime() === t1.getTime() &&
        echo.qTimes[1].getTime() === t2.getTime(),
      "qTimes query round-tripped",
    );
    check(eq(echo.qAmounts, ["7.75", "8.00"]), "qAmounts query round-tripped");
    // Header lists.
    check(eq(echo.roles, ["admin", "editor"]), "roles header round-tripped");
    check(eq(echo.limits, [10, 20]), "limits header round-tripped");
    check(eq(echo.keys, [uuidA, uuidB]), "keys header round-tripped");
    check(eq(echo.ratios, [1.5, 2.5]), "ratios header round-tripped");
    check(eq(echo.flags, [true, false]), "flags header round-tripped");
    // `times` revive to `Date`; compare instants so a `.000Z` rendering quirk
    // never makes an equal instant compare unequal.
    check(
      echo.times.length === 2 &&
        echo.times[0].getTime() === t1.getTime() &&
        echo.times[1].getTime() === t2.getTime(),
      "times header round-tripped",
    );
    check(eq(echo.amounts, ["10.50", "3.25"]), "amounts header round-tripped");
    check(eq(echo.tags, ["Active", "Inactive"]), "tags header round-tripped");

    // Empty lists round-trip as empty.
    const empty = await api.search(
      {
        ids: [],
        counts: [],
        uuids: [],
        statuses: [],
        qFloats: [],
        qFlags: [],
        qTimes: [],
        qAmounts: [],
      },
      {
        roles: [],
        limits: [],
        keys: [],
        ratios: [],
        flags: [],
        times: [],
        amounts: [],
        tags: [],
      },
    );
    check(
      empty.ids.length === 0 &&
        empty.counts.length === 0 &&
        empty.uuids.length === 0 &&
        empty.statuses.length === 0 &&
        empty.qFloats.length === 0 &&
        empty.qFlags.length === 0 &&
        empty.qTimes.length === 0 &&
        empty.qAmounts.length === 0 &&
        empty.roles.length === 0 &&
        empty.limits.length === 0 &&
        empty.keys.length === 0 &&
        empty.ratios.length === 0 &&
        empty.flags.length === 0 &&
        empty.times.length === 0 &&
        empty.amounts.length === 0 &&
        empty.tags.length === 0,
      "empty lists round-tripped",
    );

    // Reject path: an unknown enum element must fail the server's per-element
    // parseStatus (throws ValidationError → 400 → client throws). The cast smuggles
    // a bad value past the typed signature so the server coercion is what rejects it.
    let rejected = false;
    try {
      await api.search(
        {
          ids: ["a"],
          counts: [1],
          uuids: [uuidA] as Uuid[],
          statuses: ["Bogus" as unknown as Status],
          qFloats: [1.5],
          qFlags: [true],
          qTimes: [t1],
          qAmounts: ["1.00"] as Decimal[],
        },
        {
          roles: ["admin"],
          limits: [10],
          keys: [uuidA] as Uuid[],
          ratios: [1.5],
          flags: [true],
          times: [t1],
          amounts: ["10.50"] as Decimal[],
          tags: ["Active"],
        },
      );
    } catch {
      rejected = true;
    }
    check(rejected, "server rejected unknown enum list element");

    // Reject path: a malformed Uuid element must fail the server's per-element
    // parseUuid format check (throws ValidationError → 400). Issue a raw GET (the
    // typed client throws a generic error that hides the status) so the assertion
    // can pin the exact 400, matching Go's per-element format check.
    const badUuid = await fetch(`${baseUrl}/search?uuids=not-a-uuid`);
    check(badUuid.status === 400, "malformed uuid list element rejected with 400");
  } finally {
    await close();
  }

  console.log("OK");
}
