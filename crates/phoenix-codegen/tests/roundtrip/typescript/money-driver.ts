// Behavioral Money round-trip driver for the TypeScript target (Express).
//
// Committed source. The Rust harness (`roundtrip.rs::money_typescript_roundtrip`)
// generates the small Money schema into ./generated-money/, then runs this via
// `tsx`. `Money` is the composite `{ amount: Decimal; currency: string }`; the
// proof is it round-trips in a body (required / optional / nested in a list
// element) and as a bare response, that `reviveMoney` rebuilds the branded
// `amount` (so it's still a string carrying the same value), and that the server
// body reviver rejects a bad amount and an unknown currency. Exits nonzero on
// failure.

import express from "express";
import type { AddressInfo } from "node:net";

import { createRouter } from "./generated-money/server";
import type { Handlers } from "./generated-money/handlers";
import { api, setBaseUrl } from "./generated-money/client";
import { parseDecimal } from "./generated-money/types";
import type {
  EchoInvoiceBody,
  Invoice,
  Money,
} from "./generated-money/types";

function check(cond: boolean, msg: string): void {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

function money(amount: string, currency: string): Money {
  return { amount: parseDecimal(amount), currency };
}

const stub: Handlers = {
  echoInvoice(body: EchoInvoiceBody): Promise<Invoice> {
    check(typeof body.total.amount === "string", "server: Money.amount is a string");
    return Promise.resolve(body as Invoice);
  },
  getBalance(): Promise<Money> {
    return Promise.resolve(money("100.00", "EUR"));
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

  const total = money("19.99", "USD");
  const tip = money("2.50", "USD");
  const charges = [money("1.00", "USD"), money("3.00", "EUR")];
  const byCategory = { shipping: money("4.50", "USD") };
  const resp = await api.echoInvoice({
    id: 7,
    total,
    tip,
    items: [{ label: "widget", price: money("9.99", "USD") }],
    charges,
    byCategory,
  });
  check(
    resp.total.amount === total.amount && resp.total.currency === "USD",
    "echo total Money",
  );
  check(
    resp.tip?.amount === tip.amount && resp.tip?.currency === "USD",
    "echo optional tip Money",
  );
  check(
    resp.items.length === 1 && resp.items[0]?.price.amount === parseDecimal("9.99"),
    "echo nested list-item Money",
  );
  // Direct `List<Money>` element revival.
  check(
    resp.charges.length === 2 &&
      resp.charges[0]?.amount === parseDecimal("1.00") &&
      resp.charges[1]?.currency === "EUR",
    "echo direct list-element Money",
  );
  // `Map<String, Money>` value revival.
  check(
    resp.byCategory["shipping"]?.amount === parseDecimal("4.50") &&
      resp.byCategory["shipping"]?.currency === "USD",
    "echo map-value Money",
  );

  const bal = await api.getBalance();
  check(
    typeof bal.amount === "string" && bal.amount === parseDecimal("100.00") && bal.currency === "EUR",
    "bare Money response decoded",
  );

  // Reject path: a malformed amount fails the server body reviver's `parseDecimal`.
  let badAmount = false;
  try {
    await api.echoInvoice({
      id: 1,
      total: { amount: "not-a-number" as unknown as Money["amount"], currency: "USD" },
      items: [],
      charges: [],
      byCategory: {},
    });
  } catch {
    badAmount = true;
  }
  check(badAmount, "server rejected malformed Money amount");

  // Reject path: an unknown currency fails `reviveMoney`'s CURRENCY_CODES check.
  let badCurrency = false;
  try {
    await api.echoInvoice({
      id: 1,
      total: { amount: parseDecimal("1.00"), currency: "ZZZ" },
      items: [],
      charges: [],
      byCategory: {},
    });
  } catch {
    badCurrency = true;
  }
  check(badCurrency, "server rejected invalid ISO 4217 currency");

  // Reject path — NESTED elements. `reviveMoney` runs through the body reviver's
  // list/map/nested-struct walk, so a bad Money inside a `List<Money>` /
  // `Map<String, Money>` / `List<LineItem>` is rejected too. These are the
  // cross-target parallel of the Go driver's nested reject cases (Go's Validate()
  // now recurses likewise), so all three servers agree. Each carries a valid total
  // so only the nested item is the offender.
  const good = money("1.00", "USD");
  const rejects = async (
    body: Parameters<typeof api.echoInvoice>[0],
  ): Promise<boolean> => {
    try {
      await api.echoInvoice(body);
      return false;
    } catch {
      return true;
    }
  };

  check(
    await rejects({
      id: 1,
      total: good,
      items: [],
      charges: [money("1.00", "ZZZ")],
      byCategory: {},
    }),
    "server rejected bad currency in List<Money>",
  );
  check(
    await rejects({
      id: 1,
      total: good,
      items: [],
      charges: [],
      byCategory: {
        // A bad *amount* (not currency), so we can't use the `money()` helper —
        // its `parseDecimal` would throw client-side on "bad" before the request is
        // sent. Cast a raw string past the branded `amount` type to drive the value
        // all the way to the server's element validator.
        shipping: { amount: "bad" as unknown as Money["amount"], currency: "USD" },
      },
    }),
    "server rejected bad amount in Map<String, Money>",
  );
  check(
    await rejects({
      id: 1,
      total: good,
      items: [
        { label: "widget", price: money("9.99", "ZZZ") },
      ],
      charges: [],
      byCategory: {},
    }),
    "server rejected bad currency in List<LineItem>",
  );

  await new Promise<void>((resolve, reject) =>
    server.close((err) => (err ? reject(err) : resolve())),
  );
  console.log("OK");
}

void main();
