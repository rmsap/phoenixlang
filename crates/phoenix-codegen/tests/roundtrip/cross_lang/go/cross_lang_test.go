// Cross-language wire-conformance driver for the Go target.
//
// Committed source assembled into a tempdir module by the Rust harness
// (`roundtrip.rs::cross_lang_go_conformance`) alongside the generated `api` package
// and the shared golden `wire.json`. Unlike the other Go round-trips — which only
// prove Go's client and Go's server agree with EACH OTHER — this asserts the actual
// bytes Go puts on the wire equal the single golden contract every target is checked
// against. Conformance of all three targets to one wire ⟹ any client interoperates
// with any server, without cross-process pairing.
//
// It drives the generated client against the generated server through a recording
// `http.RoundTripper` that captures the request the client sends and the response it
// receives, then compares both to the golden. Comparison is structural with one
// twist: a `createdAt` datetime compares as an INSTANT, since Go emits RFC 3339 with
// a `Z` suffix while Python emits `+00:00` and TS emits `.000Z` — all valid RFC 3339
// and mutually parseable, so the targets interoperate even though the strings differ.
package roundtrip_test

import (
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"regexp"
	"testing"
	"time"

	"roundtrip/api"
)

// rfc3339Prefix matches an RFC-3339-ish prefix (`YYYY-MM-DDThh:mm:ss`).
// `time.Parse(RFC3339)` is stricter than this, but gating on the prefix first keeps
// all three comparators (Go/Python/TS) equally strict, so a non-datetime string pair
// can't coerce to a match in the instant path only.
var rfc3339Prefix = regexp.MustCompile(`^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}`)

// recorder captures the request a client sends and the response it receives.
type recorder struct {
	base      http.RoundTripper
	reqMethod string
	reqPath   string
	reqQuery  string
	reqHeader http.Header
	reqBody   []byte
	respBody  []byte
}

func (r *recorder) RoundTrip(req *http.Request) (*http.Response, error) {
	r.reqMethod = req.Method
	r.reqPath = req.URL.Path
	r.reqQuery = req.URL.RawQuery
	r.reqHeader = req.Header.Clone()
	r.reqBody = nil
	if req.Body != nil {
		b, _ := io.ReadAll(req.Body)
		r.reqBody = b
		req.Body = io.NopCloser(bytes.NewReader(b))
	}
	resp, err := r.base.RoundTrip(req)
	if err != nil {
		return resp, err
	}
	rb, _ := io.ReadAll(resp.Body)
	r.respBody = rb
	resp.Body = io.NopCloser(bytes.NewReader(rb))
	return resp, nil
}

func mustTime(s string) time.Time {
	t, err := time.Parse(time.RFC3339, s)
	if err != nil {
		panic(err)
	}
	return t
}

// canonicalAccount is the typed value matching golden `account`.
func canonicalAccount() api.Account {
	return api.Account{
		Id:        "11111111-1111-1111-1111-111111111111",
		CreatedAt: mustTime("2026-01-15T08:30:00Z"),
		Balance:   "19.99",
		Homepage:  "https://Example.com/u?x=1#f",
		Avatar:    []byte{0x00, 0x01, 0xFF},
		Wallet:    api.Money{Amount: "5.00", Currency: "USD"},
		Role:      api.Roleadmin,
		Profile:   api.Profile{DisplayName: "Ada", AvatarUrl: nil},
		Tags:      []string{"x", "y"},
		Active:    true,
	}
}

type stub struct{}

func (s stub) CreateAccount(body api.CreateAccountBody) (*api.Account, error) {
	// Echo the decoded body: if the server dropped/renamed a field on decode, the
	// echoed response wire would diverge from the golden.
	a := api.Account(body)
	return &a, nil
}

func (s stub) GetAccount(accountId string, includeArchived bool, roles []api.Role, requestId string) (*api.Account, error) {
	a := canonicalAccount()
	return &a, nil
}

func (s stub) ListAccounts(page int64) (*api.ListAccountsPage, error) {
	return &api.ListAccountsPage{Items: []api.Account{canonicalAccount()}, TotalCount: 3}, nil
}

// jsonEqual compares two decoded-JSON trees, treating two RFC 3339 datetime strings
// as equal when they denote the same instant (cross-target serialization differs).
func jsonEqual(a, b interface{}) bool {
	switch av := a.(type) {
	case map[string]interface{}:
		bv, ok := b.(map[string]interface{})
		if !ok {
			return false
		}
		// Compare over the UNION of keys, treating a missing key as `null`: an
		// absent optional (TS omits it) and an explicit `null` (Go/Python emit it)
		// are equivalent for a Phoenix `Option`. A present non-null value vs a
		// missing key still differs, so dropped REQUIRED fields are still caught.
		// Corollary: a RENAMED field is caught only when its golden value is
		// non-null (the renamed-from key then reads as null≠value). A renamed
		// null optional (e.g. `avatarUrl`) — or, equivalently, any extra spurious
		// null-valued field — slips through; the non-null `displayName` is what
		// actually exercises the nested-struct rename.
		keys := map[string]struct{}{}
		for k := range av {
			keys[k] = struct{}{}
		}
		for k := range bv {
			keys[k] = struct{}{}
		}
		for k := range keys {
			if !jsonEqual(av[k], bv[k]) {
				return false
			}
		}
		return true
	case []interface{}:
		bv, ok := b.([]interface{})
		if !ok || len(av) != len(bv) {
			return false
		}
		for i := range av {
			if !jsonEqual(av[i], bv[i]) {
				return false
			}
		}
		return true
	case string:
		bs, ok := b.(string)
		if !ok {
			return false
		}
		if av == bs {
			return true
		}
		if !rfc3339Prefix.MatchString(av) || !rfc3339Prefix.MatchString(bs) {
			return false
		}
		ta, ea := time.Parse(time.RFC3339, av)
		tb, eb := time.Parse(time.RFC3339, bs)
		return ea == nil && eb == nil && ta.Equal(tb)
	default:
		return a == b
	}
}

func assertJSONEqual(t *testing.T, label string, gotBytes []byte, want interface{}) {
	t.Helper()
	var got interface{}
	if err := json.Unmarshal(gotBytes, &got); err != nil {
		t.Fatalf("%s: invalid JSON %q: %v", label, gotBytes, err)
	}
	if !jsonEqual(got, want) {
		t.Fatalf("%s: wire mismatch\n got:  %s\n want: %v", label, gotBytes, want)
	}
}

// assertQuery parses a raw query string and asserts its repeated-key multimap
// equals `want` (a golden `{key: [values...]}` map).
func assertQuery(t *testing.T, label, rawQuery string, want map[string]interface{}) {
	t.Helper()
	got, err := url.ParseQuery(rawQuery)
	if err != nil {
		t.Fatalf("%s: bad query %q: %v", label, rawQuery, err)
	}
	if len(got) != len(want) {
		t.Fatalf("%s query keys: got %v want %v", label, got, want)
	}
	for k, wvRaw := range want {
		wv := wvRaw.([]interface{})
		gv := got[k]
		if len(gv) != len(wv) {
			t.Fatalf("%s query[%s]: got %v want %v", label, k, gv, wv)
		}
		for i := range wv {
			if gv[i] != wv[i].(string) {
				t.Fatalf("%s query[%s][%d]: got %q want %q", label, k, i, gv[i], wv[i])
			}
		}
	}
}

// withRenamedKey shallow-copies `m`, renaming top-level key `from` to `to`.
func withRenamedKey(m map[string]interface{}, from, to string) map[string]interface{} {
	out := make(map[string]interface{}, len(m))
	for k, v := range m {
		if k == from {
			out[to] = v
		} else {
			out[k] = v
		}
	}
	return out
}

func TestCrossLangWireConformance(t *testing.T) {
	raw, err := os.ReadFile("wire.json")
	if err != nil {
		t.Fatalf("read wire.json: %v", err)
	}
	var golden map[string]interface{}
	if err := json.Unmarshal(raw, &golden); err != nil {
		t.Fatalf("parse wire.json: %v", err)
	}

	// Meta-guard: the comparator MUST reject a snake_cased rename of a non-null
	// field — exactly the shape of the Python snake-wire bug this whole test
	// exists to catch. Without this, a future change that weakened `jsonEqual`
	// (e.g. intersecting keys instead of unioning) would make every conformance
	// assertion below pass vacuously.
	goldenAcct := golden["account"].(map[string]interface{})
	if jsonEqual(withRenamedKey(goldenAcct, "createdAt", "created_at"), goldenAcct) {
		t.Fatal("comparator accepted a snake_cased rename; conformance assertions would be vacuous")
	}
	// Meta-guard for the OTHER load-bearing rule, the datetime-instant path: it must
	// not collapse two DIFFERENT instants, and must not leak into non-datetime strings
	// (the RFC-3339 prefix gate). Either weakening would let an over-lenient comparator
	// pass the conformance assertions vacuously, the same way a weakened key rule would.
	if jsonEqual("2026-01-15T08:30:00Z", "2026-01-15T09:30:00Z") {
		t.Fatal("comparator treated two different instants as equal")
	}
	if jsonEqual("admin", "guest") {
		t.Fatal("comparator treated two different non-datetime strings as equal")
	}

	srv := httptest.NewServer(api.NewRouter(stub{}))
	defer srv.Close()
	rec := &recorder{base: http.DefaultTransport}
	client := api.NewApiClient(srv.URL)
	client.Client = &http.Client{Transport: rec}

	// createAccount: the request body the client SENDS and the response body the
	// server EMITS must both equal the golden account.
	if _, err := client.CreateAccount(api.CreateAccountBody(canonicalAccount())); err != nil {
		t.Fatalf("createAccount: %v", err)
	}
	createSpec := golden["createAccountRequest"].(map[string]interface{})
	if rec.reqMethod != createSpec["method"].(string) {
		t.Fatalf("createAccount method: got %q want %q", rec.reqMethod, createSpec["method"])
	}
	if rec.reqPath != createSpec["path"].(string) {
		t.Fatalf("createAccount path: got %q want %q", rec.reqPath, createSpec["path"])
	}
	assertJSONEqual(t, "createAccount request body", rec.reqBody, golden["account"])
	assertJSONEqual(t, "createAccount response body", rec.respBody, golden["account"])

	// getAccount: the param wire (path / repeated-key query / aliased header) plus
	// the response body.
	if _, err := client.GetAccount("acc-7", true, []api.Role{api.Roleadmin, api.Roleguest}, "req-1"); err != nil {
		t.Fatalf("getAccount: %v", err)
	}
	spec := golden["getAccountRequest"].(map[string]interface{})
	if rec.reqMethod != spec["method"].(string) {
		t.Fatalf("getAccount method: got %q want %q", rec.reqMethod, spec["method"])
	}
	if rec.reqPath != spec["path"].(string) {
		t.Fatalf("getAccount path: got %q want %q", rec.reqPath, spec["path"])
	}
	assertQuery(t, "getAccount", rec.reqQuery, spec["query"].(map[string]interface{}))
	for k, wv := range spec["headers"].(map[string]interface{}) {
		if rec.reqHeader.Get(k) != wv.(string) {
			t.Fatalf("getAccount header %s: got %q want %q", k, rec.reqHeader.Get(k), wv)
		}
	}
	assertJSONEqual(t, "getAccount response body", rec.respBody, golden["account"])

	// listAccounts: the request query (`page`) plus the pagination envelope wire
	// ({ items, totalCount }).
	if _, err := client.ListAccounts(2); err != nil {
		t.Fatalf("listAccounts: %v", err)
	}
	listSpec := golden["listAccountsRequest"].(map[string]interface{})
	if rec.reqMethod != listSpec["method"].(string) {
		t.Fatalf("listAccounts method: got %q want %q", rec.reqMethod, listSpec["method"])
	}
	if rec.reqPath != listSpec["path"].(string) {
		t.Fatalf("listAccounts path: got %q want %q", rec.reqPath, listSpec["path"])
	}
	assertQuery(t, "listAccounts", rec.reqQuery, listSpec["query"].(map[string]interface{}))
	assertJSONEqual(t, "listAccounts response body", rec.respBody, golden["page"])
}
