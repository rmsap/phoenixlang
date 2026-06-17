// Behavioral Money round-trip driver for the Go target.
//
// Committed source assembled into a tempdir module by the Rust harness
// (`roundtrip.rs::money_go_roundtrip`) alongside the generated `api` package
// (from the small Money schema there). `Money` is the composite `{Amount,
// Currency}` struct; the proof is that a `Money` survives the wire unchanged in a
// body (required / `Option` / nested in a list element) and as a bare response,
// and that the server's generated `Invoice.Validate()` (which recurses into the
// `Money` fields' own `Validate()`) ACCEPTS valid input and REJECTS a bad amount
// and a bad currency.
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
		Total:      body.Total,
		Tip:        body.Tip,
		Items:      body.Items,
		Charges:    body.Charges,
		ByCategory: body.ByCategory,
	}, nil
}

func (s *stub) GetBalance() (*api.Money, error) {
	return &api.Money{Amount: "100.00", Currency: "EUR"}, nil
}

func TestMoneyRoundtrip(t *testing.T) {
	srv := httptest.NewServer(api.NewRouter(&stub{}))
	defer srv.Close()
	client := api.NewApiClient(srv.URL)

	tip := api.Money{Amount: "2.50", Currency: "USD"}
	resp, err := client.EchoInvoice(api.EchoInvoiceBody{
		Id:    7,
		Total: api.Money{Amount: "19.99", Currency: "USD"},
		Tip:   &tip,
		Items: []api.LineItem{
			{Label: "widget", Price: api.Money{Amount: "9.99", Currency: "USD"}},
		},
		Charges: []api.Money{
			{Amount: "1.00", Currency: "USD"},
			{Amount: "3.00", Currency: "EUR"},
		},
		ByCategory: map[string]api.Money{
			"shipping": {Amount: "4.50", Currency: "USD"},
		},
	})
	if err != nil {
		t.Fatalf("echoInvoice: %v", err)
	}
	if resp.Total.Amount != "19.99" || resp.Total.Currency != "USD" {
		t.Fatalf("total: got %+v", resp.Total)
	}
	if resp.Tip == nil || resp.Tip.Amount != "2.50" || resp.Tip.Currency != "USD" {
		t.Fatalf("tip: got %+v", resp.Tip)
	}
	if len(resp.Items) != 1 || resp.Items[0].Price.Amount != "9.99" {
		t.Fatalf("items: got %+v", resp.Items)
	}
	// Direct `List<Money>` element round-trip.
	if len(resp.Charges) != 2 || resp.Charges[0].Amount != "1.00" ||
		resp.Charges[1].Currency != "EUR" {
		t.Fatalf("charges: got %+v", resp.Charges)
	}
	// `Map<String, Money>` value round-trip.
	if c, ok := resp.ByCategory["shipping"]; !ok || c.Amount != "4.50" || c.Currency != "USD" {
		t.Fatalf("byCategory: got %+v", resp.ByCategory)
	}

	bal, err := client.GetBalance()
	if err != nil {
		t.Fatalf("getBalance: %v", err)
	}
	if bal == nil || bal.Amount != "100.00" || bal.Currency != "EUR" {
		t.Fatalf("bare Money response: got %+v", bal)
	}

	// Reject path: a malformed amount must fail Money.Validate()'s decimalRe via
	// the containing Invoice.Validate() — surfacing as a non-nil client error.
	if _, err := client.EchoInvoice(api.EchoInvoiceBody{
		Id:    1,
		Total: api.Money{Amount: "not-a-number", Currency: "USD"},
		Items: []api.LineItem{},
	}); err == nil {
		t.Fatal("server accepted malformed Money amount (Validate() did not reject)")
	}

	// Reject path: an unknown currency code must fail the currencyCodes check.
	if _, err := client.EchoInvoice(api.EchoInvoiceBody{
		Id:    1,
		Total: api.Money{Amount: "1.00", Currency: "ZZZ"},
		Items: []api.LineItem{},
	}); err == nil {
		t.Fatal("server accepted invalid ISO 4217 currency (Validate() did not reject)")
	}
}
