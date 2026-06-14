// Schema-agnostic round-trip boilerplate shared with the per-schema driver in
// roundtrip_test.go. This file holds the contract.json schema types, the
// generic assertions/value/multipart/query/header/json helpers, and the
// `target` constant — none of which reference the generated `api` package, so
// they stay stable across schema changes. The schema-coupled stub (which
// implements api.Handlers) and the TestRoundtrip/invoke driver live next door
// in roundtrip_test.go; both files compile together as package roundtrip_test.
package roundtrip_test

import (
	"encoding/json"
	"io"
	"mime/multipart"
	"reflect"
	"strconv"
	"strings"
	"testing"
)

// ── contract.json schema (mirror of the language-agnostic format) ──────────

type contractCase struct {
	Name     string      `json:"name"`
	Endpoint string      `json:"endpoint"`
	Kind     string      `json:"kind"` // "ok" | "error" | "constraint"
	Call     callSpec    `json:"call"`
	Handler  handlerSpec `json:"handler"`
	Raw      *rawSpec    `json:"raw_response"`
	Expect   expectSpec  `json:"expect_client"`
}

// rawSpec mirrors the optional raw_response field: when present, the driver
// serves this canned status (+ optional JSON body) for every request INSTEAD of
// mounting the generated server — the only way to put a status on the wire that
// the generated server's own guard would refuse (e.g. an undeclared 2xx for the
// client-leniency cases). The stub handler is never invoked.
type rawSpec struct {
	Status int             `json:"status"`
	Body   json.RawMessage `json:"body"`
}

type callSpec struct {
	PathParams map[string]string      `json:"path_params"`
	Query      map[string]interface{} `json:"query"`
	Body       json.RawMessage        `json:"body"`
	Headers    map[string]interface{} `json:"headers"`
	Multipart  *multipartSpec         `json:"multipart"`
}

// multipartSpec mirrors call.multipart: file parts (filename + UTF-8 content the
// driver encodes to bytes) plus scalar form fields. `Fields` values are
// JSON-typed (string/number/bool); the invoke case coerces each into the
// generated body's typed field (e.g. an Int field → int64).
type multipartSpec struct {
	Files map[string]struct {
		Filename string `json:"filename"`
		Content  string `json:"content"`
	} `json:"files"`
	Fields map[string]interface{} `json:"fields"`
}

type handlerSpec struct {
	ExpectReceived  map[string]interface{} `json:"expect_received"`
	Returns         json.RawMessage        `json:"returns"`
	ReturnsHeaders  map[string]interface{} `json:"returns_headers"`
	ReturnsFile     string                 `json:"returns_file"`
	ReturnsStatus   int                    `json:"returns_status"`
	Raises          string                 `json:"raises"`
	ExpectNotCalled bool                   `json:"expect_not_called"`
}

type expectSpec struct {
	OK             json.RawMessage        `json:"ok"`
	OKHeaders      map[string]interface{} `json:"ok_headers"`
	ExpectDownload *string                `json:"expect_download"`
	Status         int                    `json:"status"`
	OkAbsent       bool                   `json:"ok_absent"`
	Error          *errorExpect           `json:"error"`
}

type errorExpect struct {
	Variant         string         `json:"variant"`
	StatusPerTarget map[string]int `json:"status_per_target"`
}

const target = "go"

// assertDownload reads the client's binary response body fully and compares the
// decoded UTF-8 bytes against expect_client.expect_download.
func assertDownload(t *testing.T, c contractCase, rc io.ReadCloser) {
	defer rc.Close()
	b, err := io.ReadAll(rc)
	if err != nil {
		t.Fatalf("[%s] read download body: %v", c.Name, err)
	}
	if c.Expect.ExpectDownload == nil {
		t.Fatalf("[%s] case has no expect_client.expect_download", c.Name)
	}
	if string(b) != *c.Expect.ExpectDownload {
		t.Fatalf("[%s] download mismatch:\n got: %q\nwant: %q", c.Name, string(b), *c.Expect.ExpectDownload)
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

// ── multipart helpers ────────────────────────────────────────────────────────

// readFileHeader opens a *multipart.FileHeader the generated server decoded from
// the request, reads it fully, and returns the bytes as a string for comparison
// against expect_received's <field>_content.
func readFileHeader(t *testing.T, c contractCase, fh *multipart.FileHeader) string {
	if fh == nil {
		t.Fatalf("[%s] expected a file part but got nil header", c.Name)
	}
	f, err := fh.Open()
	if err != nil {
		t.Fatalf("[%s] open file part: %v", c.Name, err)
	}
	defer f.Close()
	b, err := io.ReadAll(f)
	if err != nil {
		t.Fatalf("[%s] read file part: %v", c.Name, err)
	}
	return string(b)
}

func fileHeaderName(fh *multipart.FileHeader) interface{} {
	if fh == nil {
		return nil
	}
	return fh.Filename
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

// ── header helpers (read typed values from a generic header map) ────────────

// headerStr reads a required string header value (empty string when absent).
func headerStr(m map[string]interface{}, key string) string {
	if v, ok := m[key]; ok && v != nil {
		if s, ok := v.(string); ok {
			return s
		}
	}
	return ""
}

// headerOptStr reads an optional string header value (nil when absent or JSON null).
func headerOptStr(m map[string]interface{}, key string) *string {
	if v, ok := m[key]; ok && v != nil {
		if s, ok := v.(string); ok {
			return &s
		}
	}
	return nil
}

// headerInt reads a numeric header value as int64 (0 when absent).
func headerInt(m map[string]interface{}, key string) int64 {
	return headerIntDefault(m, key, 0)
}

// headerIntDefault reads a numeric header value as int64, falling back to def
// when the key is absent or JSON null.
func headerIntDefault(m map[string]interface{}, key string, def int64) int64 {
	if v, ok := m[key]; ok && v != nil {
		if f, ok := v.(float64); ok {
			return int64(f)
		}
	}
	return def
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
