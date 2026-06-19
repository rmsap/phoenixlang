// Behavioral list-valued-param round-trip driver for the Go target.
//
// Committed source assembled into a tempdir module by the Rust harness
// (`roundtrip.rs::list_go_roundtrip`) alongside the generated `api` package (from
// the small list schema there). It proves list-valued params survive the wire:
// query params (repeated keys via `url.Values.Add` → `r.URL.Query()[name]`) and
// request headers, BOTH covering every element type — `String`/`Int`/`Uuid`/
// `Status` (a simple enum)/`Float`/`Bool`/`DateTime`/`Decimal` (headers comma-joined
// on send, split + coerced per element on receive) — echo back unchanged, including
// the empty list. Go shares its encode/coerce helpers across both positions, so
// query and header are the same code paths; the schema carries every element type
// in both positions chiefly for the Python driver (whose paths diverge). A
// malformed numeric/`DateTime` header element is the documented query-vs-header
// divergence and is NOT asserted here. The reject path is pinned via the query
// elements instead: the enum query element drives an unknown variant through the
// server's generated `Valid()` (400), and a malformed `Uuid` query element through
// the per-element format check (400) — both surfacing as a non-nil client error (a
// `Status`/`Uuid` is a plain string, so the driver can hand the typed client an
// out-of-range value without a raw request).
package roundtrip_test

import (
	"net/http/httptest"
	"reflect"
	"testing"
	"time"

	"roundtrip/api"
)

const (
	uuidA = "11111111-1111-1111-1111-111111111111"
	uuidB = "22222222-2222-2222-2222-222222222222"
)

func mustParse(s string) time.Time {
	t, err := time.Parse(time.RFC3339, s)
	if err != nil {
		panic(err)
	}
	return t
}

type stub struct{}

func (s *stub) Search(
	ids []string,
	counts []int64,
	uuids []string,
	statuses []api.Status,
	qFloats []float64,
	qFlags []bool,
	qTimes []time.Time,
	qAmounts []string,
	roles []string,
	limits []int64,
	keys []string,
	ratios []float64,
	flags []bool,
	times []time.Time,
	amounts []string,
	tags []api.Status,
) (*api.Echo, error) {
	return &api.Echo{
		Ids: ids, Counts: counts, Uuids: uuids, Statuses: statuses,
		QFloats: qFloats, QFlags: qFlags, QTimes: qTimes, QAmounts: qAmounts,
		Roles: roles, Limits: limits, Keys: keys, Ratios: ratios,
		Flags: flags, Times: times, Amounts: amounts, Tags: tags,
	}, nil
}

func TestListRoundtrip(t *testing.T) {
	srv := httptest.NewServer(api.NewRouter(&stub{}))
	defer srv.Close()
	client := api.NewApiClient(srv.URL)

	t1 := mustParse("2024-01-15T08:30:00Z")
	t2 := mustParse("2024-02-20T16:45:00Z")

	// Multiple elements in each position round-trip in order. Both query and header
	// cover every list element type (String/Int/Uuid/enum/Float/Bool/DateTime/
	// Decimal): the query block exercises the repeated-key encode/coerce path, the
	// header block the comma-split → per-element coerce path.
	echo, err := client.Search(
		[]string{"a", "b", "c"},
		[]int64{1, 2, 3},
		[]string{uuidA, uuidB},
		[]api.Status{api.StatusActive, api.StatusPending},
		[]float64{0.5, 1.25},
		[]bool{true, false},
		[]time.Time{t1, t2},
		[]string{"7.75", "8.00"},
		[]string{"admin", "editor"},
		[]int64{10, 20},
		[]string{uuidA, uuidB},
		[]float64{1.5, 2.5},
		[]bool{true, false},
		[]time.Time{t1, t2},
		[]string{"10.50", "3.25"},
		[]api.Status{api.StatusActive, api.StatusInactive},
	)
	if err != nil {
		t.Fatalf("search: %v", err)
	}
	// Query lists.
	if !reflect.DeepEqual(echo.Ids, []string{"a", "b", "c"}) {
		t.Fatalf("ids: got %v", echo.Ids)
	}
	if !reflect.DeepEqual(echo.Counts, []int64{1, 2, 3}) {
		t.Fatalf("counts: got %v", echo.Counts)
	}
	if !reflect.DeepEqual(echo.Uuids, []string{uuidA, uuidB}) {
		t.Fatalf("uuids: got %v", echo.Uuids)
	}
	if !reflect.DeepEqual(echo.Statuses, []api.Status{api.StatusActive, api.StatusPending}) {
		t.Fatalf("statuses: got %v", echo.Statuses)
	}
	if !reflect.DeepEqual(echo.QFloats, []float64{0.5, 1.25}) {
		t.Fatalf("qFloats query: got %v", echo.QFloats)
	}
	if !reflect.DeepEqual(echo.QFlags, []bool{true, false}) {
		t.Fatalf("qFlags query: got %v", echo.QFlags)
	}
	if len(echo.QTimes) != 2 || !echo.QTimes[0].Equal(t1) || !echo.QTimes[1].Equal(t2) {
		t.Fatalf("qTimes query: got %v want [%v %v]", echo.QTimes, t1, t2)
	}
	if !reflect.DeepEqual(echo.QAmounts, []string{"7.75", "8.00"}) {
		t.Fatalf("qAmounts query: got %v", echo.QAmounts)
	}
	// Header lists.
	if !reflect.DeepEqual(echo.Roles, []string{"admin", "editor"}) {
		t.Fatalf("roles header: got %v", echo.Roles)
	}
	if !reflect.DeepEqual(echo.Limits, []int64{10, 20}) {
		t.Fatalf("limits header: got %v", echo.Limits)
	}
	if !reflect.DeepEqual(echo.Keys, []string{uuidA, uuidB}) {
		t.Fatalf("keys header: got %v", echo.Keys)
	}
	if !reflect.DeepEqual(echo.Ratios, []float64{1.5, 2.5}) {
		t.Fatalf("ratios header: got %v", echo.Ratios)
	}
	if !reflect.DeepEqual(echo.Flags, []bool{true, false}) {
		t.Fatalf("flags header: got %v", echo.Flags)
	}
	// `time.Time` is compared with `.Equal` so a `Z` vs `+00:00` rendering
	// difference never makes an equal instant compare unequal.
	if len(echo.Times) != 2 || !echo.Times[0].Equal(t1) || !echo.Times[1].Equal(t2) {
		t.Fatalf("times header: got %v want [%v %v]", echo.Times, t1, t2)
	}
	if !reflect.DeepEqual(echo.Amounts, []string{"10.50", "3.25"}) {
		t.Fatalf("amounts header: got %v", echo.Amounts)
	}
	if !reflect.DeepEqual(echo.Tags, []api.Status{api.StatusActive, api.StatusInactive}) {
		t.Fatalf("tags header: got %v", echo.Tags)
	}

	// Empty lists round-trip as empty (no params / empty header → []).
	empty, err := client.Search(
		[]string{}, []int64{}, []string{}, []api.Status{},
		[]float64{}, []bool{}, []time.Time{}, []string{},
		[]string{}, []int64{}, []string{}, []float64{}, []bool{},
		[]time.Time{}, []string{}, []api.Status{},
	)
	if err != nil {
		t.Fatalf("search empty: %v", err)
	}
	if len(empty.Ids) != 0 || len(empty.Counts) != 0 || len(empty.Uuids) != 0 ||
		len(empty.Statuses) != 0 || len(empty.QFloats) != 0 || len(empty.QFlags) != 0 ||
		len(empty.QTimes) != 0 || len(empty.QAmounts) != 0 || len(empty.Roles) != 0 ||
		len(empty.Limits) != 0 || len(empty.Keys) != 0 || len(empty.Ratios) != 0 ||
		len(empty.Flags) != 0 || len(empty.Times) != 0 || len(empty.Amounts) != 0 ||
		len(empty.Tags) != 0 {
		t.Fatalf("empty lists not all empty: %+v", empty)
	}

	// Reject path: an unknown enum element must fail the server's per-element
	// Valid() check (400), surfacing as a non-nil client error.
	if _, err := client.Search(
		[]string{"a"}, []int64{1}, []string{uuidA}, []api.Status{api.Status("Bogus")},
		[]float64{1.5}, []bool{true}, []time.Time{t1}, []string{"1.00"},
		[]string{"admin"}, []int64{10}, []string{uuidA}, []float64{1.5}, []bool{true},
		[]time.Time{t1}, []string{"10.50"}, []api.Status{api.StatusActive},
	); err == nil {
		t.Fatal("server accepted unknown enum list element (Valid() did not reject)")
	}

	// Reject path: a malformed Uuid element must fail the server's per-element
	// format check (400) — parallel to the TS/Python validation.
	if _, err := client.Search(
		[]string{"a"}, []int64{1}, []string{"not-a-uuid"}, []api.Status{api.StatusActive},
		[]float64{1.5}, []bool{true}, []time.Time{t1}, []string{"1.00"},
		[]string{"admin"}, []int64{10}, []string{uuidA}, []float64{1.5}, []bool{true},
		[]time.Time{t1}, []string{"10.50"}, []api.Status{api.StatusActive},
	); err == nil {
		t.Fatal("server accepted malformed uuid list element (format check did not reject)")
	}
}
