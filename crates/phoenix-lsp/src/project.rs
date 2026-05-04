//! Project-aware analysis pipeline for the LSP backend.
//!
//! The pipeline:
//! 1. Walk up from the opened file to find a `main.phx` entry.
//! 2. Run `phoenix-modules`'s resolver with an in-memory overlay of
//!    every open buffer's contents.
//! 3. On success: run `check_modules` and publish per-file diagnostics
//!    (every project file gets its own slice; open siblings get their
//!    `DocumentState` refreshed against the shared `Arc<SourceMap>`).
//! 4. On failure: render the resolve error to LSP diagnostics, fold in a
//!    single-file analysis of the edited buffer (so handlers stay
//!    useful), and publish.
//!
//! Every free function in this module is pure (no `Client`, no shared
//! state) and testable in isolation. The async [`Backend::analyze`] /
//! [`Backend::apply`] methods snapshot the `documents` map under the
//! lock, run the pure pipeline, and apply the resulting state writes
//! and diagnostic publishes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use phoenix_common::module_path::ModulePath;
use phoenix_common::source::SourceMap;
use phoenix_common::span::{SourceId, Span};
use phoenix_lexer::lexer::tokenize;
use phoenix_modules::{ResolveError, ResolvedSourceModule, resolve_with_overlay};
use phoenix_parser::parser;
use phoenix_sema::checker;
use tower_lsp::lsp_types::*;

use crate::convert::{span_to_range, to_lsp_diagnostic};
use crate::state::DocumentState;

/// A snapshot of one open buffer's text plus its cached canonical
/// filesystem path. The Backend takes one of these per open document
/// under the documents lock and hands the bag to [`compute_analyze`];
/// the cached canonical path lets the project pipeline route each open
/// buffer to its module via a hashmap probe instead of paying a fresh
/// `canonicalize` syscall per file per keystroke.
#[derive(Clone)]
pub(crate) struct BufferSnapshot {
    pub source: String,
    pub canonical_path: Option<PathBuf>,
}

/// Outcome of a single `analyze` invocation.
///
/// Pure compute helpers populate this struct; the `Backend` then
/// applies the writes/publishes. Splitting the computation from the
/// I/O is what lets us unit-test the routing, single-file fallback,
/// and stale-clear scoping without standing up a tower-lsp `Client`.
pub(crate) struct AnalyzeOutcome {
    /// Per-URI LSP diagnostics to publish this round.
    pub by_uri: HashMap<Url, Vec<Diagnostic>>,
    /// Per-URI `DocumentState` writes (edited buffer plus every open
    /// sibling that participates in the project).
    pub new_states: HashMap<Url, DocumentState>,
    /// URIs that participated in *this* analyze (every project file
    /// plus the edited buffer). The clear-stale pass is scoped to
    /// these, so unrelated open buffers don't get their diagnostics
    /// wiped when the user edits an unrelated file.
    pub project_uris: HashSet<Url>,
}

/// Walk up from `opened`'s parent directory looking for an ancestor that
/// contains `main.phx`; that's the project entry. Bounded by either the
/// filesystem root or the first ancestor that contains a `.git` directory
/// (project boundary marker, prevents the LSP from walking out of the
/// project tree on disk).
///
/// In a tree that is *not* under a `.git` directory the walk continues
/// to the filesystem root — one `exists()` syscall per ancestor.
/// Intentional: `phoenix` projects opened outside a git checkout still
/// need to find their `main.phx`, and the per-keystroke cost is only
/// paid on the first `did_open` for each buffer (subsequent analyses
/// reuse the cached canonical path on the buffer's `DocumentState`).
///
/// If no ancestor `main.phx` exists, the opened file is its own entry —
/// matches the previous single-file LSP behaviour. Note that for an
/// unsaved buffer that doesn't exist on disk yet, the resolver will fail
/// to canonicalize `opened` and the analyze pipeline falls back to
/// single-file analysis on the in-memory text.
pub(crate) fn discover_entry_for(opened: &Path) -> PathBuf {
    let mut cur = opened.parent();
    while let Some(dir) = cur {
        let candidate = dir.join("main.phx");
        if candidate.exists() {
            return candidate;
        }
        if dir.join(".git").exists() {
            break;
        }
        cur = dir.parent();
    }
    opened.to_path_buf()
}

/// Build a `SourceId → Url` map from a successful resolve's module list.
pub(crate) fn build_source_id_to_url(modules: &[ResolvedSourceModule]) -> HashMap<SourceId, Url> {
    let mut out = HashMap::new();
    for m in modules {
        if let Ok(url) = Url::from_file_path(&m.file_path) {
            out.insert(m.source_id, url);
        }
    }
    out
}

/// Build a `SourceId → ModulePath` map from a successful resolve. Used
/// by the LSP find-references / rename handlers to qualify each
/// candidate reference against *its own* module's scope before
/// comparing identities — without this, two same-named declarations in
/// different modules would be conflated by a bare-name comparison.
pub(crate) fn build_source_id_to_module(
    modules: &[ResolvedSourceModule],
) -> HashMap<SourceId, ModulePath> {
    modules
        .iter()
        .map(|m| (m.source_id, m.module_path.clone()))
        .collect()
}

/// On a resolver error, the modules vector isn't available. Recover the
/// `SourceId → Url` mapping from the [`SourceMap`] alone by translating
/// each registered name (which the resolver populated as either the
/// caller-supplied entry path or a root-relative path) back to a `Url`.
///
/// `entry_path` is the path passed to the resolver — its parent is the
/// project root the resolver used as the base for root-relative names.
/// (The edited file's parent is **not** a safe substitute when the
/// edited file lives in a subdirectory.)
///
/// If `entry_path.canonicalize()` fails (e.g. the entry doesn't exist
/// on disk yet) we have no anchor for root-relative names and return
/// an empty map; we deliberately do *not* fall back to CWD-relative
/// resolution because that can silently misroute names through
/// whatever directory the LSP happens to have been started from.
/// Callers will then route diagnostics through the fallback URI,
/// which is the safe outcome.
///
/// Beyond that anchor: any name that doesn't resolve to a filesystem
/// path is silently skipped — the diagnostic falls back to the edited
/// document's URI.
pub(crate) fn build_source_id_to_url_from_map(
    source_map: &SourceMap,
    entry_path: &Path,
) -> HashMap<SourceId, Url> {
    let mut out = HashMap::new();
    let Ok(entry_canon) = entry_path.canonicalize() else {
        return out;
    };
    let Some(project_root) = entry_canon.parent() else {
        return out;
    };

    for raw in 0..source_map.len() {
        let id = SourceId(raw);
        let name = source_map.name(id);
        let path = PathBuf::from(name);
        let resolved = if path.is_absolute() {
            path.canonicalize().ok()
        } else {
            project_root.join(&path).canonicalize().ok()
        };
        if let Some(resolved) = resolved
            && let Ok(url) = Url::from_file_path(&resolved)
        {
            out.insert(id, url);
        }
    }
    out
}

/// Render a [`ResolveError`] into per-URI LSP diagnostics.
///
/// Pure: no I/O, no client interaction. Returns the diagnostic groups
/// the caller should publish. `fallback_uri` is used for diagnostics
/// whose source id isn't in `source_id_to_url` (e.g. errors with no
/// span at all — `EntryNotFound`/`FileReadFailures`).
pub(crate) fn render_resolve_error(
    err: &ResolveError,
    source_map: &SourceMap,
    source_id_to_url: &HashMap<SourceId, Url>,
    fallback_uri: &Url,
) -> HashMap<Url, Vec<Diagnostic>> {
    let mut by_uri: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
    // Render an error diagnostic. `span = None` means there's no
    // span-bearing error site (`EntryNotFound` / `FileReadFailures`); we
    // attach a zero-range diagnostic to `fallback_uri` instead.
    let mut push_diag = |span: Option<Span>, message: String| {
        let (uri, range) = match span {
            Some(s) => {
                let uri = source_id_to_url
                    .get(&s.source_id)
                    .cloned()
                    .unwrap_or_else(|| fallback_uri.clone());
                (uri, span_to_range(&s, source_map))
            }
            None => (fallback_uri.clone(), Range::default()),
        };
        by_uri.entry(uri).or_default().push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("phoenix".to_string()),
            message,
            ..Default::default()
        });
    };

    match err {
        ResolveError::MalformedSourceFiles { files } => {
            // `MalformedSourceFiles` is special: each entry already carries
            // a fully-formed list of `phoenix_common::Diagnostic`s, so they
            // route through `to_lsp_diagnostic` rather than `push_diag`.
            for (path, diags) in files {
                let uri = Url::from_file_path(path).unwrap_or_else(|_| fallback_uri.clone());
                let lsp_diags: Vec<Diagnostic> = diags
                    .iter()
                    .map(|d| to_lsp_diagnostic(d, source_map, source_id_to_url, &uri))
                    .collect();
                by_uri.entry(uri).or_default().extend(lsp_diags);
            }
        }
        ResolveError::MissingModule {
            path, import_span, ..
        } => push_diag(Some(*import_span), format!("cannot find module '{}'", path)),
        ResolveError::AmbiguousModule {
            path,
            file_path,
            mod_path,
            import_span,
        } => push_diag(
            Some(*import_span),
            format!(
                "module '{}' is ambiguous: both {} and {} exist",
                path,
                file_path.display(),
                mod_path.display()
            ),
        ),
        ResolveError::CyclicImports {
            cycle,
            last_import_span,
        } => {
            let names: Vec<String> = cycle.iter().map(|m| m.dotted()).collect();
            push_diag(
                Some(*last_import_span),
                format!("cyclic module imports: {}", names.join(" → ")),
            );
        }
        ResolveError::EscapesRoot {
            requested_path,
            import_span,
        } => push_diag(
            Some(*import_span),
            format!(
                "import path escapes project root: {}",
                requested_path.display()
            ),
        ),
        ResolveError::EntryNotFound { .. } | ResolveError::FileReadFailures { .. } => {
            push_diag(None, err.to_string());
        }
    }

    by_uri
}

/// Single-file analysis result: the `DocumentState` to store under the
/// edited URI, plus the LSP diagnostics it produced.
pub(crate) struct SingleFileResult {
    pub state: DocumentState,
    pub diagnostics: Vec<Diagnostic>,
}

/// Compute a single-file analysis of `text` for `uri`. Pure — no
/// `Client`, no shared state. Used by the non-file URI path, the
/// resolve-error fallback, and the "edited file outside import graph"
/// branch of `compute_project_outcome`.
pub(crate) fn compute_single_file_state(uri: Url, text: String) -> SingleFileResult {
    let path = uri
        .to_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| uri.to_string());

    let mut source_map = SourceMap::new();
    let source_id = source_map.add(&path, &text);
    let tokens = tokenize(&text, source_id);
    let (program, parse_errors) = parser::parse(&tokens);

    let mut source_id_to_url: HashMap<SourceId, Url> = HashMap::new();
    source_id_to_url.insert(source_id, uri.clone());
    let mut source_id_to_module: HashMap<SourceId, ModulePath> = HashMap::new();
    source_id_to_module.insert(source_id, ModulePath::entry());

    let mut diagnostics: Vec<Diagnostic> = parse_errors
        .iter()
        .map(|d| to_lsp_diagnostic(d, &source_map, &source_id_to_url, &uri))
        .collect();

    let check_result = checker::check(&program);
    diagnostics.extend(
        check_result
            .diagnostics
            .iter()
            .map(|d| to_lsp_diagnostic(d, &source_map, &source_id_to_url, &uri)),
    );

    let canonical_path = uri.to_file_path().ok().and_then(|p| p.canonicalize().ok());

    SingleFileResult {
        state: DocumentState {
            source: text,
            source_map: Arc::new(source_map),
            source_id,
            current_module: ModulePath::entry(),
            check_result: Arc::new(check_result),
            source_id_to_url: Arc::new(source_id_to_url),
            source_id_to_module: Arc::new(source_id_to_module),
            canonical_path,
        },
        diagnostics,
    }
}

/// Build the resolver overlay from a snapshot of every open buffer's
/// source. Keys are file paths derived from the URI; the resolver
/// canonicalizes them on entry, so callers don't need to pre-canonicalize.
///
/// Values borrow from the snapshot and from `edited_text`, so the only
/// per-keystroke heap allocation here is the overlay's own `HashMap`
/// spine — none of the buffer source bytes are copied.
///
/// The snapshot is taken before this analyze installs the edited
/// buffer's new text, so it still carries `edited_uri`'s *previous*
/// contents. We deliberately overwrite that entry with `edited_text`
/// after the loop so the resolver sees what the user just typed.
fn build_overlay_from_snapshot<'a>(
    snapshot: &'a HashMap<Url, BufferSnapshot>,
    edited_uri: &Url,
    edited_text: &'a str,
) -> HashMap<PathBuf, &'a str> {
    let mut overlay: HashMap<PathBuf, &'a str> = HashMap::new();
    for (other_uri, buf) in snapshot {
        if other_uri == edited_uri {
            continue;
        }
        if let Ok(p) = other_uri.to_file_path() {
            overlay.insert(p, buf.source.as_str());
        }
    }
    if let Ok(p) = edited_uri.to_file_path() {
        overlay.insert(p, edited_text);
    }
    overlay
}

/// Pure single-file outcome: used when the URI isn't a `file://` URL,
/// or any other case where the project pipeline can't run.
fn compute_single_file_outcome(uri: Url, text: String) -> AnalyzeOutcome {
    let SingleFileResult { state, diagnostics } = compute_single_file_state(uri.clone(), text);
    let mut by_uri = HashMap::new();
    by_uri.insert(uri.clone(), diagnostics);
    let mut new_states = HashMap::new();
    new_states.insert(uri.clone(), state);
    let mut project_uris = HashSet::new();
    project_uris.insert(uri);
    AnalyzeOutcome {
        by_uri,
        new_states,
        project_uris,
    }
}

/// Pure outcome computation for the success path. Runs `check_modules`,
/// buckets diagnostics per file URI, and synthesizes fresh
/// `DocumentState`s for every open buffer that participates in the
/// project. If the edited buffer isn't reachable from the entry, its
/// state falls back to single-file analysis but the project's siblings
/// still get their per-file diagnostics — the edited buffer's analysis
/// is *merged* on top, not substituted.
pub(crate) fn compute_project_outcome(
    snapshot: &HashMap<Url, BufferSnapshot>,
    edited_uri: &Url,
    edited_text: String,
    edited_canon: Option<&Path>,
    source_map: Arc<SourceMap>,
    modules: Vec<ResolvedSourceModule>,
) -> AnalyzeOutcome {
    let source_id_to_url = Arc::new(build_source_id_to_url(&modules));
    let source_id_to_module = Arc::new(build_source_id_to_module(&modules));
    let analysis = Arc::new(checker::check_modules(&modules));

    // Bucket project diagnostics by URI. A diagnostic whose source id
    // isn't in the URL map (defensive — shouldn't normally happen) is
    // routed to the edited URI as the last-resort fallback.
    let mut by_uri: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
    for diag in &analysis.diagnostics {
        let uri = source_id_to_url
            .get(&diag.span.source_id)
            .cloned()
            .unwrap_or_else(|| edited_uri.clone());
        let lsp_diag = to_lsp_diagnostic(diag, &source_map, &source_id_to_url, &uri);
        by_uri.entry(uri).or_default().push(lsp_diag);
    }

    // Refresh `DocumentState` for every open sibling that maps to a
    // project module. They share the new `Arc<SourceMap>`, `Arc<Analysis>`,
    // and `Arc<HashMap<SourceId, Url>>`, so cross-file handlers stay
    // coherent and per-keystroke cost is one `Arc::clone` per open file
    // rather than a full `Analysis` clone. Each project sibling URI is
    // also pinned in `by_uri` (with an empty Vec when it has no
    // diagnostics) so the stale-clear pass leaves it alone even when
    // this round produces no diagnostics for it.
    //
    // `module_by_path` indexes the resolver's modules by canonical path
    // so the sibling loop below does a single hashmap lookup per open
    // buffer instead of a linear scan over every project module. The
    // canonical path comes from `BufferSnapshot::canonical_path` (cached
    // on the prior analyze that built the sibling's `DocumentState`), so
    // there's no per-keystroke `canonicalize` syscall. Snapshots whose
    // cache is `None` (a buffer that didn't exist on disk at its last
    // analyze) get one fallback syscall — the cached `None` would
    // otherwise lock them out of the project forever.
    let module_by_path: HashMap<&Path, &ResolvedSourceModule> =
        modules.iter().map(|m| (m.file_path.as_path(), m)).collect();
    let edited_module = edited_canon.and_then(|c| module_by_path.get(c).copied());

    let mut new_states: HashMap<Url, DocumentState> = HashMap::new();
    for (other_uri, buf) in snapshot {
        if other_uri == edited_uri {
            continue;
        }
        let canon: PathBuf = match &buf.canonical_path {
            Some(p) => p.clone(),
            None => {
                let Ok(other_path) = other_uri.to_file_path() else {
                    continue;
                };
                let Ok(c) = other_path.canonicalize() else {
                    continue;
                };
                c
            }
        };
        let Some(&other_module) = module_by_path.get(canon.as_path()) else {
            continue;
        };
        new_states.insert(
            other_uri.clone(),
            DocumentState {
                source: buf.source.clone(),
                source_map: source_map.clone(),
                source_id: other_module.source_id,
                current_module: other_module.module_path.clone(),
                check_result: analysis.clone(),
                source_id_to_url: source_id_to_url.clone(),
                source_id_to_module: source_id_to_module.clone(),
                canonical_path: Some(canon),
            },
        );
        // Pin sibling URI in `by_uri` so the stale-clear pass treats
        // a clean sibling as "no change" instead of "needs clearing".
        by_uri.entry(other_uri.clone()).or_default();
    }

    if let Some(em) = edited_module {
        new_states.insert(
            edited_uri.clone(),
            DocumentState {
                source: edited_text,
                source_map,
                source_id: em.source_id,
                current_module: em.module_path.clone(),
                check_result: analysis,
                source_id_to_url,
                source_id_to_module,
                canonical_path: edited_canon.map(|p| p.to_path_buf()),
            },
        );
        by_uri.entry(edited_uri.clone()).or_default();
    } else {
        // Edited buffer isn't part of the import graph (e.g. a scratch
        // buffer alongside the project, or a file that hasn't been
        // imported yet). Project-sibling diagnostics in `by_uri` are
        // left alone; the edited buffer gets a single-file analysis
        // and any diagnostics from that get merged in.
        let single = compute_single_file_state(edited_uri.clone(), edited_text);
        new_states.insert(edited_uri.clone(), single.state);
        by_uri
            .entry(edited_uri.clone())
            .or_default()
            .extend(single.diagnostics);
    }

    // Stale-clear scope. Including every project file the resolver
    // touched (open or not) would publish empty diagnostics to N closed
    // files on every keystroke; instead, scope to URIs that already
    // carry a `by_uri` entry — the success path pins every open sibling
    // and the edited URI in `by_uri` (with an empty Vec when there are
    // no diagnostics), so this captures all open buffers that belong to
    // the project plus any closed-but-buggy file without fanning out to
    // closed-and-clean siblings. A closed-not-open project file with
    // stale diagnostics from a prior round will *not* be cleared here;
    // the editor's Problems panel will retain that diagnostic until the
    // file opens or its real diagnostic transitions. The trade-off
    // favours keystroke-time JSON-RPC volume.
    let mut project_uris: HashSet<Url> = by_uri.keys().cloned().collect();
    project_uris.insert(edited_uri.clone());

    AnalyzeOutcome {
        by_uri,
        new_states,
        project_uris,
    }
}

/// Pure outcome computation for the resolve-error path. Renders the
/// resolver error per-URI, folds in a single-file analysis of the
/// edited buffer (so handlers stay useful), and scopes `project_uris`
/// to URIs that are open *or* received a diagnostic this round.
pub(crate) fn compute_resolve_error_outcome(
    edited_uri: &Url,
    edited_text: String,
    entry_path: &Path,
    source_map: Arc<SourceMap>,
    err: ResolveError,
) -> AnalyzeOutcome {
    let source_id_to_url = build_source_id_to_url_from_map(&source_map, entry_path);
    let mut by_uri = render_resolve_error(&err, &source_map, &source_id_to_url, edited_uri);

    // When the edited buffer is itself a malformed file, the resolver's
    // renderer already bucketed its parse errors under `edited_uri`. The
    // single-file fallback below would re-parse the same buffer and
    // re-emit the same parse errors, producing duplicates in the editor.
    // Drop the resolver's bucket for the edited URI in that one case so
    // the single-file pass owns the parse errors. We deliberately keep
    // the resolver's diagnostics on `edited_uri` for *other* error
    // shapes (e.g. `MissingModule` whose import_span lands on the entry
    // file) — single-file analysis can't reproduce those.
    //
    // The `p == ec` comparison relies on `phoenix-modules` populating
    // `MalformedSourceFiles::files` with canonical paths
    // (`malformed_source_files_paths_are_canonical` in phoenix-modules
    // pins this); if that invariant ever breaks, this dedup silently
    // misses and duplicate parse diagnostics return.
    let edited_canon = edited_uri
        .to_file_path()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    if let (ResolveError::MalformedSourceFiles { files }, Some(ec)) = (&err, edited_canon.as_ref())
        && files.iter().any(|(p, _)| p == ec)
    {
        by_uri.remove(edited_uri);
    }

    // Single-file fallback for the edited buffer so hover/goto-def stay
    // useful. The fallback diagnostics are *appended* to whatever the
    // resolver renderer attached to this URI — we never overwrite the
    // resolver's message (other than the malformed-self dedup above).
    let single = compute_single_file_state(edited_uri.clone(), edited_text);
    by_uri
        .entry(edited_uri.clone())
        .or_default()
        .extend(single.diagnostics);

    let mut new_states: HashMap<Url, DocumentState> = HashMap::new();
    new_states.insert(edited_uri.clone(), single.state);
    // Note: open siblings retain their previous `DocumentState` — the
    // resolver didn't produce a new check result for them, so we don't
    // overwrite. They appear in `project_uris` (so the stale-clear pass
    // can wipe an old diagnostic if the renderer no longer emits one),
    // but only when the buffer is open *or* the renderer attached
    // something to it.

    // Stale-clear scope. Same trade-off as `compute_project_outcome`:
    // URIs that received diagnostics this round, plus the edited URI.
    // We don't include open siblings here — on resolve error their
    // `DocumentState` is preserved, so their previously-published
    // diagnostics should also stay until the next successful analyze
    // either re-emits or supersedes them.
    let mut project_uris: HashSet<Url> = by_uri.keys().cloned().collect();
    project_uris.insert(edited_uri.clone());

    AnalyzeOutcome {
        by_uri,
        new_states,
        project_uris,
    }
}

/// Pure dispatch: snapshot in, outcome out. Picks the project-success,
/// resolve-error, or single-file branch based on the URI shape and
/// resolver result.
pub(crate) fn compute_analyze(
    snapshot: HashMap<Url, BufferSnapshot>,
    edited_uri: Url,
    edited_text: String,
    edited_path: Option<PathBuf>,
) -> AnalyzeOutcome {
    let Some(opened_path) = edited_path else {
        // Non-file URI (untitled buffer, custom scheme): can't run the
        // project pipeline. Fall back to single-file.
        return compute_single_file_outcome(edited_uri, edited_text);
    };
    let entry_path = discover_entry_for(&opened_path);
    let overlay = build_overlay_from_snapshot(&snapshot, &edited_uri, &edited_text);

    let mut source_map = SourceMap::new();
    let resolve_result = resolve_with_overlay(&entry_path, &mut source_map, &overlay);
    let source_map = Arc::new(source_map);

    match resolve_result {
        Ok(modules) => {
            // Canonicalize once here and pass the result down so
            // `compute_project_outcome` doesn't re-syscall to find the
            // edited file's module.
            let edited_canon = opened_path.canonicalize().ok();
            compute_project_outcome(
                &snapshot,
                &edited_uri,
                edited_text,
                edited_canon.as_deref(),
                source_map,
                modules,
            )
        }
        Err(err) => {
            compute_resolve_error_outcome(&edited_uri, edited_text, &entry_path, source_map, err)
        }
    }
}

#[cfg(test)]
mod tests;
