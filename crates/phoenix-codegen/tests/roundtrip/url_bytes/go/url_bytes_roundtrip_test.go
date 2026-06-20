// Behavioral Url/Bytes round-trip driver for the Go target.
//
// Committed source assembled into a tempdir module by the Rust harness
// (`roundtrip.rs::url_bytes_go_roundtrip`) alongside the generated `api` package
// (from the small Url/Bytes schema there). It proves:
//   - `Bytes` is `[]byte`: a field set from raw binary (including non-UTF-8 bytes
//     0x00/0xFF/0xFE/0x80) survives the base64 wire (encoding/json auto-base64s
//     `[]byte`) as the SAME bytes — across a required field, a present/absent
//     `*[]byte` `Option`, a `[][]byte` `List`, and a `map[string][]byte`
//     (`Map<String, Bytes>`), in both request body and the echoed response.
//   - `Url` (Go `string`) round-trips byte-for-byte (validated by `urlRe`, never
//     normalized) through a body field / `Option` / `List`, a query param, a
//     `List<Url>` query param, and a request header — all echoed into the response.
//   - a MULTI-STATUS endpoint (`response { }` block) round-trips a Bytes-bearing
//     shared body through the `{ Status, Body }` envelope.
//   - the reject path: a malformed `Url` query value, a malformed `List<Url>`
//     query element, and a malformed `Url` body field each fail the server's
//     `urlRe` check with exactly HTTP 400 (issued as raw requests so the status is
//     pinned — a 500 regression cannot pass).
package roundtrip_test

import (
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"net/url"
	"reflect"
	"testing"

	"roundtrip/api"
)

const (
	source   = "https://Example.com/a/b?x=1&y=2#frag"
	mirror   = "ftp://mirror.example.org/pub/file.bin"
	thumbA   = "https://t.example/1.png"
	thumbB   = "https://t.example/2.png"
	origin   = "https://origin.example.com/in"
	mirrorQA = "https://m1.example.com"
	mirrorQB = "https://m2.example.com"
	referer  = "https://ref.example.com/page?from=test"
)

// Raw binary with non-UTF-8 bytes: a wrong base64 round-trip would corrupt these.
var (
	checksum = []byte{0x00, 0x01, 0xFF, 0xFE, 0x80}
	sig      = []byte{0xCA, 0xFE, 0x00, 0xBA, 0xBE}
	chunkA   = []byte{0xDE, 0xAD}
	chunkB   = []byte{0xBE, 0xEF, 0x00}
	// A `Map<String, Bytes>` — `encoding/json` base64s each `[]byte` value.
	tags = map[string][]byte{"a": chunkA, "b": chunkB}
)

type stub struct{}

func (s *stub) Upload(
	body api.UploadBody,
	originParam string,
	mirrors []string,
	refererParam string,
) (*api.Echo, error) {
	return &api.Echo{
		Source:     body.Source,
		Mirror:     body.Mirror,
		Thumbnails: body.Thumbnails,
		Checksum:   body.Checksum,
		Signature:  body.Signature,
		Chunks:     body.Chunks,
		Tags:       body.Tags,
		Origin:     originParam,
		Mirrors:    mirrors,
		Referer:    refererParam,
	}, nil
}

// Replace exercises a MULTI-STATUS endpoint (a `response { }` block) whose shared
// body carries `Bytes` (incl. the `Map<String, Bytes>`): the handler echoes the
// body into the `*Payload` envelope body and chooses status 200. The generated
// server wraps it for the wire and the client reads back the `{ Status, Body }`
// envelope — proving the `Bytes`/`Map` round-trip survives the multi-status path.
func (s *stub) Replace(id string, body api.ReplaceBody) (*api.ReplaceResponse, error) {
	echoed := api.Payload{
		Source:     body.Source,
		Mirror:     body.Mirror,
		Thumbnails: body.Thumbnails,
		Checksum:   body.Checksum,
		Signature:  body.Signature,
		Chunks:     body.Chunks,
		Tags:       body.Tags,
	}
	return &api.ReplaceResponse{Status: 200, Body: &echoed}, nil
}

func TestUrlBytesRoundtrip(t *testing.T) {
	srv := httptest.NewServer(api.NewRouter(&stub{}))
	defer srv.Close()
	client := api.NewApiClient(srv.URL)

	mirrorPtr := mirror
	sigPtr := sig
	echo, err := client.Upload(
		api.UploadBody{
			Source:     source,
			Mirror:     &mirrorPtr,
			Thumbnails: []string{thumbA, thumbB},
			Checksum:   checksum,
			Signature:  &sigPtr,
			Chunks:     [][]byte{chunkA, chunkB},
			Tags:       tags,
		},
		origin,
		[]string{mirrorQA, mirrorQB},
		referer,
	)
	if err != nil {
		t.Fatalf("upload: %v", err)
	}

	// Bytes round-trip as raw binary (identical bytes, non-UTF-8 intact).
	if !bytes.Equal(echo.Checksum, checksum) {
		t.Fatalf("checksum: got %v want %v", echo.Checksum, checksum)
	}
	if echo.Signature == nil || !bytes.Equal(*echo.Signature, sig) {
		t.Fatalf("signature: got %v want %v", echo.Signature, sig)
	}
	if len(echo.Chunks) != 2 || !bytes.Equal(echo.Chunks[0], chunkA) || !bytes.Equal(echo.Chunks[1], chunkB) {
		t.Fatalf("chunks: got %v", echo.Chunks)
	}
	// Map<String, Bytes> round-trips as raw binary per value.
	if !reflect.DeepEqual(echo.Tags, tags) {
		t.Fatalf("tags: got %v want %v", echo.Tags, tags)
	}
	// Url round-trips byte-for-byte (no normalization).
	if echo.Source != source {
		t.Fatalf("source: got %q want %q", echo.Source, source)
	}
	if echo.Mirror == nil || *echo.Mirror != mirror {
		t.Fatalf("mirror: got %v want %q", echo.Mirror, mirror)
	}
	if !reflect.DeepEqual(echo.Thumbnails, []string{thumbA, thumbB}) {
		t.Fatalf("thumbnails: got %v", echo.Thumbnails)
	}
	// Url query / List<Url> query / Url header round-trip.
	if echo.Origin != origin {
		t.Fatalf("origin query: got %q want %q", echo.Origin, origin)
	}
	if !reflect.DeepEqual(echo.Mirrors, []string{mirrorQA, mirrorQB}) {
		t.Fatalf("mirrors query: got %v", echo.Mirrors)
	}
	if echo.Referer != referer {
		t.Fatalf("referer header: got %q want %q", echo.Referer, referer)
	}

	// Optional Bytes/Url absent + empty lists round-trip cleanly.
	echo2, err := client.Upload(
		api.UploadBody{
			Source:     source,
			Mirror:     nil,
			Thumbnails: []string{},
			Checksum:   checksum,
			Signature:  nil,
			Chunks:     [][]byte{},
			Tags:       map[string][]byte{},
		},
		origin,
		[]string{},
		referer,
	)
	if err != nil {
		t.Fatalf("upload (call 2): %v", err)
	}
	if echo2.Mirror != nil {
		t.Fatalf("absent mirror came back as %v", *echo2.Mirror)
	}
	if echo2.Signature != nil {
		t.Fatalf("absent signature came back as %v", *echo2.Signature)
	}
	if len(echo2.Thumbnails) != 0 || len(echo2.Chunks) != 0 || len(echo2.Mirrors) != 0 || len(echo2.Tags) != 0 {
		t.Fatalf("empty lists/map not all empty: %+v", echo2)
	}
	if !bytes.Equal(echo2.Checksum, checksum) {
		t.Fatalf("checksum (call 2): got %v", echo2.Checksum)
	}

	// Multi-status endpoint: the shared `Payload` body (carrying Bytes + the
	// Map<String, Bytes>) round-trips through the `{ Status, Body }` envelope. The
	// server wraps the body for the wire and the client revives it back, so the
	// binary must survive identically here too.
	rep, err := client.Replace(
		"asset-1",
		api.ReplaceBody{
			Source:     source,
			Mirror:     &mirrorPtr,
			Thumbnails: []string{thumbA, thumbB},
			Checksum:   checksum,
			Signature:  &sigPtr,
			Chunks:     [][]byte{chunkA, chunkB},
			Tags:       tags,
		},
	)
	if err != nil {
		t.Fatalf("replace: %v", err)
	}
	if rep.Status != 200 {
		t.Fatalf("replace status: got %d want 200", rep.Status)
	}
	if rep.Body == nil {
		t.Fatal("replace envelope body is nil")
	}
	if !bytes.Equal(rep.Body.Checksum, checksum) {
		t.Fatalf("replace checksum: got %v want %v", rep.Body.Checksum, checksum)
	}
	if !reflect.DeepEqual(rep.Body.Tags, tags) {
		t.Fatalf("replace tags: got %v want %v", rep.Body.Tags, tags)
	}
	if rep.Body.Source != source {
		t.Fatalf("replace source: got %q want %q", rep.Body.Source, source)
	}

	// Reject paths: issue raw POSTs (not via the client, which can't expose the
	// status) so each malformed `Url` is pinned to exactly HTTP 400 — a 500
	// regression cannot pass. `postAssets` builds a request with the required
	// `X-Referer` header and the given query + body.
	postAssets := func(query url.Values, payload api.UploadBody) int {
		t.Helper()
		jsonBody, err := json.Marshal(payload)
		if err != nil {
			t.Fatalf("marshal body: %v", err)
		}
		req, err := http.NewRequest(http.MethodPost, srv.URL+"/assets?"+query.Encode(), bytes.NewReader(jsonBody))
		if err != nil {
			t.Fatalf("build request: %v", err)
		}
		req.Header.Set("Content-Type", "application/json")
		req.Header.Set("X-Referer", referer)
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatalf("do request: %v", err)
		}
		resp.Body.Close()
		return resp.StatusCode
	}

	validBody := api.UploadBody{Source: source, Thumbnails: []string{}, Checksum: checksum, Chunks: [][]byte{}}

	// A malformed Url query value must fail the server's urlRe check (400).
	if got := postAssets(url.Values{"origin": {"not-a-url"}}, validBody); got != 400 {
		t.Fatalf("malformed url query: got status %d want 400", got)
	}

	// A malformed element in the `mirrors` List<Url> query must fail the
	// per-element urlRe check (400). Origin + body are valid here.
	if got := postAssets(url.Values{"origin": {origin}, "mirrors": {mirrorQA, "not-a-url"}}, validBody); got != 400 {
		t.Fatalf("malformed List<Url> query element: got status %d want 400", got)
	}

	// A malformed Url in a BODY field must fail the server's body Validate()
	// (urlRe → 400). The query is valid here, so the body field is the only
	// thing that can be rejected.
	badBody := api.UploadBody{Source: "not-a-url", Thumbnails: []string{}, Checksum: checksum, Chunks: [][]byte{}}
	if got := postAssets(url.Values{"origin": {origin}}, badBody); got != 400 {
		t.Fatalf("malformed url body field: got status %d want 400", got)
	}
}
