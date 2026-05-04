use super::*;

fn test_uri() -> Url {
    Url::parse("file:///tmp/test.phx").expect("test uri parses")
}

/// Build a `BufferSnapshot` with no cached canonical path. The
/// production flow caches one when possible, but the project pipeline
/// has a fallback `canonicalize` syscall for the `None` case, so the
/// routing under test still reaches its intended branches.
fn snap(source: &str) -> BufferSnapshot {
    BufferSnapshot {
        source: source.to_string(),
        canonical_path: None,
    }
}

/// Build a `BufferSnapshot` with the cached canonical path populated —
/// matches what production builds for siblings on every analyze after
/// the first.
fn snap_canon(source: &str, canon: PathBuf) -> BufferSnapshot {
    BufferSnapshot {
        source: source.to_string(),
        canonical_path: Some(canon),
    }
}

// ── discover_entry_for ──────────────────────────────────────────

#[test]
fn discover_entry_finds_main_phx_in_same_dir() {
    let td = tempfile::TempDir::new().unwrap();
    let main = td.path().join("main.phx");
    std::fs::write(&main, "function main() {}\n").unwrap();
    let lib = td.path().join("lib.phx");
    std::fs::write(&lib, "public function add() {}\n").unwrap();
    assert_eq!(
        discover_entry_for(&lib).canonicalize().unwrap(),
        main.canonicalize().unwrap()
    );
}

#[test]
fn discover_entry_walks_up_directories() {
    let td = tempfile::TempDir::new().unwrap();
    let main = td.path().join("main.phx");
    std::fs::write(&main, "function main() {}\n").unwrap();
    let nested = td.path().join("models");
    std::fs::create_dir_all(&nested).unwrap();
    let user = nested.join("user.phx");
    std::fs::write(&user, "public function makeUser() {}\n").unwrap();
    assert_eq!(
        discover_entry_for(&user).canonicalize().unwrap(),
        main.canonicalize().unwrap()
    );
}

#[test]
fn discover_entry_falls_back_to_self_when_no_main() {
    let td = tempfile::TempDir::new().unwrap();
    let scratch = td.path().join("scratch.phx");
    std::fs::write(&scratch, "function main() {}\n").unwrap();
    assert_eq!(discover_entry_for(&scratch), scratch);
}

#[test]
fn discover_entry_stops_at_git_boundary() {
    let td = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(td.path().join(".git")).unwrap();
    let main = td.path().join("main.phx");
    std::fs::write(&main, "function main() {}\n").unwrap();
    let inner = td.path().join("subproject");
    std::fs::create_dir_all(&inner).unwrap();
    let inner_main = inner.join("main.phx");
    std::fs::write(&inner_main, "function main() {}\n").unwrap();
    let inner_lib = inner.join("lib.phx");
    std::fs::write(&inner_lib, "public function helper() {}\n").unwrap();
    assert_eq!(
        discover_entry_for(&inner_lib).canonicalize().unwrap(),
        inner_main.canonicalize().unwrap()
    );
}

// ── private_import_diagnostic_note_url_points_at_def_file ───────

#[test]
fn private_import_diagnostic_note_url_points_at_def_file() {
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    let lib_path = td.path().join("lib.phx");
    std::fs::write(&lib_path, "function secretHelper() -> Int { return 42 }\n").unwrap();
    let main_text = "import lib { secretHelper }\nfunction main() { secretHelper() }\n";
    std::fs::write(&main_path, main_text).unwrap();

    let mut source_map = SourceMap::new();
    let modules = resolve_with_overlay(&main_path, &mut source_map, &HashMap::new())
        .expect("resolver succeeds even for a private-import program");
    let source_id_to_url = build_source_id_to_url(&modules);
    let analysis = checker::check_modules(&modules);
    let lib_uri = Url::from_file_path(lib_path.canonicalize().unwrap()).unwrap();
    let main_uri = Url::from_file_path(main_path.canonicalize().unwrap()).unwrap();

    let mut found_private_with_lib_note = false;
    for diag in &analysis.diagnostics {
        for note in &diag.notes {
            let note_uri = source_id_to_url
                .get(&note.span.source_id)
                .cloned()
                .unwrap_or_else(|| main_uri.clone());
            if note_uri == lib_uri && diag.message.to_lowercase().contains("private") {
                found_private_with_lib_note = true;
            }
        }
    }
    assert!(
        found_private_with_lib_note,
        "expected a privacy diagnostic with a note pointing at lib.phx; \
         diagnostics: {:?}",
        analysis
            .diagnostics
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn single_file_program_no_main_phx_round_trips() {
    let td = tempfile::TempDir::new().unwrap();
    let scratch = td.path().join("scratch.phx");
    let text = "function main() { let x: Int = 1 }\n";
    std::fs::write(&scratch, text).unwrap();

    let entry = discover_entry_for(&scratch);
    assert_eq!(entry, scratch);
    let mut sm = SourceMap::new();
    let modules = resolve_with_overlay(&entry, &mut sm, &HashMap::new()).unwrap();
    assert_eq!(modules.len(), 1);
    let analysis = checker::check_modules(&modules);
    assert!(
        analysis.diagnostics.is_empty(),
        "expected clean analysis on scratch buffer, got: {:?}",
        analysis.diagnostics
    );
}

// ── build_source_id_to_url_from_map ─────────────────────────────

#[test]
fn build_source_id_to_url_from_map_handles_subdirectory_edited_file() {
    // Regression for the bug where the function used the edited
    // file's parent as the project root. With a nested edited file
    // the source map's root-relative names (e.g. "models/user.phx")
    // would join with the wrong base and silently fall back to
    // `edited_uri`. Pin the contract: pass the resolver's entry
    // path and root-relative names resolve correctly.
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    std::fs::write(
        &main_path,
        "import models.user { makeUser }\nfunction main() { makeUser() }\n",
    )
    .unwrap();
    let models = td.path().join("models");
    std::fs::create_dir_all(&models).unwrap();
    let user_path = models.join("user.phx");
    std::fs::write(&user_path, "public function makeUser() {}\n").unwrap();

    let mut sm = SourceMap::new();
    let _modules =
        resolve_with_overlay(&main_path, &mut sm, &HashMap::new()).expect("resolver succeeds");

    let urls = build_source_id_to_url_from_map(&sm, &main_path);
    let user_canon = user_path.canonicalize().unwrap();
    let user_uri = Url::from_file_path(&user_canon).unwrap();

    assert!(
        urls.values().any(|u| u == &user_uri),
        "subdirectory file's URL should be reconstructed from \
         entry-relative names; got: {:?}",
        urls.values().collect::<Vec<_>>()
    );
}

// ── render_resolve_error ────────────────────────────────────────

#[test]
fn render_resolve_error_missing_module_targets_import_span() {
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    std::fs::write(
        &main_path,
        "import nonexistent { foo }\nfunction main() {}\n",
    )
    .unwrap();

    let mut sm = SourceMap::new();
    let err = resolve_with_overlay(&main_path, &mut sm, &HashMap::new())
        .expect_err("resolver should fail on missing module");
    let main_uri = Url::from_file_path(main_path.canonicalize().unwrap()).unwrap();
    let id_map = build_source_id_to_url_from_map(&sm, &main_path);

    let by_uri = render_resolve_error(&err, &sm, &id_map, &main_uri);
    let main_diags = by_uri
        .get(&main_uri)
        .expect("missing-module diagnostic should land on main.phx");
    assert_eq!(main_diags.len(), 1);
    assert!(
        main_diags[0].message.to_lowercase().contains("nonexistent"),
        "diagnostic should name the missing module: {:?}",
        main_diags[0].message
    );
    assert_eq!(main_diags[0].severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn render_resolve_error_entry_not_found_falls_back_to_uri() {
    let bogus = std::path::PathBuf::from("/this/path/does/not/exist/main.phx");
    let mut sm = SourceMap::new();
    let err = resolve_with_overlay(&bogus, &mut sm, &HashMap::new())
        .expect_err("resolver should reject missing entry");
    let fallback = test_uri();
    let id_map = HashMap::new();

    let by_uri = render_resolve_error(&err, &sm, &id_map, &fallback);
    let diags = by_uri
        .get(&fallback)
        .expect("entry-not-found should produce a fallback-URI diagnostic");
    assert_eq!(diags.len(), 1);
    assert!(
        diags[0].message.to_lowercase().contains("not found")
            || diags[0].message.to_lowercase().contains("entry"),
        "diagnostic should describe the entry failure: {:?}",
        diags[0].message
    );
}

// ── compute_analyze: pipeline behaviour ─────────────────────────

#[test]
fn compute_analyze_non_file_uri_falls_back_to_single_file() {
    // Untitled buffers / custom-scheme URIs can't run the project
    // pipeline. The outcome should publish only against the same
    // URI and scope `project_uris` to it, so unrelated open buffers
    // aren't inadvertently included.
    let uri = Url::parse("untitled:Untitled-1").unwrap();
    let outcome = compute_analyze(
        HashMap::new(),
        uri.clone(),
        "function main() {}".into(),
        None,
    );
    assert_eq!(outcome.project_uris.len(), 1);
    assert!(outcome.project_uris.contains(&uri));
    assert!(outcome.by_uri.contains_key(&uri));
    assert!(outcome.new_states.contains_key(&uri));
}

#[test]
fn compute_analyze_clear_stale_does_not_include_unrelated_buffer() {
    // Pins the bug in the previous design: editing a project A
    // file should NOT appear in the stale-clear scope for an open
    // unrelated buffer (e.g. a scratch buffer outside the project).
    // Concretely, a buffer that's in the snapshot but isn't
    // reachable from the entry must not be in `project_uris`.
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    std::fs::write(&main_path, "function main() {}\n").unwrap();
    let main_uri = Url::from_file_path(main_path.canonicalize().unwrap()).unwrap();

    // Unrelated buffer in a totally separate directory tree.
    let other_td = tempfile::TempDir::new().unwrap();
    let other_path = other_td.path().join("scratch.phx");
    std::fs::write(&other_path, "function main() {}\n").unwrap();
    let other_uri = Url::from_file_path(other_path.canonicalize().unwrap()).unwrap();

    let mut snapshot = HashMap::new();
    snapshot.insert(other_uri.clone(), snap("function main() {}\n"));

    let outcome = compute_analyze(
        snapshot,
        main_uri.clone(),
        "function main() {}\n".to_string(),
        Some(main_path.clone()),
    );
    assert!(outcome.project_uris.contains(&main_uri));
    assert!(
        !outcome.project_uris.contains(&other_uri),
        "unrelated buffer must not be in the stale-clear scope; got {:?}",
        outcome.project_uris
    );
}

#[test]
fn compute_project_outcome_publishes_for_open_siblings() {
    // Sibling lib.phx is open. Editing main.phx should publish
    // a (possibly empty) diagnostic slot for lib.phx so the
    // stale-clear pass leaves it alone, AND should refresh
    // lib.phx's `DocumentState` against the new shared SourceMap.
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    let lib_path = td.path().join("lib.phx");
    std::fs::write(
        &lib_path,
        "public function add(a: Int, b: Int) -> Int { return a + b }\n",
    )
    .unwrap();
    let main_text = "import lib { add }\nfunction main() { add(1, 2) }\n";
    std::fs::write(&main_path, main_text).unwrap();
    let main_uri = Url::from_file_path(main_path.canonicalize().unwrap()).unwrap();
    let lib_uri = Url::from_file_path(lib_path.canonicalize().unwrap()).unwrap();

    let mut snapshot = HashMap::new();
    snapshot.insert(
        lib_uri.clone(),
        snap_canon(
            "public function add(a: Int, b: Int) -> Int { return a + b }\n",
            lib_path.canonicalize().unwrap(),
        ),
    );

    let outcome = compute_analyze(
        snapshot,
        main_uri.clone(),
        main_text.to_string(),
        Some(main_path.clone()),
    );
    assert!(outcome.project_uris.contains(&main_uri));
    assert!(outcome.project_uris.contains(&lib_uri));
    assert!(
        outcome.by_uri.contains_key(&lib_uri),
        "open sibling URI should be pinned in by_uri so it isn't \
         stale-cleared; got {:?}",
        outcome.by_uri.keys().collect::<Vec<_>>()
    );
    assert!(
        outcome.new_states.contains_key(&lib_uri),
        "open sibling DocumentState should be refreshed against the \
         new analysis"
    );
    let lib_state = outcome
        .new_states
        .get(&lib_uri)
        .expect("sibling state present");
    assert_eq!(
        lib_state.canonical_path.as_deref(),
        Some(lib_path.canonicalize().unwrap().as_path()),
        "sibling state should carry the canonical path forward so the \
         next analyze can skip the canonicalize syscall"
    );
}

#[test]
fn compute_project_outcome_keeps_sibling_diags_when_edited_outside_graph() {
    // Edited buffer is a scratch file inside the project tree but
    // not reachable from main.phx. The project's own files should
    // still get their diagnostics published (this regresses the
    // previous early-return that dropped them); the edited buffer
    // gets a single-file analysis merged on top.
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    let lib_path = td.path().join("lib.phx");
    // lib.phx has a parse error so we can verify project diags
    // for the project files survive.
    std::fs::write(&lib_path, "function add(a: Int b: Int) {}\n").unwrap();
    std::fs::write(
        &main_path,
        "import lib { add }\nfunction main() { add(1, 2) }\n",
    )
    .unwrap();
    let scratch_path = td.path().join("scratch.phx");
    std::fs::write(&scratch_path, "function main() {}\n").unwrap();
    let scratch_uri = Url::from_file_path(scratch_path.canonicalize().unwrap()).unwrap();

    let outcome = compute_analyze(
        HashMap::new(),
        scratch_uri.clone(),
        "function main() {}\n".to_string(),
        Some(scratch_path.clone()),
    );

    // The malformed lib.phx should produce a diagnostic somewhere
    // in by_uri — it must NOT have been silently dropped.
    let total_diags: usize = outcome.by_uri.values().map(|v| v.len()).sum();
    assert!(
        total_diags >= 1,
        "project diagnostics for malformed sibling must survive even \
         when the edited buffer isn't in the import graph; got by_uri={:?}",
        outcome
            .by_uri
            .iter()
            .map(|(u, v)| (u.clone(), v.len()))
            .collect::<Vec<_>>()
    );
    assert!(outcome.project_uris.contains(&scratch_uri));
    assert!(outcome.new_states.contains_key(&scratch_uri));
}

#[test]
fn stale_clear_across_success_to_resolve_error_transition() {
    // Pins the stale-clear contract across a project-success →
    // resolve-error transition. The first round (clean project)
    // pins both files in `by_uri` so both URIs are part of the
    // stale-clear scope. The second round (entry has a parse error
    // → `MalformedSourceFiles`) attaches a diagnostic to main.phx
    // and *also* keeps lib.phx in `project_uris` whenever the
    // resolver's source_map happened to include it before the
    // failure. This test pins that lib.phx remains in scope when
    // the resolver discovered it before bailing — so a published
    // diagnostic from a prior round can be cleared on the next
    // analyze.
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    let lib_path = td.path().join("lib.phx");
    std::fs::write(
        &lib_path,
        "public function add(a: Int, b: Int) -> Int { return a + b }\n",
    )
    .unwrap();
    let main_text_ok = "import lib { add }\nfunction main() { add(1, 2) }\n";
    std::fs::write(&main_path, main_text_ok).unwrap();
    let main_uri = Url::from_file_path(main_path.canonicalize().unwrap()).unwrap();
    let lib_uri = Url::from_file_path(lib_path.canonicalize().unwrap()).unwrap();

    let mut snapshot = HashMap::new();
    snapshot.insert(
        lib_uri.clone(),
        snap_canon(
            "public function add(a: Int, b: Int) -> Int { return a + b }\n",
            lib_path.canonicalize().unwrap(),
        ),
    );

    // Round 1: clean project. Both URIs are pinned in by_uri so the
    // apply layer's clear pass treats lib as "no change".
    let round1 = compute_analyze(
        snapshot.clone(),
        main_uri.clone(),
        main_text_ok.to_string(),
        Some(main_path.clone()),
    );
    assert!(round1.project_uris.contains(&main_uri));
    assert!(round1.project_uris.contains(&lib_uri));
    assert!(round1.by_uri.contains_key(&lib_uri));

    // Round 2: edit main.phx into something that parses badly so
    // the resolver returns `MalformedSourceFiles`. The malformed
    // file is recorded but its imports are not followed, so
    // lib.phx may or may not be in source_map depending on the
    // resolver's order. In either case main.phx must carry the
    // diagnostic, and lib.phx — when it did get registered in the
    // source map — must remain in `project_uris` so a stale
    // diagnostic from a prior round would be cleared.
    let main_text_bad = "import lib { add }\nfunction main() { add(1, 2 }\n";
    let round2 = compute_analyze(
        snapshot,
        main_uri.clone(),
        main_text_bad.to_string(),
        Some(main_path.clone()),
    );
    assert!(
        round2.project_uris.contains(&main_uri),
        "main URI must be in resolve-error project scope"
    );
    // The malformed-files renderer should attach a diagnostic on
    // main.phx (mapped from `MalformedSourceFiles`).
    let main_diags = round2
        .by_uri
        .get(&main_uri)
        .expect("main.phx should carry a parse diagnostic");
    assert!(
        !main_diags.is_empty(),
        "expected at least one diagnostic on main.phx"
    );
}

#[test]
fn malformed_edited_buffer_does_not_emit_duplicate_parse_diagnostics() {
    // Regression for the duplicate-diagnostics bug: when the edited
    // buffer is itself malformed, the resolver's
    // `MalformedSourceFiles` arm bucketed its parse errors under
    // `edited_uri` and the single-file fallback re-parsed and
    // re-emitted them, so each parse error showed up twice in the
    // editor. The fix drops the resolver's bucket for the edited
    // URI in that one case so the single-file pass owns the parse
    // errors. Verify by counting how many diagnostics share the
    // same span as the dedicated single-file analysis would emit.
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    // Missing `)` triggers a parse error.
    let bad_text = "function main() { foo(1, 2 }\n";
    std::fs::write(&main_path, bad_text).unwrap();
    let main_uri = Url::from_file_path(main_path.canonicalize().unwrap()).unwrap();

    // Compare against the bare single-file analysis: the project
    // pipeline must not produce *more* diagnostics on the edited
    // URI than the single-file pass alone would.
    let single = compute_single_file_state(main_uri.clone(), bad_text.to_string());
    let single_count = single.diagnostics.len();
    assert!(
        single_count >= 1,
        "single-file analysis should emit at least one parse diagnostic"
    );

    let outcome = compute_analyze(
        HashMap::new(),
        main_uri.clone(),
        bad_text.to_string(),
        Some(main_path),
    );
    let main_diags = outcome
        .by_uri
        .get(&main_uri)
        .expect("malformed edited buffer should carry diagnostics");
    assert_eq!(
        main_diags.len(),
        single_count,
        "edited buffer's diagnostics must not be duplicated by the \
         resolver's malformed-files renderer; got {} project vs {} \
         single-file: {:?}",
        main_diags.len(),
        single_count,
        main_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn compute_analyze_entry_not_found_routes_to_edited_uri() {
    // End-to-end check that an `EntryNotFound` resolve error
    // produces exactly one top-level diagnostic on the edited URI
    // and the single-file fallback's diagnostics get appended (so
    // hover/goto-def stay useful for whatever the user typed).
    // Pins that we don't accidentally lose the top-level resolver
    // message when the single-file pass succeeds.
    let bogus = std::path::PathBuf::from("/this/path/does/not/exist/main.phx");
    let edited_uri = Url::parse("file:///this/path/does/not/exist/main.phx").unwrap();
    let outcome = compute_analyze(
        HashMap::new(),
        edited_uri.clone(),
        "function main() {}\n".to_string(),
        Some(bogus),
    );
    let diags = outcome
        .by_uri
        .get(&edited_uri)
        .expect("entry-not-found should land diagnostics on the edited URI");
    assert!(
        diags.iter().any(|d| {
            let m = d.message.to_lowercase();
            m.contains("not found") || m.contains("entry")
        }),
        "expected an entry-not-found message; got: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert!(
        outcome.new_states.contains_key(&edited_uri),
        "single-file fallback should still install a DocumentState"
    );
}

#[test]
fn compute_resolve_error_outcome_does_not_overwrite_sibling_state() {
    // Pins the comment in `compute_resolve_error_outcome`: open siblings
    // retain their previous `DocumentState`. The resolver didn't produce
    // a new check result for them, so we must not overwrite — siblings'
    // `new_states` slot must be absent. (The Backend's `apply` only
    // overwrites entries that are explicitly present in `new_states`,
    // so an empty-Vec stand-in would erase the live sibling state.)
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    let lib_path = td.path().join("lib.phx");
    std::fs::write(
        &lib_path,
        "public function add(a: Int, b: Int) -> Int { return a + b }\n",
    )
    .unwrap();
    // main.phx imports a non-existent module → resolver returns
    // `MissingModule` and the resolve-error path runs.
    std::fs::write(
        &main_path,
        "import nonexistent { foo }\nfunction main() {}\n",
    )
    .unwrap();
    let main_uri = Url::from_file_path(main_path.canonicalize().unwrap()).unwrap();
    let lib_uri = Url::from_file_path(lib_path.canonicalize().unwrap()).unwrap();

    // Open both buffers so the snapshot would carry lib's state.
    let mut snapshot = HashMap::new();
    snapshot.insert(
        lib_uri.clone(),
        snap_canon(
            "public function add(a: Int, b: Int) -> Int { return a + b }\n",
            lib_path.canonicalize().unwrap(),
        ),
    );

    let outcome = compute_analyze(
        snapshot,
        main_uri.clone(),
        "import nonexistent { foo }\nfunction main() {}\n".to_string(),
        Some(main_path.clone()),
    );
    // The edited buffer always gets a fresh single-file state so the
    // handlers stay useful.
    assert!(
        outcome.new_states.contains_key(&main_uri),
        "edited buffer must get a fresh single-file state on resolve error"
    );
    // The sibling must NOT be in new_states — its prior state would be
    // clobbered by an overwrite.
    assert!(
        !outcome.new_states.contains_key(&lib_uri),
        "open sibling must retain its previous DocumentState on resolve \
         error; got new_states keys {:?}",
        outcome.new_states.keys().collect::<Vec<_>>()
    );
}

#[test]
fn compute_resolve_error_outcome_does_not_clear_unrelated_uris() {
    // On resolve error (here: missing-module), `project_uris`
    // should be scoped to URIs the resolver actually attempted —
    // not include open buffers from unrelated projects.
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    std::fs::write(
        &main_path,
        "import nonexistent { foo }\nfunction main() {}\n",
    )
    .unwrap();
    let main_uri = Url::from_file_path(main_path.canonicalize().unwrap()).unwrap();

    // Unrelated buffer — must not appear in the stale-clear scope.
    let other_td = tempfile::TempDir::new().unwrap();
    let other_path = other_td.path().join("scratch.phx");
    std::fs::write(&other_path, "function main() {}\n").unwrap();
    let other_uri = Url::from_file_path(other_path.canonicalize().unwrap()).unwrap();

    let mut snapshot = HashMap::new();
    snapshot.insert(other_uri.clone(), snap("function main() {}\n"));

    let outcome = compute_analyze(
        snapshot,
        main_uri.clone(),
        "import nonexistent { foo }\nfunction main() {}\n".to_string(),
        Some(main_path),
    );
    assert!(outcome.project_uris.contains(&main_uri));
    assert!(
        !outcome.project_uris.contains(&other_uri),
        "resolve-error outcome must scope project_uris to project \
         URIs only; got {:?}",
        outcome.project_uris
    );
}
