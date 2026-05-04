//! Per-document state stored in the LSP backend.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use phoenix_common::module_path::ModulePath;
use phoenix_common::source::SourceMap;
use phoenix_common::span::SourceId;
use phoenix_sema::Analysis;
use tower_lsp::lsp_types::Url;

/// Per-document state: the parsed program, check result, and project-wide
/// source map.
///
/// When the project pipeline succeeds, every open document in the same
/// project shares a single [`SourceMap`], a single [`Analysis`], and a
/// single `SourceId → Url` map (all held via `Arc`) so cross-file
/// diagnostic spans — e.g. a privacy-note pointing at the declaration
/// in another file — resolve against one consistent view. The `Arc`
/// wrappers are also why a project-wide refresh stays cheap on every
/// keystroke: each open sibling clones an `Arc` rather than the full
/// analysis payload.
///
/// Fallback paths:
/// - **Resolve error** and **non-`file://` URI**: the edited buffer
///   holds its own freshly-built shared bundle, and any sibling buffers
///   retain whatever they had on their last project-success update.
///   Siblings here are *not* refreshed — there is no new bundle to share.
/// - **Edited buffer reachable-but-not-in-graph** ("partial fallback"):
///   the resolver succeeded for the project but the edited file isn't
///   reached from the entry. Only the *edited buffer's* state falls back
///   to single-file; the project's siblings still receive the new shared
///   bundle for that round.
///
/// In other words, the shared-bundle property is a property of the
/// *most recent successful project analyze*, not an invariant — and on
/// the partial-fallback path (resolver succeeded, edited buffer outside
/// the graph) the property still holds for siblings.
pub(crate) struct DocumentState {
    /// The raw source text of this document.
    ///
    /// Duplicated with `source_map.contents(source_id)` deliberately:
    /// `position_to_offset` reads this on every hover/goto-def request,
    /// and a direct `&str` field avoids a `SourceMap` indirection per
    /// keystroke. The source map is the single source of truth for
    /// diagnostic line/column conversion; this field is only read by
    /// the cursor-positioning helpers.
    pub(crate) source: String,
    /// Project-wide source map populated by the resolver. Shared (via
    /// `Arc`) across every open document in the same project.
    pub(crate) source_map: Arc<SourceMap>,
    /// This file's source id within `source_map`.
    pub(crate) source_id: SourceId,
    /// This file's module path within the project. The entry file uses
    /// `ModulePath::entry()`; siblings use their dotted module path.
    pub(crate) current_module: ModulePath,
    /// Project-wide semantic analysis (multi-module). `Arc`-shared so
    /// every open sibling reads the same instance instead of paying a
    /// full `Analysis` clone per keystroke.
    pub(crate) check_result: Arc<Analysis>,
    /// `SourceId → Url` for every parsed file in the project. Used by
    /// `to_lsp_diagnostic` (cross-file note URIs) and by the
    /// `goto_definition`/`references`/`rename` handlers to point at the
    /// right document. `Arc`-shared alongside `check_result`.
    pub(crate) source_id_to_url: Arc<HashMap<SourceId, Url>>,
    /// `SourceId → ModulePath` for every parsed file in the project.
    /// Lets find-references / rename qualify each candidate reference
    /// against *its own* module's scope before comparing identities, so
    /// two unrelated `add` declarations in different modules don't get
    /// conflated into one rename. `Arc`-shared alongside `check_result`.
    pub(crate) source_id_to_module: Arc<HashMap<SourceId, ModulePath>>,
    /// Cached canonical filesystem path for this document, computed
    /// once when the state is built. Lets the per-keystroke
    /// `compute_project_outcome` sibling loop look up each open buffer's
    /// resolved module via a hashmap probe instead of paying a fresh
    /// `canonicalize` syscall per file per keystroke. `None` for
    /// non-`file://` URIs, or when the underlying file no longer exists.
    pub(crate) canonical_path: Option<PathBuf>,
}
