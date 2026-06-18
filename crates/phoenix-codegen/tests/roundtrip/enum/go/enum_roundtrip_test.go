// Behavioral enum query/header round-trip driver for the Go target.
//
// Committed source assembled into a tempdir module by the Rust harness
// (`roundtrip.rs::enum_go_roundtrip`) alongside the generated `api` package (from
// the small enum schema there). A Go enum lowers to `type T string`, so the proof
// is that valid variant strings survive the wire unchanged through a query param
// (required + defaulted), a request header (required + `Option`), and a response
// header (required + `Option`) — and that the server's generated `Valid()` check
// REJECTS an unknown variant in a query param and in a header (surfacing as a
// non-nil client error). Because `Color` is a plain string, the driver can hand
// the client an out-of-range value to drive the reject path without a raw request.
package roundtrip_test

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	"roundtrip/api"
)

type stub struct{}

// PickItem echoes the query enums into the body and the header enums into the
// response headers, so the client can assert every position round-trips.
func (s *stub) PickItem(
	color api.Color,
	size api.Size,
	preferred api.Color,
	fallback *api.Color,
) (*api.PickItemResult, error) {
	return &api.PickItemResult{
		Body:   api.Item{Name: "picked", Color: color, Size: size},
		Chosen: preferred,
		Alt:    fallback,
	}, nil
}

func TestEnumRoundtrip(t *testing.T) {
	srv := httptest.NewServer(api.NewRouter(&stub{}))
	defer srv.Close()
	client := api.NewApiClient(srv.URL)

	fallback := api.ColorGreen
	r, err := client.PickItem(api.ColorBlue, api.SizeLarge, api.ColorRed, &fallback)
	if err != nil {
		t.Fatalf("pickItem: %v", err)
	}
	if r.Body.Color != api.ColorBlue {
		t.Fatalf("query color: got %v want Blue", r.Body.Color)
	}
	if r.Body.Size != api.SizeLarge {
		t.Fatalf("query size: got %v want Large", r.Body.Size)
	}
	if r.Chosen != api.ColorRed {
		t.Fatalf("chosen header: got %v want Red", r.Chosen)
	}
	if r.Alt == nil || *r.Alt != api.ColorGreen {
		t.Fatalf("alt header: got %v want Green", r.Alt)
	}

	// Server-applied default: a raw GET omitting `size` must have the server
	// seed `Medium` (the typed client always sends its `size` argument, so only
	// a raw request can omit it — exercising the `size := SizeMedium` decode).
	req, err := http.NewRequest(http.MethodGet, srv.URL+"/pick?color=Red", nil)
	if err != nil {
		t.Fatalf("build default request: %v", err)
	}
	req.Header.Set("X-Preferred", "Blue")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatalf("raw default request: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("raw default request: HTTP %d", resp.StatusCode)
	}
	var item api.Item
	if err := json.NewDecoder(resp.Body).Decode(&item); err != nil {
		t.Fatalf("decode default body: %v", err)
	}
	if item.Size != api.SizeMedium {
		t.Fatalf("defaulted query enum not applied server-side: got %v want Medium", item.Size)
	}

	// Reject path: an unknown enum QUERY value must fail the server's Valid()
	// check (400), surfacing as a non-nil client error.
	if _, err := client.PickItem(api.Color("Purple"), api.SizeSmall, api.ColorRed, nil); err == nil {
		t.Fatal("server accepted unknown query enum (Valid() did not reject)")
	}

	// Reject path: an unknown enum HEADER value must also 400.
	if _, err := client.PickItem(api.ColorRed, api.SizeSmall, api.Color("Mauve"), nil); err == nil {
		t.Fatal("server accepted unknown header enum (Valid() did not reject)")
	}
}
