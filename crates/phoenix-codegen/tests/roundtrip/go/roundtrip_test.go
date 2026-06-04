// Behavioral round-trip driver for the Go target.
//
// This test program is committed source. The Rust harness
// (`crates/phoenix-codegen/tests/roundtrip.rs`) assembles a tempdir Go module
// at test time containing:
//   - the generated `api` package (in ./api/, package api),
//   - this file (package roundtrip_test, importing the api package),
//   - go.mod (module name `roundtrip`, with the generated package importable as
//     `roundtrip/api`),
//   - contract.json (copied next to this file).
//
// For each case in contract.json it:
//  1. builds a fixture-driven stub implementing api.Handlers that records the
//     decoded args it received and returns either the canned success value or
//     an error whose message contains the variant name (so the server's
//     `strings.Contains(err.Error(), "X")` mapping fires);
//  2. spins up `httptest.NewServer(api.NewRouter(stub))` and points
//     `api.NewApiClient(srv.URL)` at it;
//  3. invokes the matching client method with the case inputs and asserts:
//     (a) the handler received exactly the expected decoded inputs (this is
//     what catches query-coercion / path-substitution bugs), and
//     (b) for ok cases the client's observed result equals expect_client.ok;
//     for error/constraint cases the client returned an error whose status
//     (parsed out of the Go client's `HTTP <status>` message) equals the
//     expected per-target status. Constraint cases additionally assert the
//     handler was NOT called (server rejected via body.Validate()).
package roundtrip_test

import (
	"encoding/json"
	"fmt"
	"net/http/httptest"
	"os"
	"reflect"
	"strconv"
	"strings"
	"testing"

	"roundtrip/api"
)

// ── contract.json schema (mirror of the language-agnostic format) ──────────

type contractCase struct {
	Name     string      `json:"name"`
	Endpoint string      `json:"endpoint"`
	Kind     string      `json:"kind"` // "ok" | "error" | "constraint"
	Call     callSpec    `json:"call"`
	Handler  handlerSpec `json:"handler"`
	Expect   expectSpec  `json:"expect_client"`
}

type callSpec struct {
	PathParams map[string]string      `json:"path_params"`
	Query      map[string]interface{} `json:"query"`
	Body       json.RawMessage        `json:"body"`
}

type handlerSpec struct {
	ExpectReceived  map[string]interface{} `json:"expect_received"`
	Returns         json.RawMessage        `json:"returns"`
	Raises          string                 `json:"raises"`
	ExpectNotCalled bool                   `json:"expect_not_called"`
}

type expectSpec struct {
	OK    json.RawMessage `json:"ok"`
	Error *errorExpect    `json:"error"`
}

type errorExpect struct {
	Variant         string         `json:"variant"`
	StatusPerTarget map[string]int `json:"status_per_target"`
}

const target = "go"

// ── fixture-driven stub ────────────────────────────────────────────────────

// stub implements api.Handlers. Only the endpoints exercised by contract.json
// are wired with assertions; the rest panic so an unexpected route is loud. The
// active case is set per-iteration so the relevant method can record its args.
type stub struct {
	t   *testing.T
	c   contractCase
	hit bool // whether the relevant handler method was invoked
}

// errOrNil returns an error whose message contains the case's `raises` variant
// (so the server's `strings.Contains` mapping fires), or nil when the case is
// not an error case.
func (s *stub) errOrNil() error {
	if s.c.Handler.Raises != "" {
		return fmt.Errorf("%s: something went wrong", s.c.Handler.Raises)
	}
	return nil
}

func (s *stub) ListPosts(page int64, limit int64, tag *string, search *string, featured bool, minScore float64, maxScore *float64) (*[]api.Post, error) {
	s.hit = true
	got := map[string]interface{}{
		"page":     page,
		"limit":    limit,
		"tag":      derefStr(tag),
		"search":   derefStr(search),
		"featured": featured,
		"minScore": minScore,
		"maxScore": derefFloat(maxScore),
	}
	assertReceived(s.t, s.c, got)
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	var out []api.Post
	mustUnmarshal(s.t, s.c.Handler.Returns, &out)
	return &out, nil
}

func (s *stub) GetPost(id string) (*api.Post, error) {
	s.hit = true
	assertReceived(s.t, s.c, map[string]interface{}{"id": id})
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	var out api.Post
	mustUnmarshal(s.t, s.c.Handler.Returns, &out)
	return &out, nil
}

func (s *stub) CreatePost(body api.CreatePostBody) (*api.Post, error) {
	s.hit = true
	got := map[string]interface{}{
		"title":  body.Title,
		"body":   body.Body,
		"status": string(body.Status),
		"tags":   derefStrSlice(body.Tags),
	}
	assertReceived(s.t, s.c, got)
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	var out api.Post
	mustUnmarshal(s.t, s.c.Handler.Returns, &out)
	return &out, nil
}

func (s *stub) SearchPosts(maxResults int64, sortField string) (*[]api.Post, error) {
	s.hit = true
	got := map[string]interface{}{
		"maxResults": maxResults,
		"sortField":  sortField,
	}
	assertReceived(s.t, s.c, got)
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	var out []api.Post
	mustUnmarshal(s.t, s.c.Handler.Returns, &out)
	return &out, nil
}

// UpdateAuthorProfile carries a constrained Option<String> body field
// (avatarUrl) that is also `partial`-applied, so the generated body type renders
// it as a single `*string` and the server's body.Validate() must nil-guard +
// dereference it. The constraint case sends an empty avatarUrl, so this records
// args only for completeness — server-side Validate() rejects before we run.
func (s *stub) UpdateAuthorProfile(id string, body api.UpdateAuthorProfileBody) (*api.Author, error) {
	s.hit = true
	got := map[string]interface{}{
		"id":        id,
		"name":      body.Name,
		"avatarUrl": derefStr(body.AvatarUrl),
	}
	assertReceived(s.t, s.c, got)
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	var out api.Author
	mustUnmarshal(s.t, s.c.Handler.Returns, &out)
	return &out, nil
}

// Unused endpoints — present to satisfy the interface; loud if ever routed.
func (s *stub) UpdatePost(id string, body api.UpdatePostBody) (*api.Post, error) {
	s.t.Fatalf("unexpected call to UpdatePost")
	return nil, nil
}
func (s *stub) PatchPost(id string, body api.PatchPostBody) (*api.Post, error) {
	s.t.Fatalf("unexpected call to PatchPost")
	return nil, nil
}
func (s *stub) DeletePost(id string) error {
	s.t.Fatalf("unexpected call to DeletePost")
	return nil
}
func (s *stub) ListComments(postId string, page int64, limit int64) (*[]api.Comment, error) {
	s.t.Fatalf("unexpected call to ListComments")
	return nil, nil
}
func (s *stub) CreateComment(postId string, body api.CreateCommentBody) (*api.Comment, error) {
	s.t.Fatalf("unexpected call to CreateComment")
	return nil, nil
}
func (s *stub) GetAuthorProfile(id string) (*api.Author, error) {
	s.t.Fatalf("unexpected call to GetAuthorProfile")
	return nil, nil
}

// ── driver ──────────────────────────────────────────────────────────────────

func TestRoundtrip(t *testing.T) {
	raw, err := os.ReadFile("contract.json")
	if err != nil {
		t.Fatalf("read contract.json: %v", err)
	}
	var cases []contractCase
	if err := json.Unmarshal(raw, &cases); err != nil {
		t.Fatalf("parse contract.json: %v", err)
	}
	if len(cases) == 0 {
		t.Fatal("contract.json has no cases")
	}

	for _, c := range cases {
		c := c
		t.Run(c.Name, func(t *testing.T) {
			s := &stub{t: t, c: c}
			srv := httptest.NewServer(api.NewRouter(s))
			defer srv.Close()
			client := api.NewApiClient(srv.URL)

			callErr := invoke(t, client, c)

			switch c.Kind {
			case "ok":
				if callErr != nil {
					t.Fatalf("expected success, got error: %v", callErr)
				}
				if !s.hit {
					t.Fatalf("handler was never called for ok case")
				}
			case "error":
				if !s.hit {
					t.Fatalf("handler was never called for error case")
				}
				assertErrorStatus(t, c, callErr)
			case "constraint":
				if c.Handler.ExpectNotCalled && s.hit {
					t.Fatalf("constraint case: handler WAS called but should have been rejected server-side")
				}
				assertErrorStatus(t, c, callErr)
			default:
				t.Fatalf("unknown case kind %q", c.Kind)
			}
		})
	}
}

// invoke calls the matching client method and, for ok cases, asserts the typed
// result equals expect_client.ok. Returns the client error (nil on success).
func invoke(t *testing.T, client *api.ApiClient, c contractCase) error {
	switch c.Endpoint {
	case "getPost":
		got, err := client.GetPost(c.Call.PathParams["id"])
		if err != nil {
			return err
		}
		assertOK(t, c, got)
		return nil

	case "listPosts":
		page := queryInt(c, "page", 1)
		limit := queryInt(c, "limit", 20)
		tag := queryOptStr(c, "tag")
		search := queryOptStr(c, "search")
		featured := queryBool(c, "featured", false)
		minScore := queryFloat(c, "minScore", 0.0)
		maxScore := queryOptFloat(c, "maxScore")
		got, err := client.ListPosts(page, limit, tag, search, featured, minScore, maxScore)
		if err != nil {
			return err
		}
		assertOK(t, c, got)
		return nil

	case "searchPosts":
		maxResults := queryInt(c, "maxResults", 0)
		sortField := queryStr(c, "sortField", "")
		got, err := client.SearchPosts(maxResults, sortField)
		if err != nil {
			return err
		}
		assertOK(t, c, got)
		return nil

	case "createPost":
		var body api.CreatePostBody
		mustUnmarshal(t, c.Call.Body, &body)
		got, err := client.CreatePost(body)
		if err != nil {
			return err
		}
		assertOK(t, c, got)
		return nil

	case "updateAuthorProfile":
		var body api.UpdateAuthorProfileBody
		mustUnmarshal(t, c.Call.Body, &body)
		got, err := client.UpdateAuthorProfile(c.Call.PathParams["id"], body)
		if err != nil {
			return err
		}
		assertOK(t, c, got)
		return nil

	default:
		t.Fatalf("driver has no invoke mapping for endpoint %q", c.Endpoint)
		return nil
	}
}

// ── assertions ──────────────────────────────────────────────────────────────

// assertReceived checks every key in expect_received against the decoded args
// the handler actually saw. Numeric JSON literals are compared as float64.
func assertReceived(t *testing.T, c contractCase, got map[string]interface{}) {
	for k, want := range c.Handler.ExpectReceived {
		g, ok := got[k]
		if !ok {
			t.Fatalf("[%s] handler did not receive arg %q", c.Name, k)
		}
		if !valueEqual(want, g) {
			t.Fatalf("[%s] handler arg %q: got %#v (%T), want %#v (%T)", c.Name, k, g, g, want, want)
		}
	}
}

// assertOK compares the client's typed result against expect_client.ok by
// re-marshalling both to canonical JSON.
func assertOK(t *testing.T, c contractCase, got interface{}) {
	wantJSON := c.Expect.OK
	gotJSON, err := json.Marshal(got)
	if err != nil {
		t.Fatalf("[%s] marshal client result: %v", c.Name, err)
	}
	if !jsonEqual(t, gotJSON, wantJSON) {
		t.Fatalf("[%s] client result mismatch:\n got: %s\nwant: %s", c.Name, gotJSON, wantJSON)
	}
}

// assertErrorStatus parses the Go client's `HTTP <status>` error and compares it
// to the expected per-target status for "go".
func assertErrorStatus(t *testing.T, c contractCase, callErr error) {
	if callErr == nil {
		t.Fatalf("[%s] expected an error, got nil", c.Name)
	}
	if c.Expect.Error == nil {
		t.Fatalf("[%s] case has no expect_client.error", c.Name)
	}
	wantStatus, ok := c.Expect.Error.StatusPerTarget[target]
	if !ok {
		t.Fatalf("[%s] no status_per_target[%q]", c.Name, target)
	}
	gotStatus := parseHTTPStatus(t, c.Name, callErr.Error())
	if gotStatus != wantStatus {
		t.Fatalf("[%s] error status: got %d, want %d (err=%q)", c.Name, gotStatus, wantStatus, callErr.Error())
	}
}

// parseHTTPStatus extracts the integer from the Go client's `HTTP <status>`
// error string.
func parseHTTPStatus(t *testing.T, name, msg string) int {
	const prefix = "HTTP "
	idx := strings.Index(msg, prefix)
	if idx < 0 {
		t.Fatalf("[%s] error %q does not contain %q", name, msg, prefix)
	}
	n, err := strconv.Atoi(strings.TrimSpace(msg[idx+len(prefix):]))
	if err != nil {
		t.Fatalf("[%s] could not parse status from %q: %v", name, msg, err)
	}
	return n
}

// ── value helpers ──────────────────────────────────────────────────────────

// valueEqual compares an expected JSON-decoded value (string / float64 / bool /
// nil) against a handler arg (Go-typed). Numbers are normalised to float64.
func valueEqual(want, got interface{}) bool {
	if want == nil {
		return got == nil
	}
	switch w := want.(type) {
	case float64:
		gf, ok := toFloat(got)
		return ok && gf == w
	case bool:
		gb, ok := got.(bool)
		return ok && gb == w
	case string:
		gs, ok := got.(string)
		return ok && gs == w
	case []interface{}:
		return reflect.DeepEqual(want, normalizeSlice(got))
	default:
		return reflect.DeepEqual(want, got)
	}
}

func toFloat(v interface{}) (float64, bool) {
	switch n := v.(type) {
	case float64:
		return n, true
	case int64:
		return float64(n), true
	case int:
		return float64(n), true
	default:
		return 0, false
	}
}

func normalizeSlice(v interface{}) []interface{} {
	switch s := v.(type) {
	case []string:
		out := make([]interface{}, len(s))
		for i, x := range s {
			out[i] = x
		}
		return out
	case []interface{}:
		return s
	default:
		return nil
	}
}

func derefStr(p *string) interface{} {
	if p == nil {
		return nil
	}
	return *p
}

func derefFloat(p *float64) interface{} {
	if p == nil {
		return nil
	}
	return *p
}

func derefStrSlice(p *[]string) interface{} {
	if p == nil {
		return nil
	}
	out := make([]interface{}, len(*p))
	for i, x := range *p {
		out[i] = x
	}
	return out
}

// ── query helpers (read typed values from the generic call.query map) ───────

func queryInt(c contractCase, key string, def int64) int64 {
	if v, ok := c.Call.Query[key]; ok {
		if f, ok := v.(float64); ok {
			return int64(f)
		}
	}
	return def
}

func queryStr(c contractCase, key string, def string) string {
	if v, ok := c.Call.Query[key]; ok {
		if s, ok := v.(string); ok {
			return s
		}
	}
	return def
}

func queryFloat(c contractCase, key string, def float64) float64 {
	if v, ok := c.Call.Query[key]; ok {
		if f, ok := v.(float64); ok {
			return f
		}
	}
	return def
}

func queryBool(c contractCase, key string, def bool) bool {
	if v, ok := c.Call.Query[key]; ok {
		if b, ok := v.(bool); ok {
			return b
		}
	}
	return def
}

func queryOptStr(c contractCase, key string) *string {
	if v, ok := c.Call.Query[key]; ok && v != nil {
		if s, ok := v.(string); ok {
			return &s
		}
	}
	return nil
}

func queryOptFloat(c contractCase, key string) *float64 {
	if v, ok := c.Call.Query[key]; ok && v != nil {
		if f, ok := v.(float64); ok {
			return &f
		}
	}
	return nil
}

// ── json helpers ────────────────────────────────────────────────────────────

func mustUnmarshal(t *testing.T, raw json.RawMessage, dst interface{}) {
	if len(raw) == 0 {
		t.Fatalf("missing JSON payload to unmarshal into %T", dst)
	}
	if err := json.Unmarshal(raw, dst); err != nil {
		t.Fatalf("unmarshal into %T: %v", dst, err)
	}
}

func jsonEqual(t *testing.T, a, b json.RawMessage) bool {
	var av, bv interface{}
	if err := json.Unmarshal(a, &av); err != nil {
		t.Fatalf("jsonEqual: bad a: %v", err)
	}
	if err := json.Unmarshal(b, &bv); err != nil {
		t.Fatalf("jsonEqual: bad b: %v", err)
	}
	return reflect.DeepEqual(av, bv)
}
