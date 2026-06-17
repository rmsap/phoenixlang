// Behavioral Decimal round-trip driver for the Go target.
//
// Committed source assembled into a tempdir module by the Rust harness
// (`roundtrip.rs::decimal_go_roundtrip`) alongside the generated `api` package
// (from the small Decimal schema there). In Go a `Decimal` is a plain `string`
// (transport-only, no arithmetic), so the proof is that valid decimal strings
// survive the wire unchanged in a body (required / `Option` / `List` / `Map`), as
// a query param (echoed into the response), and as a required response header —
// and that the server's generated `Invoice.Validate()` (the `decimalRe` check)
// ACCEPTS valid input on the body path and REJECTS a malformed one.
package roundtrip_test

import (
	"net/http/httptest"
	"testing"

	"roundtrip/api"
)

type stub struct{}

func (s *stub) EchoInvoice(body api.EchoInvoiceBody) (*api.Invoice, error) {
	return &api.Invoice{
		Id:         body.Id,
		Subtotal:   body.Subtotal,
		Discount:   body.Discount,
		LineTotals: body.LineTotals,
		Rates:      body.Rates,
	}, nil
}

func (s *stub) GetQuote(id string, minAmount string) (*api.GetQuoteResult, error) {
	// Echo the query decimal into Subtotal so the client can assert it round-tripped.
	return &api.GetQuoteResult{
		Body: api.Invoice{
			Id:         1,
			Subtotal:   minAmount,
			LineTotals: []string{},
			Rates:      map[string]string{},
		},
		ComputedTax: "8.25",
	}, nil
}

func (s *stub) ExchangeRate() (*string, error) {
	v := "1.0825"
	return &v, nil
}

func TestDecimalRoundtrip(t *testing.T) {
	srv := httptest.NewServer(api.NewRouter(&stub{}))
	defer srv.Close()
	client := api.NewApiClient(srv.URL)

	discount := "-2.50"
	resp, err := client.EchoInvoice(api.EchoInvoiceBody{
		Id:         7,
		Subtotal:   "19.99",
		Discount:   &discount,
		LineTotals: []string{"10.00", "9.99"},
		Rates:      map[string]string{"usd": "1.0", "eur": "0.92"},
	})
	if err != nil {
		t.Fatalf("echoInvoice: %v", err)
	}
	if resp.Subtotal != "19.99" {
		t.Fatalf("subtotal: got %s", resp.Subtotal)
	}
	if resp.Discount == nil || *resp.Discount != "-2.50" {
		t.Fatalf("discount: got %v", resp.Discount)
	}
	if len(resp.LineTotals) != 2 || resp.LineTotals[1] != "9.99" {
		t.Fatalf("lineTotals: got %v", resp.LineTotals)
	}
	if resp.Rates["eur"] != "0.92" {
		t.Fatalf("rates: got %v", resp.Rates)
	}

	r2, err := client.GetQuote("inv-1", "5.00")
	if err != nil {
		t.Fatalf("getQuote: %v", err)
	}
	if r2.Body.Subtotal != "5.00" {
		t.Fatalf("query decimal not round-tripped: got %s", r2.Body.Subtotal)
	}
	if r2.ComputedTax != "8.25" {
		t.Fatalf("computedTax header: got %s", r2.ComputedTax)
	}

	rate, err := client.ExchangeRate()
	if err != nil {
		t.Fatalf("exchangeRate: %v", err)
	}
	if rate == nil || *rate != "1.0825" {
		t.Fatalf("bare decimal response: got %v", rate)
	}

	// Reject path: a malformed body decimal must fail Invoice.Validate()'s
	// decimalRe check on the server, surfacing as a non-nil client error.
	if _, err := client.EchoInvoice(api.EchoInvoiceBody{
		Id:         1,
		Subtotal:   "not-a-number",
		LineTotals: []string{},
		Rates:      map[string]string{},
	}); err == nil {
		t.Fatal("server accepted malformed body decimal (Validate() did not reject)")
	}

	// Accept path (query decimal): Go validates ONLY body fields, never
	// query/header decimals — the documented weak link. A malformed query decimal
	// must round-trip unchanged rather than error (the TS driver asserts the opposite).
	r3, err := client.GetQuote("inv-1", "not-a-number")
	if err != nil {
		t.Fatalf("getQuote with malformed query decimal errored (Go must not validate it): %v", err)
	}
	if r3.Body.Subtotal != "not-a-number" {
		t.Fatalf("malformed query decimal not echoed back: got %s", r3.Body.Subtotal)
	}
}
