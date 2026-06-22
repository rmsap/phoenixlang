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
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"

	"roundtrip/api"
)

// The schema-agnostic round-trip boilerplate (contract.json schema types, the
// `target` constant, and the generic assertion/value/multipart/query/header/json
// helpers) lives in harness_test.go; this file holds only the schema-coupled
// parts: the stub implementing api.Handlers plus the TestRoundtrip/invoke driver.

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

// ListTaggedPosts exercises a versioned endpoint (path /v2/api/posts/tagged/{tag}):
// a path param (tag) plus a defaulted query param (limit). Mutual success proves
// the /v2 prefix round-trips between the generated client and server.
func (s *stub) ListTaggedPosts(tag string, limit int64) (*[]api.Post, error) {
	s.hit = true
	got := map[string]interface{}{
		"tag":   tag,
		"limit": limit,
	}
	assertReceived(s.t, s.c, got)
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	var out []api.Post
	mustUnmarshal(s.t, s.c.Handler.Returns, &out)
	return &out, nil
}

// ListPostsOffset returns an offset-pagination envelope (*ListPostsOffsetPage):
// a list of items plus a totalCount metadata field. handler.returns is the whole
// page JSON object, so it unmarshals directly into the Page struct — proving the
// handler-supplied metadata round-trips through the response body.
func (s *stub) ListPostsOffset(page int64, limit int64) (*api.ListPostsOffsetPage, error) {
	s.hit = true
	got := map[string]interface{}{
		"page":  page,
		"limit": limit,
	}
	assertReceived(s.t, s.c, got)
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	var out api.ListPostsOffsetPage
	mustUnmarshal(s.t, s.c.Handler.Returns, &out)
	return &out, nil
}

// ListPostsCursor returns a cursor-pagination envelope (*ListPostsCursorPage):
// a list of items plus an optional nextCursor metadata field. cursor is an
// optional query param (*string, nil when absent).
func (s *stub) ListPostsCursor(cursor *string, limit int64) (*api.ListPostsCursorPage, error) {
	s.hit = true
	got := map[string]interface{}{
		"cursor": derefStr(cursor),
		"limit":  limit,
	}
	assertReceived(s.t, s.c, got)
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	var out api.ListPostsCursorPage
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

// GetPostMetered exercises request + response headers. Request headers reach the
// handler as ordinary args (asserted via expect_received like path/query params):
// authorization (required), requestId (required), ifNoneMatch (optional *string),
// maxStale (defaulted int64). The response is a typed envelope: a Post body plus
// response headers the stub sets from handler.returns_headers.
func (s *stub) GetPostMetered(id string, authorization string, requestId string, ifNoneMatch *string, maxStale int64) (*api.GetPostMeteredResult, error) {
	s.hit = true
	got := map[string]interface{}{
		"id":            id,
		"authorization": authorization,
		"requestId":     requestId,
		"ifNoneMatch":   derefStr(ifNoneMatch),
		"maxStale":      maxStale,
	}
	assertReceived(s.t, s.c, got)
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	var out api.GetPostMeteredResult
	mustUnmarshal(s.t, s.c.Handler.Returns, &out.Body)
	out.RatelimitRemaining = headerInt(s.c.Handler.ReturnsHeaders, "ratelimitRemaining")
	out.Etag = headerOptStr(s.c.Handler.ReturnsHeaders, "etag")
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

// UpsertPost2 exercises a multi-status endpoint (a response { } block). The
// handler returns the <Endpoint>Response envelope { status, body }: it sets
// .Status from handler.returns_status (the status the stub chooses) and .Body
// from handler.returns (unmarshalled into a *Post) when present — left nil for
// the no-body status (e.g. 204). The generated server writes that status to the
// wire; the client reads it back plus the optional body.
func (s *stub) UpsertPost2(id string, body api.UpsertPost2Body) (*api.UpsertPost2Response, error) {
	s.hit = true
	got := map[string]interface{}{
		"id":     id,
		"title":  body.Title,
		"body":   body.Body,
		"status": string(body.Status),
		"tags":   normalizeSlice(body.Tags),
	}
	assertReceived(s.t, s.c, got)
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	out := &api.UpsertPost2Response{Status: s.c.Handler.ReturnsStatus}
	if len(s.c.Handler.Returns) > 0 {
		var post api.Post
		mustUnmarshal(s.t, s.c.Handler.Returns, &post)
		out.Body = &post
	}
	return out, nil
}

// RequeuePost exercises an ALL-TYPELESS multi-status endpoint (response
// { 202 204 }). The RequeuePostResponse envelope has no Body field at all —
// the stub only chooses the status (handler.returns_status); the generated
// server writes it and the client reads it back off the status-only envelope.
func (s *stub) RequeuePost(id string) (*api.RequeuePostResponse, error) {
	s.hit = true
	got := map[string]interface{}{
		"id": id,
	}
	assertReceived(s.t, s.c, got)
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	return &api.RequeuePostResponse{Status: s.c.Handler.ReturnsStatus}, nil
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

// UploadAvatar exercises a multipart/form-data request: the generated server
// parses the form into UploadAvatarBody, where each file part is a
// *multipart.FileHeader (opened via .Open() then read to bytes) and scalar
// fields are plain Go values. The stub records each file's decoded content +
// filename and the scalar fields (caption/rotation/crop) so expect_received can
// assert them; the optional thumbnail is recorded as nil content when absent.
func (s *stub) UploadAvatar(id string, body api.UploadAvatarBody) (*api.Author, error) {
	s.hit = true
	got := map[string]interface{}{
		"id":              id,
		"avatar_content":  readFileHeader(s.t, s.c, body.Avatar),
		"avatar_filename": fileHeaderName(body.Avatar),
		"caption":         body.Caption,
		"rotation":        body.Rotation,
		"crop":            body.Crop,
		"thumbnail_content": func() interface{} {
			if body.Thumbnail == nil {
				return nil
			}
			return readFileHeader(s.t, s.c, body.Thumbnail)
		}(),
		"thumbnail_filename": func() interface{} {
			if body.Thumbnail == nil {
				return nil
			}
			return fileHeaderName(body.Thumbnail)
		}(),
	}
	assertReceived(s.t, s.c, got)
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	var out api.Author
	mustUnmarshal(s.t, s.c.Handler.Returns, &out)
	return &out, nil
}

// DownloadAvatar streams a binary response body: the stub returns an io.Reader
// over handler.returns_file (the UTF-8 content the server writes as raw bytes).
func (s *stub) DownloadAvatar(id string) (io.Reader, error) {
	s.hit = true
	assertReceived(s.t, s.c, map[string]interface{}{"id": id})
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	return strings.NewReader(s.c.Handler.ReturnsFile), nil
}

// SyncCatalog exercises the three composite type shapes the rest of the schema
// doesn't round-trip: a Map<String,String> (Labels), a List<enum>
// (AllowedStatuses), and a List<struct> as a field (Entries). The stub ECHOES the
// decoded body straight into the Catalog response, so the driver's existing
// deep-JSON assertOK proves the shapes survive both the client→server decode and
// the server→client decode in one shot — no per-field expect_received (and thus
// no map/nested equality helper) needed.
func (s *stub) SyncCatalog(body api.SyncCatalogBody) (*api.Catalog, error) {
	s.hit = true
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	return &api.Catalog{
		Id:              body.Id,
		Labels:          body.Labels,
		AllowedStatuses: body.AllowedStatuses,
		Entries:         body.Entries,
		// Reserved-word fields: Go capitalizes `class`/`async` to exported
		// `Class`/`Async` (json tags keep the `class`/`async` wire keys), so they
		// echo like any other field.
		Class: body.Class,
		Async: body.Async,
	}, nil
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
	s.hit = true
	assertReceived(s.t, s.c, map[string]interface{}{"postId": postId, "page": page, "limit": limit})
	if err := s.errOrNil(); err != nil {
		return nil, err
	}
	var out []api.Comment
	mustUnmarshal(s.t, s.c.Handler.Returns, &out)
	return &out, nil
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
			var h http.Handler = api.NewRouter(s)
			if c.Raw != nil {
				// raw_response case: bypass the generated server entirely and
				// answer with the canned status/body (see rawSpec).
				raw := c.Raw
				h = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
					if len(raw.Body) > 0 {
						w.Header().Set("Content-Type", "application/json")
					}
					w.WriteHeader(raw.Status)
					if len(raw.Body) > 0 {
						_, _ = w.Write(raw.Body)
					}
				})
			}
			srv := httptest.NewServer(h)
			defer srv.Close()
			client := api.NewApiClient(srv.URL)

			callErr := invoke(t, client, c)

			switch c.Kind {
			case "ok":
				if callErr != nil {
					t.Fatalf("expected success, got error: %v", callErr)
				}
				if !s.hit && c.Raw == nil {
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

	case "listComments":
		got, err := client.ListComments(
			c.Call.PathParams["postId"],
			queryInt(c, "page", 1),
			queryInt(c, "limit", 50),
		)
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

	case "listTaggedPosts":
		tag := c.Call.PathParams["tag"]
		limit := queryInt(c, "limit", 20)
		got, err := client.ListTaggedPosts(tag, limit)
		if err != nil {
			return err
		}
		assertOK(t, c, got)
		return nil

	case "listPostsOffset":
		page := queryInt(c, "page", 1)
		limit := queryInt(c, "limit", 20)
		got, err := client.ListPostsOffset(page, limit)
		if err != nil {
			return err
		}
		assertOK(t, c, got)
		return nil

	case "listPostsCursor":
		cursor := queryOptStr(c, "cursor")
		limit := queryInt(c, "limit", 20)
		got, err := client.ListPostsCursor(cursor, limit)
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

	case "getPostMetered":
		// Request headers come from call.headers with the client's declared types.
		// authorization/requestId are required strings; ifNoneMatch is optional
		// (*string, nil when absent). maxStale is a defaulted int64 — note the
		// generated Go client takes it as a *required* value and ALWAYS writes the
		// Max-Stale header (see client.go), so the driver must supply a value. When
		// the case omits maxStale we pass the server-side default (60) so the
		// handler observes 60, matching expect_received.
		authorization := headerStr(c.Call.Headers, "authorization")
		requestId := headerStr(c.Call.Headers, "requestId")
		ifNoneMatch := headerOptStr(c.Call.Headers, "ifNoneMatch")
		maxStale := headerIntDefault(c.Call.Headers, "maxStale", 60)
		got, err := client.GetPostMetered(c.Call.PathParams["id"], authorization, requestId, ifNoneMatch, maxStale)
		if err != nil {
			return err
		}
		assertOK(t, c, &got.Body)
		assertOKHeaders(t, c, got)
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

	case "upsertPost2":
		// Multi-status endpoint: the client returns an envelope { status, body }.
		// Assert the observed status equals expect_client.status; when ok_absent is
		// set (the no-body status, e.g. 204) the envelope body must be nil,
		// otherwise compare the body (*Post) against expect_client.ok.
		var body api.UpsertPost2Body
		mustUnmarshal(t, c.Call.Body, &body)
		got, err := client.UpsertPost2(c.Call.PathParams["id"], body)
		if err != nil {
			return err
		}
		if got.Status != c.Expect.Status {
			t.Fatalf("[%s] envelope status: got %d, want %d", c.Name, got.Status, c.Expect.Status)
		}
		if c.Expect.OkAbsent {
			if got.Body != nil {
				t.Fatalf("[%s] expected absent body, got %#v", c.Name, got.Body)
			}
		} else {
			assertOK(t, c, got.Body)
		}
		return nil

	case "requeuePost":
		// All-typeless multi-status endpoint: the envelope is { Status } with no
		// Body field, so the only client-side observation is the status itself.
		got, err := client.RequeuePost(c.Call.PathParams["id"])
		if err != nil {
			return err
		}
		if got.Status != c.Expect.Status {
			t.Fatalf("[%s] envelope status: got %d, want %d", c.Name, got.Status, c.Expect.Status)
		}
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

	case "uploadAvatar":
		// Build the client-side multipart body from call.multipart: the required
		// avatar file (filename + a reader over its UTF-8 content), the scalar
		// fields (caption/rotation/crop, coerced from their JSON types into the
		// body's typed fields), and the optional thumbnail (nil when absent).
		mp := c.Call.Multipart
		if mp == nil {
			t.Fatalf("[%s] uploadAvatar case has no call.multipart", c.Name)
		}
		avatar := mp.Files["avatar"]
		body := api.UploadAvatarClientBody{
			Avatar:  api.FileUpload{Filename: avatar.Filename, Content: strings.NewReader(avatar.Content)},
			Caption: mp.Fields["caption"].(string),
			// JSON numbers decode into float64 through interface{}; the generated
			// body field is int64, so narrow it here.
			Rotation: int64(mp.Fields["rotation"].(float64)),
			Crop:     mp.Fields["crop"].(bool),
		}
		if thumb, ok := mp.Files["thumbnail"]; ok {
			body.Thumbnail = &api.FileUpload{Filename: thumb.Filename, Content: strings.NewReader(thumb.Content)}
		}
		got, err := client.UploadAvatar(c.Call.PathParams["id"], body)
		if err != nil {
			return err
		}
		assertOK(t, c, got)
		return nil

	case "downloadAvatar":
		rc, err := client.DownloadAvatar(c.Call.PathParams["id"])
		if err != nil {
			return err
		}
		assertDownload(t, c, rc)
		return nil

	case "syncCatalog":
		// The composite-shape body (Map / List<enum> / nested List<struct>)
		// unmarshals straight into the generated body type; the stub echoes it
		// back, so assertOK's deep compare against expect_client.ok validates the
		// full round-trip.
		var body api.SyncCatalogBody
		mustUnmarshal(t, c.Call.Body, &body)
		got, err := client.SyncCatalog(body)
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

// assertOKHeaders compares the response-header fields the client read off the
// envelope against expect_client.ok_headers. ratelimitRemaining is a required
// int; etag is an optional string (nil when the expected value is JSON null).
func assertOKHeaders(t *testing.T, c contractCase, got *api.GetPostMeteredResult) {
	for k, want := range c.Expect.OKHeaders {
		switch k {
		case "ratelimitRemaining":
			if !valueEqual(want, got.RatelimitRemaining) {
				t.Fatalf("[%s] ok_header %q: got %#v, want %#v", c.Name, k, got.RatelimitRemaining, want)
			}
		case "etag":
			if !valueEqual(want, derefStr(got.Etag)) {
				t.Fatalf("[%s] ok_header %q: got %#v, want %#v", c.Name, k, derefStr(got.Etag), want)
			}
		default:
			t.Fatalf("[%s] unknown ok_header %q", c.Name, k)
		}
	}
}
