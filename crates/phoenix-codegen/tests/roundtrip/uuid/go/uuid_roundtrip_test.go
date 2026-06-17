// Behavioral UUID round-trip driver for the Go target.
//
// Committed source assembled into a tempdir module by the Rust harness
// (`roundtrip.rs::uuid_go_roundtrip`) alongside the generated `api` package
// (from the small UUID schema there). In Go a `Uuid` is a plain `string`, so the
// proof is that valid RFC 4122 strings survive the wire unchanged in a body
// (required / `Option` / `List` / `Map`), as a query param (echoed into the
// response), and as a required response header — and that the server's generated
// `Account.Validate()` (the `uuidRe` check) ACCEPTS valid input on the body path.
package roundtrip_test

import (
	"net/http/httptest"
	"testing"

	"roundtrip/api"
)

const (
	idA   = "550e8400-e29b-41d4-a716-446655440000"
	idB   = "6ba7b810-9dad-11d1-80b4-00c04fd430c8"
	idC   = "6ba7b811-9dad-11d1-80b4-00c04fd430c8"
	refID = "00000000-0000-0000-0000-000000000000"
	reqID = "11111111-1111-1111-1111-111111111111"
)

type stub struct{}

func (s *stub) EchoAccount(body api.EchoAccountBody) (*api.Account, error) {
	return &api.Account{
		Id:      body.Id,
		OwnerId: body.OwnerId,
		Members: body.Members,
		Index:   body.Index,
	}, nil
}

func (s *stub) GetAccount(id string, ref string) (*api.GetAccountResult, error) {
	// Echo the query uuid into Id so the client can assert it round-tripped.
	return &api.GetAccountResult{
		Body: api.Account{
			Id:      ref,
			Members: []string{},
			Index:   map[string]string{},
		},
		RequestId: reqID,
	}, nil
}

func (s *stub) NewId() (*string, error) {
	v := idA
	return &v, nil
}

func TestUuidRoundtrip(t *testing.T) {
	srv := httptest.NewServer(api.NewRouter(&stub{}))
	defer srv.Close()
	client := api.NewApiClient(srv.URL)

	owner := idB
	resp, err := client.EchoAccount(api.EchoAccountBody{
		Id:      idA,
		OwnerId: &owner,
		Members: []string{idC},
		Index:   map[string]string{"primary": idA},
	})
	if err != nil {
		t.Fatalf("echoAccount: %v", err)
	}
	if resp.Id != idA {
		t.Fatalf("id: got %s want %s", resp.Id, idA)
	}
	if resp.OwnerId == nil || *resp.OwnerId != idB {
		t.Fatalf("ownerId: got %v want %s", resp.OwnerId, idB)
	}
	if len(resp.Members) != 1 || resp.Members[0] != idC {
		t.Fatalf("members: got %v want [%s]", resp.Members, idC)
	}
	if resp.Index["primary"] != idA {
		t.Fatalf("index: got %v want {primary:%s}", resp.Index, idA)
	}

	r2, err := client.GetAccount("acct-1", refID)
	if err != nil {
		t.Fatalf("getAccount: %v", err)
	}
	if r2.Body.Id != refID {
		t.Fatalf("query uuid not round-tripped: got %s want %s", r2.Body.Id, refID)
	}
	if r2.RequestId != reqID {
		t.Fatalf("requestId header: got %s want %s", r2.RequestId, reqID)
	}

	id, err := client.NewId()
	if err != nil {
		t.Fatalf("newId: %v", err)
	}
	if id == nil || *id != idA {
		t.Fatalf("bare uuid response: got %v want %s", id, idA)
	}

	// Reject path: a malformed body uuid must fail the generated Account.Validate()
	// (its uuidRe check) on the server, surfacing as a non-nil client error. This
	// proves the validator actually fires, not just that valid input survives.
	if _, err := client.EchoAccount(api.EchoAccountBody{
		Id:      "not-a-uuid",
		Members: []string{},
		Index:   map[string]string{},
	}); err == nil {
		t.Fatal("server accepted malformed body uuid (Validate() did not reject)")
	}

	// Accept path (query uuid): Go validates ONLY body fields (via Validate()),
	// never query/header uuids — the documented weak link. A malformed query
	// uuid must therefore round-trip unchanged rather than error, pinning the
	// TS-validates / Go-accepts divergence (the TS driver asserts the opposite).
	r3, err := client.GetAccount("acct-1", "not-a-uuid")
	if err != nil {
		t.Fatalf("getAccount with malformed query uuid errored (Go must not validate it): %v", err)
	}
	if r3.Body.Id != "not-a-uuid" {
		t.Fatalf("malformed query uuid not echoed back: got %s", r3.Body.Id)
	}
}
