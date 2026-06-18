// Behavioral inline-response-projection round-trip driver for the Go target.
//
// Committed source assembled into a tempdir module by the Rust harness
// (`roundtrip.rs::projection_go_roundtrip`) alongside the generated `api` package
// (from the small projection schema there). It proves that the generated
// `<Endpoint>Response` projected structs round-trip the wire: a bare projected
// response (`GetProfileResponse`), a `List<…>` of them (`ListProfilesResponse`), and
// a `partial` projection (`GetSummaryResponse` — every field optional, i.e. a Go
// pointer) carry their `Uuid`/`DateTime` fields back and forth unchanged.
// `time.Time.Equal` compares instants so a `Z` vs `+00:00` rendering never makes
// equal compare unequal.
package roundtrip_test

import (
	"net/http/httptest"
	"testing"
	"time"

	"roundtrip/api"
)

func mustParse(s string) time.Time {
	t, err := time.Parse(time.RFC3339, s)
	if err != nil {
		panic(err)
	}
	return t
}

func strptr(s string) *string { return &s }

type stub struct{}

func (s *stub) GetProfile(id string) (*api.GetProfileResponse, error) {
	return &api.GetProfileResponse{
		Id:          "11111111-1111-1111-1111-111111111111",
		DisplayName: id,
		CreatedAt:   mustParse("2026-01-02T03:04:05Z"),
	}, nil
}

func (s *stub) ListProfiles() (*[]api.ListProfilesResponse, error) {
	return &[]api.ListProfilesResponse{
		{
			Id:          "22222222-2222-2222-2222-222222222222",
			DisplayName: "ada",
			CreatedAt:   mustParse("2026-02-03T04:05:06Z"),
		},
	}, nil
}

func (s *stub) GetSummary(id string) (*api.GetSummaryResponse, error) {
	t := mustParse("2026-03-04T05:06:07Z")
	return &api.GetSummaryResponse{
		Id:          strptr("33333333-3333-3333-3333-333333333333"),
		DisplayName: strptr(id),
		CreatedAt:   &t,
	}, nil
}

func (s *stub) GetContact(id string) (*api.GetContactResponse, error) {
	return &api.GetContactResponse{
		Id:          "44444444-4444-4444-4444-444444444444",
		DisplayName: id,
		Email:       "ada@example.com",
		CreatedAt:   mustParse("2026-04-05T06:07:08Z"),
	}, nil
}

func TestProjectionRoundtrip(t *testing.T) {
	srv := httptest.NewServer(api.NewRouter(&stub{}))
	defer srv.Close()
	client := api.NewApiClient(srv.URL)

	// Bare projected response: the picked fields round-trip the wire.
	p, err := client.GetProfile("grace")
	if err != nil {
		t.Fatalf("getProfile: %v", err)
	}
	if p.Id != "11111111-1111-1111-1111-111111111111" {
		t.Fatalf("projected id: got %v", p.Id)
	}
	if p.DisplayName != "grace" {
		t.Fatalf("projected displayName: got %v want grace", p.DisplayName)
	}
	if !p.CreatedAt.Equal(mustParse("2026-01-02T03:04:05Z")) {
		t.Fatalf("projected createdAt: got %v", p.CreatedAt)
	}

	// List of projected responses: each element round-trips.
	list, err := client.ListProfiles()
	if err != nil {
		t.Fatalf("listProfiles: %v", err)
	}
	if list == nil || len(*list) != 1 {
		t.Fatalf("listProfiles: got %v", list)
	}
	row := (*list)[0]
	if row.Id != "22222222-2222-2222-2222-222222222222" || row.DisplayName != "ada" {
		t.Fatalf("projected list element: got %+v", row)
	}
	if !row.CreatedAt.Equal(mustParse("2026-02-03T04:05:06Z")) {
		t.Fatalf("projected list createdAt: got %v", row.CreatedAt)
	}

	// Partial projected response: every field optional (a Go pointer); present
	// values round-trip and the pointers come back non-nil.
	sum, err := client.GetSummary("turing")
	if err != nil {
		t.Fatalf("getSummary: %v", err)
	}
	if sum.Id == nil || *sum.Id != "33333333-3333-3333-3333-333333333333" {
		t.Fatalf("partial id: got %v", sum.Id)
	}
	if sum.DisplayName == nil || *sum.DisplayName != "turing" {
		t.Fatalf("partial displayName: got %v want turing", sum.DisplayName)
	}
	if sum.CreatedAt == nil || !sum.CreatedAt.Equal(mustParse("2026-03-04T05:06:07Z")) {
		t.Fatalf("partial createdAt: got %v", sum.CreatedAt)
	}

	// Omit projection: the complementary selector (drops `passwordHash`); the
	// remaining fields — incl. `email`, kept by omit — round-trip the wire.
	contact, err := client.GetContact("ada")
	if err != nil {
		t.Fatalf("getContact: %v", err)
	}
	if contact.Id != "44444444-4444-4444-4444-444444444444" {
		t.Fatalf("omit id: got %v", contact.Id)
	}
	if contact.DisplayName != "ada" || contact.Email != "ada@example.com" {
		t.Fatalf("omit fields: got %+v", contact)
	}
	if !contact.CreatedAt.Equal(mustParse("2026-04-05T06:07:08Z")) {
		t.Fatalf("omit createdAt: got %v", contact.CreatedAt)
	}
}
