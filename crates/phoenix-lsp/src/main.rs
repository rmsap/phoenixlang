//! Phoenix Language Server.
//!
//! Provides IDE features for `.phx` files via the Language Server Protocol:
//! diagnostics, hover, autocomplete, go-to-definition, find references, and rename.
#![warn(missing_docs)]

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

mod convert;
mod handlers;
mod project;
mod state;

use crate::handlers::{
    completion_items_for, goto_definition_at, hover_at, references_at, rename_at,
};
use crate::project::{AnalyzeOutcome, BufferSnapshot, compute_analyze};
use crate::state::DocumentState;

/// The Phoenix language server backend.
pub(crate) struct Backend {
    /// The LSP client handle for sending notifications (e.g., diagnostics).
    pub(crate) client: Client,
    /// Per-document analysis state, keyed by document URI.
    pub(crate) documents: Mutex<HashMap<Url, DocumentState>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
        }
    }

    /// Re-parses and re-checks the project containing `uri`, publishing
    /// diagnostics for every affected document.
    ///
    /// Snapshots the open `documents` map under the lock, hands it to
    /// the pure compute pipeline in `project`, then applies the
    /// resulting state writes and diagnostic publishes. The lock is
    /// *not* held across canonicalize syscalls, parsing, or `await`
    /// points.
    pub(crate) async fn analyze(&self, uri: Url, text: String) {
        let edited_path = uri.to_file_path().ok();
        let snapshot = self.snapshot_open_documents();
        let outcome = compute_analyze(snapshot, uri, text, edited_path);
        self.apply(outcome).await;
    }

    /// Snapshot every open buffer's source plus its cached canonical
    /// path under the documents lock. The returned map is owned, so the
    /// caller can run filesystem work without holding the lock. The
    /// cached canonical path lets the project pipeline route each open
    /// buffer to its module via a hashmap probe instead of paying a
    /// fresh `canonicalize` syscall per file per keystroke.
    fn snapshot_open_documents(&self) -> HashMap<Url, BufferSnapshot> {
        let docs = self.documents.lock().expect("document lock poisoned");
        docs.iter()
            .map(|(u, s)| {
                (
                    u.clone(),
                    BufferSnapshot {
                        source: s.source.clone(),
                        canonical_path: s.canonical_path.clone(),
                    },
                )
            })
            .collect()
    }

    /// Apply an [`AnalyzeOutcome`]: write new `DocumentState`s under
    /// the documents lock (single acquisition), then publish per-URI
    /// diagnostics, then clear stale diagnostics on URIs that
    /// participated in the project but weren't represented this round.
    async fn apply(&self, outcome: AnalyzeOutcome) {
        let AnalyzeOutcome {
            by_uri,
            new_states,
            project_uris,
        } = outcome;
        {
            let mut docs = self.documents.lock().expect("document lock poisoned");
            for (uri, state) in new_states {
                docs.insert(uri, state);
            }
        }
        // Snapshot the set of URIs we're about to publish so the
        // stale-clear pass can check membership after `by_uri` is
        // consumed by the publish loop. The URI clones here are the
        // unavoidable price of needing to reference them from the
        // second loop without keeping the diagnostic Vecs alive.
        let touched: HashSet<Url> = by_uri.keys().cloned().collect();
        // Sequential awaits are intentional: tower-lsp's `Client` enqueues
        // every notification onto a single shared mpsc sink, so a
        // `join_all` here wouldn't actually parallelize the underlying I/O —
        // the messages would still be drained in order. Keeping the loop
        // sequential avoids pulling in `futures` for no real win.
        for (uri, diags) in by_uri {
            self.client.publish_diagnostics(uri, diags, None).await;
        }
        // Stale-clear is scoped to URIs *this* analyze touched, so an
        // unrelated open buffer (different project, scratch file, etc.)
        // is never inadvertently cleared. A URI that's in `project_uris`
        // but missing from `touched` is a project file that produced no
        // diagnostics this round — clear any stale ones.
        for uri in project_uris {
            if !touched.contains(&uri) {
                self.client.publish_diagnostics(uri, vec![], None).await;
            }
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    /// Declares server capabilities: full text sync, hover, completion,
    /// go-to-definition, find-references, and rename.
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// Logs a startup message after the client confirms initialization.
    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "Phoenix LSP initialized")
            .await;
    }

    /// Graceful shutdown — no cleanup needed.
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// Analyzes a newly opened document and publishes diagnostics.
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.analyze(params.text_document.uri, params.text_document.text)
            .await;
    }

    /// Re-analyzes a document after edits and publishes updated diagnostics.
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            self.analyze(params.text_document.uri, change.text).await;
        }
    }

    /// Removes document state and clears diagnostics when a file is closed.
    ///
    /// If the closing buffer participated in any other open buffer's
    /// project (the closed URI appears in some sibling's
    /// `source_id_to_url`), the project's overlay-derived analysis is
    /// now stale — siblings still hold an `Arc<Analysis>` built from the
    /// gone buffer's unsaved contents. Pick any one remaining sibling
    /// and re-analyze it; that single pass refreshes the shared
    /// `Arc<Analysis>` for every other open buffer in the project,
    /// switching the closed file to disk content.
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let closed_uri = params.text_document.uri;
        let reanalyze: Option<(Url, String)> = {
            let mut docs = self.documents.lock().expect("document lock poisoned");
            docs.remove(&closed_uri);
            docs.iter()
                .find(|(_, s)| s.source_id_to_url.values().any(|u| *u == closed_uri))
                .map(|(u, s)| (u.clone(), s.source.clone()))
        };
        self.client
            .publish_diagnostics(closed_uri, vec![], None)
            .await;
        if let Some((uri, text)) = reanalyze {
            self.analyze(uri, text).await;
        }
    }

    /// Returns the resolved type at the cursor position as a Markdown hover.
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let docs = self.documents.lock().expect("document lock poisoned");
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };
        Ok(hover_at(state, pos))
    }

    /// Returns completion items: struct/enum/function names visible in
    /// the current module, plus keywords.
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let docs = self.documents.lock().expect("document lock poisoned");
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };
        Ok(Some(CompletionResponse::Array(completion_items_for(state))))
    }

    /// Jumps to the definition of the symbol under the cursor.
    ///
    /// In multi-module mode (the project pipeline succeeded on the most
    /// recent analyze), this returns the definition's location even if
    /// it lives in a different file than the cursor. On single-file
    /// fallback (resolve error, edited buffer outside the import graph,
    /// non-`file://` URI) only same-file definitions resolve.
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let docs = self.documents.lock().expect("document lock poisoned");
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };
        Ok(goto_definition_at(state, pos).map(GotoDefinitionResponse::Scalar))
    }

    /// Returns every location where the symbol under the cursor is
    /// referenced. In multi-module mode this spans every file the most
    /// recent project analyze touched; on single-file fallback it
    /// covers only the edited buffer.
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let docs = self.documents.lock().expect("document lock poisoned");
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };
        Ok(references_at(state, pos))
    }

    /// Renames a symbol across every reference the most recent analyze
    /// recorded. In multi-module mode the returned `WorkspaceEdit`
    /// carries a per-URI bucket so each affected file gets its own
    /// coherent edit stream; on single-file fallback the edit set is
    /// scoped to the edited buffer only.
    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let new_name = &params.new_name;
        let docs = self.documents.lock().expect("document lock poisoned");
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };
        Ok(rename_at(state, pos, new_name))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
