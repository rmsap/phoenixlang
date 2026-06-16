// Behavioral DateTime round-trip driver for the Go target.
//
// Committed source assembled into a tempdir module by the Rust harness
// (`roundtrip.rs::datetime_go_roundtrip`) alongside the generated `api` package
// (from the small DateTime schema in that file). Unlike the main `gen_api`
// driver this is bespoke and schema-coupled: it proves that `DateTime` values
// survive the wire as RFC 3339 in BOTH directions —
//   - request body Dates (required / `Option` / `List`) echo back unchanged;
//   - a `DateTime` query param is parsed server-side (echoed into the response);
//   - a required `DateTime` response header round-trips.
// `time.Time.Equal` is used for all comparisons so a `Z` vs `+00:00` rendering
// difference never makes an equal instant compare unequal.
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

// stub echoes its inputs back so the assertions can compare what the client sent
// against what it received after a full encode → decode → re-encode → decode trip.
type stub struct{}

func (s *stub) EchoEvent(body api.EchoEventBody) (*api.Event, error) {
	return &api.Event{
		Id:          body.Id,
		Name:        body.Name,
		StartsAt:    body.StartsAt,
		EndsAt:      body.EndsAt,
		Checkpoints: body.Checkpoints,
		Phases:      body.Phases,
	}, nil
}

// EchoTask echoes a body whose only DateTime is nested inside `Reminder`,
// proving a nested-struct Date survives the wire (no direct Date on the body).
func (s *stub) EchoTask(body api.EchoTaskBody) (*api.Task, error) {
	return &api.Task{Id: body.Id, Reminder: body.Reminder}, nil
}

// EchoInstant / EchoInstants / EchoInstantMap return BARE scalar / list / map
// DateTime responses (not wrapped in a struct), echoing the `at` query date so
// the client can assert each bare-response decode path round-trips.
func (s *stub) EchoInstant(at time.Time) (*time.Time, error) {
	return &at, nil
}

func (s *stub) EchoInstants(at time.Time) (*[]time.Time, error) {
	return &[]time.Time{at}, nil
}

func (s *stub) EchoInstantMap(at time.Time) (*map[string]time.Time, error) {
	return &map[string]time.Time{"at": at}, nil
}

func (s *stub) GetEvent(id string, since time.Time) (*api.GetEventResult, error) {
	// Echo the parsed query date into StartsAt so the client can assert the
	// `since` query param round-tripped through the server's parse. Set both the
	// required `ServedAt` and the optional `ExpiresAt` header so the client can
	// assert the optional response-header read path (`*time.Time`) round-trips.
	expires := mustParse("2020-01-03T00:00:00Z")
	return &api.GetEventResult{
		Body: api.Event{
			Id:          1,
			Name:        id,
			StartsAt:    since,
			Checkpoints: []time.Time{},
			Phases:      map[string]time.Time{},
		},
		ServedAt:  mustParse("2020-01-02T03:04:05Z"),
		ExpiresAt: &expires,
	}, nil
}

func TestDateTimeRoundtrip(t *testing.T) {
	srv := httptest.NewServer(api.NewRouter(&stub{}))
	defer srv.Close()
	client := api.NewApiClient(srv.URL)

	start := mustParse("2026-06-16T12:30:00Z")
	end := mustParse("2026-06-17T08:00:00Z")
	cp := mustParse("2026-06-16T13:00:00Z")

	// Request body Dates: required, Option, List, and Map all echo back unchanged.
	resp, err := client.EchoEvent(api.EchoEventBody{
		Id:          7,
		Name:        "launch",
		StartsAt:    start,
		EndsAt:      &end,
		Checkpoints: []time.Time{cp},
		Phases:      map[string]time.Time{"kickoff": start, "wrap": end},
	})
	if err != nil {
		t.Fatalf("echoEvent: %v", err)
	}
	if !resp.StartsAt.Equal(start) {
		t.Fatalf("startsAt: got %v want %v", resp.StartsAt, start)
	}
	if resp.EndsAt == nil || !resp.EndsAt.Equal(end) {
		t.Fatalf("endsAt: got %v want %v", resp.EndsAt, end)
	}
	if len(resp.Checkpoints) != 1 || !resp.Checkpoints[0].Equal(cp) {
		t.Fatalf("checkpoints: got %v want [%v]", resp.Checkpoints, cp)
	}
	if len(resp.Phases) != 2 || !resp.Phases["kickoff"].Equal(start) || !resp.Phases["wrap"].Equal(end) {
		t.Fatalf("phases: got %v want {kickoff:%v wrap:%v}", resp.Phases, start, end)
	}

	// Body whose only Date is nested inside a struct field (no direct Date).
	task, err := client.EchoTask(api.EchoTaskBody{
		Id:       3,
		Reminder: api.Reminder{Note: "ping", RemindAt: cp},
	})
	if err != nil {
		t.Fatalf("echoTask: %v", err)
	}
	if !task.Reminder.RemindAt.Equal(cp) {
		t.Fatalf("nested remindAt: got %v want %v", task.Reminder.RemindAt, cp)
	}

	// Query DateTime → parsed server-side; required response-header DateTime.
	since := mustParse("2025-12-31T23:59:59Z")
	r2, err := client.GetEvent("evt-9", since)
	if err != nil {
		t.Fatalf("getEvent: %v", err)
	}
	if !r2.Body.StartsAt.Equal(since) {
		t.Fatalf("query date not round-tripped: got %v want %v", r2.Body.StartsAt, since)
	}
	if !r2.ServedAt.Equal(mustParse("2020-01-02T03:04:05Z")) {
		t.Fatalf("servedAt header: got %v", r2.ServedAt)
	}
	if r2.ExpiresAt == nil || !r2.ExpiresAt.Equal(mustParse("2020-01-03T00:00:00Z")) {
		t.Fatalf("expiresAt optional header: got %v", r2.ExpiresAt)
	}

	// Bare scalar / list / map DateTime responses round-trip the `at` query date.
	inst, err := client.EchoInstant(start)
	if err != nil {
		t.Fatalf("echoInstant: %v", err)
	}
	if inst == nil || !inst.Equal(start) {
		t.Fatalf("bare scalar instant: got %v want %v", inst, start)
	}
	insts, err := client.EchoInstants(start)
	if err != nil {
		t.Fatalf("echoInstants: %v", err)
	}
	if insts == nil || len(*insts) != 1 || !(*insts)[0].Equal(start) {
		t.Fatalf("bare list instants: got %v want [%v]", insts, start)
	}
	instMap, err := client.EchoInstantMap(start)
	if err != nil {
		t.Fatalf("echoInstantMap: %v", err)
	}
	if instMap == nil || !(*instMap)["at"].Equal(start) {
		t.Fatalf("bare map instants: got %v want {at:%v}", instMap, start)
	}
}
