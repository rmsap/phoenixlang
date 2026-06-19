// Behavioral Decimal round-trip driver for the TypeScript target (Express).
//
// Committed source. The Rust harness (`roundtrip.rs::decimal_typescript_roundtrip`)
// generates the small Decimal schema into ./generated-decimal/ (separate from the
// other round-trips' generated dirs), then runs this via `tsx`. It proves the
// branded `Decimal` alias and the `parseDecimal` validate-on-decode pass
// round-trip exact decimal strings: body decimals (required / optional / list /
// map) come back equal and still typed `string` at runtime (the brand is
// compile-time only); a `Decimal` query param round-trips (echoed by the stub
// into the body); a required response-header decimal arrives; and a bare
// `Decimal` response decodes. It also asserts a malformed body decimal AND a
// malformed query decimal are rejected (TS validates both — the Go driver pins
// the opposite for the query). Exits nonzero on failure.

import express from "express";
import type { AddressInfo } from "node:net";

import { createRouter } from "./generated-decimal/server";
import type { Handlers } from "./generated-decimal/handlers";
import { api, setBaseUrl } from "./generated-decimal/client";
import { parseDecimal } from "./generated-decimal/types";
import type {
  Decimal,
  EchoInvoiceBody,
  GetQuoteResult,
  Invoice,
} from "./generated-decimal/types";

function check(cond: boolean, msg: string): void {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

const computedTax = parseDecimal("8.25");

const stub: Handlers = {
  // Echo the decoded (server-side reviver-validated) body back unchanged.
  echoInvoice(body: EchoInvoiceBody): Promise<Invoice> {
    check(typeof body.subtotal === "string", "server: body.subtotal is a string");
    return Promise.resolve(body as Invoice);
  },
  // Echo the query decimal into `subtotal` so the client can assert it round-tripped.
  getQuote(id: string, query: { minAmount: Decimal }): Promise<GetQuoteResult> {
    return Promise.resolve({
      body: { id: 1, subtotal: query.minAmount, lineTotals: [], rates: {} },
      computedTax,
    });
  },
  exchangeRate(): Promise<Decimal> {
    return Promise.resolve(parseDecimal("1.0825"));
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

  const subtotal = parseDecimal("19.99");
  const discount = parseDecimal("-2.50");
  const a = parseDecimal("10.00");
  const b = parseDecimal("9.99");

  const resp = await api.echoInvoice({
    id: 7,
    subtotal,
    discount,
    lineTotals: [a, b],
    rates: { usd: parseDecimal("1.0"), eur: parseDecimal("0.92") },
  });
  check(typeof resp.subtotal === "string" && resp.subtotal === subtotal, "echo subtotal");
  check(resp.discount === discount, "echo optional discount");
  check(resp.lineTotals.length === 2 && resp.lineTotals[1] === b, "echo list line total");
  check(resp.rates.eur === parseDecimal("0.92"), "echo map rate");

  const minAmount = parseDecimal("5.00");
  const r2 = await api.getQuote("inv-1", { minAmount });
  check(r2.body.subtotal === minAmount, "query Decimal round-tripped through the server");
  check(r2.computedTax === computedTax, "response-header Decimal arrived");

  const rate = await api.exchangeRate();
  check(typeof rate === "string" && rate === parseDecimal("1.0825"), "bare Decimal response decoded");

  // Reject path: a malformed body decimal must fail the server's body reviver
  // (`parseDecimal` throws → 500 → client throws on `!response.ok`). The cast
  // smuggles a bad value past `parseDecimal` so the wire decode rejects it.
  let rejected = false;
  try {
    await api.echoInvoice({
      id: 1,
      subtotal: "not-a-number" as unknown as Decimal,
      lineTotals: [],
      rates: {},
    });
  } catch {
    rejected = true;
  }
  check(rejected, "server rejected malformed body decimal");

  // Reject path (query param): TS validates query/request-header `Decimal`s
  // inline via `parseDecimal` on the server, which throws `ValidationError` → 400
  // (like an enum param), matching Go's `decimalRe` check and Python's FastAPI
  // coercion. Issue a raw GET (rather than the typed client, which throws a generic
  // error that hides the status) so the assertion can pin the exact 400.
  const badQuery = await fetch(
    `http://127.0.0.1:${String(port)}/quote/inv-1?minAmount=not-a-number`,
  );
  check(badQuery.status === 400, "malformed query decimal rejected with 400");

  await new Promise<void>((resolve, reject) =>
    server.close((err) => (err ? reject(err) : resolve())),
  );
  console.log("OK");
}

void main();
