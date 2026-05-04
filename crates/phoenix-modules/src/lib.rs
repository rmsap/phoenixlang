//! Module resolver for the Phoenix programming language.
//!
//! The resolver takes the path to an entry `.phx` file and produces a
//! deterministic, topologically-ordered list of [`ResolvedSourceModule`]s
//! reachable from that entry via the transitive import graph. Discovery is
//! lazy: only files reachable through `import` statements are parsed.
//!
//! See `docs/design-decisions.md` "Module system: discovery, root,
//! `mod.phx`, and entry-point rules" for the design rationale.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use phoenix_common::diagnostics::Diagnostic;
use phoenix_common::source::SourceMap;
use phoenix_common::span::{SourceId, Span};
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::ast::{Declaration, ImportDecl, Program};

pub use phoenix_common::module_path::ModulePath;

/// One parsed `.phx` file with its module path and identity metadata.
#[derive(Debug)]
pub struct ResolvedSourceModule {
    /// The dotted module path (`models.user`); empty for the entry file.
    pub module_path: ModulePath,
    /// The `SourceId` allocated when this file was added to the `SourceMap`.
    pub source_id: SourceId,
    /// The parsed AST.
    pub program: Program,
    /// True for exactly one module in the result — the file passed to
    /// [`resolve`].
    pub is_entry: bool,
    /// The canonical absolute path to the source file.
    pub file_path: PathBuf,
}

/// Errors produced by [`resolve`] when the import graph cannot be assembled.
#[derive(Debug)]
pub enum ResolveError {
    /// Neither `<root>/<path>.phx` nor `<root>/<path>/mod.phx` exists.
    MissingModule {
        path: ModulePath,
        import_span: Span,
        probed: Vec<PathBuf>,
    },
    /// Both `<root>/<path>.phx` and `<root>/<path>/mod.phx` exist.
    AmbiguousModule {
        path: ModulePath,
        file_path: PathBuf,
        mod_path: PathBuf,
        import_span: Span,
    },
    /// The import graph contains a cycle.
    ///
    /// `cycle` lists the module paths in cycle order (e.g., `[a, b, a]`
    /// for `a` imports `b` imports `a`).
    CyclicImports {
        cycle: Vec<ModulePath>,
        last_import_span: Span,
    },
    /// One or more discovered files failed to lex/parse.
    ///
    /// The original parser diagnostics are forwarded so the driver can
    /// render them with full source context. The resolver continues
    /// past a malformed file to surface every parse error in a single
    /// run (matching the existing single-file flow's "report all parse
    /// diagnostics, not just the first" behavior). Applies to the entry
    /// file as well as any imported module file.
    MalformedSourceFiles {
        files: Vec<(PathBuf, Vec<Diagnostic>)>,
    },
    /// A canonicalized file path landed outside the project root.
    ///
    /// Defensive guard against mod.phx symlink shenanigans; not reachable
    /// from plain `import` syntax today (no `..` in import paths).
    EscapesRoot {
        requested_path: PathBuf,
        import_span: Span,
    },
    /// The entry file does not exist or could not be opened.
    EntryNotFound { path: PathBuf, error: String },
    /// One or more discovered module files existed at probe time but could
    /// not be read (permissions, race, etc.). Distinct from `EntryNotFound`
    /// so callers and tests can disambiguate the two failure modes.
    ///
    /// Accumulates multiple read failures into one error so a single bad
    /// permission bit on one sibling doesn't mask read failures elsewhere
    /// in the project. Mirrors `MalformedSourceFiles`.
    FileReadFailures { files: Vec<(PathBuf, String)> },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::MissingModule { path, probed, .. } => {
                write!(f, "cannot find module '{}' (tried: ", path)?;
                for (i, p) in probed.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", p.display())?;
                }
                f.write_str(")")
            }
            ResolveError::AmbiguousModule {
                path,
                file_path,
                mod_path,
                ..
            } => write!(
                f,
                "module '{}' is ambiguous: both {} and {} exist",
                path,
                file_path.display(),
                mod_path.display()
            ),
            ResolveError::CyclicImports { cycle, .. } => {
                f.write_str("cyclic module imports: ")?;
                for (i, m) in cycle.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" → ")?;
                    }
                    write!(f, "{}", m)?;
                }
                Ok(())
            }
            ResolveError::MalformedSourceFiles { files } => {
                f.write_str("failed to parse source file(s): ")?;
                for (i, (path, _)) in files.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", path.display())?;
                }
                Ok(())
            }
            ResolveError::EscapesRoot { requested_path, .. } => write!(
                f,
                "import path escapes project root: {}",
                requested_path.display()
            ),
            ResolveError::EntryNotFound { path, error } => {
                write!(f, "entry file {} not found: {}", path.display(), error)
            }
            ResolveError::FileReadFailures { files } => {
                f.write_str("failed to read source file(s): ")?;
                for (i, (path, error)) in files.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{} ({})", path.display(), error)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolve the import graph rooted at `entry_file` into a deterministic,
/// topologically-ordered list of parsed modules.
///
/// The supplied `source_map` is mutated as files are read in. Each parsed
/// file is added to it and the resulting `SourceId` is stored on the
/// returned [`ResolvedSourceModule`].
///
/// On success, the first element of the returned vector is always the
/// entry module (with `is_entry == true`); subsequent elements are imported
/// modules in BFS-from-entry order with lexical-path tiebreak. This order
/// is load-bearing: downstream FuncId/StructId allocators iterate it and
/// require it to be deterministic across runs.
pub fn resolve(
    entry_file: &Path,
    source_map: &mut SourceMap,
) -> Result<Vec<ResolvedSourceModule>, ResolveError> {
    resolve_with_overlay(entry_file, source_map, &HashMap::new())
}

/// Resolve the import graph rooted at `entry_file`, consulting an in-memory
/// overlay before reading from disk.
///
/// `overlay` maps file paths to their in-memory contents. The resolver
/// canonicalizes every key on entry — overlay paths must therefore name
/// files that already exist on disk, since the BFS only ever asks about
/// canonical paths. Keys that fail to canonicalize are dropped silently
/// (the resolver will fall back to `std::fs::read_to_string` for the
/// discovered file). When the resolver is about to read a discovered
/// file, it probes the canonicalized overlay first; on a hit it uses
/// the overlay contents and skips the disk read. On a miss it falls back
/// to `std::fs::read_to_string` just as [`resolve`] does.
///
/// If two overlay keys canonicalize to the same path, exactly one of the
/// colliding values is used; which one is unspecified (HashMap iteration
/// order). Callers that hand in distinct files never hit this.
///
/// Contents are borrowed (`&str`): the resolver copies into the
/// [`SourceMap`] only the contents of files it actually reads, so callers
/// don't pay an unconditional `String` clone for buffers the BFS skips
/// (e.g. an open scratch file that isn't part of this project).
///
/// This is the path the LSP uses so unsaved buffer contents flow through
/// the resolver without writing them to disk.
pub fn resolve_with_overlay(
    entry_file: &Path,
    source_map: &mut SourceMap,
    overlay: &HashMap<PathBuf, &str>,
) -> Result<Vec<ResolvedSourceModule>, ResolveError> {
    // Canonicalize every overlay key once on entry so the BFS below can
    // do a direct lookup against the canonical paths it discovers
    // (`entry_canon` for the entry, paths returned by `discover_import_file`
    // for siblings — both already canonical). Keys whose files don't
    // exist on disk are dropped: the BFS only ever asks about canonical
    // paths, so a literal-match fallback is unreachable in practice.
    let canonical_overlay: HashMap<PathBuf, &str> = overlay
        .iter()
        .filter_map(|(k, v)| k.canonicalize().ok().map(|c| (c, *v)))
        .collect();

    let entry_canon = entry_file
        .canonicalize()
        .map_err(|e| ResolveError::EntryNotFound {
            path: entry_file.to_path_buf(),
            error: e.to_string(),
        })?;
    let root = entry_canon
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| ResolveError::EntryNotFound {
            path: entry_file.to_path_buf(),
            error: "entry file has no parent directory".to_string(),
        })?;
    // Canonicalize the root once so per-import `ensure_under_root` calls are
    // O(1) instead of paying a syscall each.
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());

    // BFS over modules. Each queue entry is (module_path, file_path).
    let mut queue: VecDeque<(ModulePath, PathBuf)> = VecDeque::new();
    let mut seen: HashSet<ModulePath> = HashSet::new();
    let mut out: Vec<ResolvedSourceModule> = Vec::new();
    // Per-module list of imports observed (for the post-BFS cycle pass).
    let mut import_edges: HashMap<ModulePath, Vec<(ModulePath, Span)>> = HashMap::new();
    // Accumulator for parse errors. Multiple broken files surface in a
    // single run instead of bailing on the first — mirrors how the
    // single-file path returns every parser diagnostic at once.
    let mut malformed: Vec<(PathBuf, Vec<Diagnostic>)> = Vec::new();
    // Accumulator for read failures on imported modules. A non-entry
    // file that can't be read does not bail the BFS; sibling subtrees
    // (and their own malformed/read failures) are still surfaced in the
    // same run.
    let mut read_failures: Vec<(PathBuf, String)> = Vec::new();

    queue.push_back((ModulePath::entry(), entry_canon.clone()));

    while let Some((mp, file_path)) = queue.pop_front() {
        if seen.contains(&mp) {
            continue;
        }
        seen.insert(mp.clone());

        // Read + parse.  The entry file gets a different error variant from
        // imported files: an unreadable entry is fatal (we have no graph to
        // explore without it), but an unreadable sibling is accumulated.
        // Editor overlays short-circuit the disk read: an open buffer's
        // contents take precedence over what is on disk.
        let contents = if let Some(buf) = canonical_overlay.get(&file_path) {
            buf.to_string()
        } else {
            match std::fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(e) if mp.is_entry() => {
                    return Err(ResolveError::EntryNotFound {
                        path: file_path,
                        error: e.to_string(),
                    });
                }
                Err(e) => {
                    read_failures.push((file_path.clone(), e.to_string()));
                    continue;
                }
            }
        };
        // Entry's display name reproduces the caller-supplied path so
        // single-file invocations keep emitting diagnostics under the same
        // prefix the user typed (`/abs/path/foo.phx:LINE` or
        // `relative/foo.phx:LINE`). Imported modules use a root-relative
        // form because their absolute paths are an artifact of the
        // canonicalize step, not anything the user named.
        let display_name = if mp.is_entry() {
            entry_file.display().to_string()
        } else {
            display_name_for(&file_path, &root)
        };
        let source_id = source_map.add(display_name, contents);
        let tokens = tokenize(source_map.contents(source_id), source_id);
        let (program, parse_diags) = phoenix_parser::parser::parse(&tokens);
        if !parse_diags.is_empty() {
            // Record the failure but keep going. A malformed file's import
            // list cannot be trusted, so we don't queue its imports — but
            // sibling modules from other branches of the BFS still get
            // parsed and any of their own parse errors are captured too.
            malformed.push((file_path.clone(), parse_diags));
            continue;
        }

        // Walk imports from this module's AST and resolve each to a file
        // path (plus capture the per-import span for diagnostic carry).
        let mut my_edges: Vec<(ModulePath, Span)> = Vec::new();
        for imp in iter_imports(&program) {
            let target = ModulePath(imp.path.clone());
            let resolved = resolve_module_path(&root, &root_canon, &target, imp.span)?;
            my_edges.push((target.clone(), imp.span));
            queue.push_back((target, resolved));
        }
        // Sort edges lexically for deterministic cycle reports.
        my_edges.sort_by(|a, b| a.0.cmp(&b.0));
        import_edges.insert(mp.clone(), my_edges);

        out.push(ResolvedSourceModule {
            is_entry: mp.is_entry(),
            module_path: mp,
            source_id,
            program,
            file_path,
        });
    }

    // If any file could not be read, surface every collected failure
    // first — read failures are more fundamental than parse failures
    // (you can't parse what you can't read) and prioritising them gives
    // the user one consolidated "fix your filesystem" message before
    // the parse-error list.
    if !read_failures.is_empty() {
        return Err(ResolveError::FileReadFailures {
            files: read_failures,
        });
    }

    // If any file failed to parse, surface every collected error in a
    // single result. We do this before the entry-first reordering below
    // because a malformed entry file would not be in `out`.
    if !malformed.is_empty() {
        return Err(ResolveError::MalformedSourceFiles { files: malformed });
    }

    // Stable order: entry first, then the rest sorted by module path. This
    // pins the order across runs even when BFS-discovery order would vary
    // (it doesn't today, but a future parallel-parse change shouldn't
    // perturb FuncId allocation). `is_entry` reverses the bool order so
    // `true` (entry) sorts before `false`; module-path tie-breaks the
    // remainder.
    out.sort_by(|a, b| {
        b.is_entry
            .cmp(&a.is_entry)
            .then_with(|| a.module_path.cmp(&b.module_path))
    });

    // Cycle detection on the import graph.
    if let Some(cycle) = detect_cycle(&import_edges) {
        // Pick the span of the last import in the reported cycle for the
        // diagnostic anchor.
        let last_import_span = cycle
            .windows(2)
            .last()
            .and_then(|pair| {
                let (from, to) = (&pair[0], &pair[1]);
                import_edges
                    .get(from)?
                    .iter()
                    .find(|(t, _)| t == to)
                    .map(|(_, s)| *s)
            })
            .unwrap_or(Span::BUILTIN);
        return Err(ResolveError::CyclicImports {
            cycle,
            last_import_span,
        });
    }

    Ok(out)
}

/// Iterate over all `Declaration::Import` nodes in a program, in source order.
fn iter_imports(program: &Program) -> impl Iterator<Item = &ImportDecl> {
    program.declarations.iter().filter_map(|d| match d {
        Declaration::Import(imp) => Some(imp),
        _ => None,
    })
}

/// Resolve a `ModulePath` to a concrete file path under `root`.
///
/// Tries `<root>/<a>/<b>/<c>.phx` first, then `<root>/<a>/<b>/<c>/mod.phx`.
/// `root_canon` is the pre-canonicalized root used by `ensure_under_root`.
fn resolve_module_path(
    root: &Path,
    root_canon: &Path,
    path: &ModulePath,
    import_span: Span,
) -> Result<PathBuf, ResolveError> {
    let mut file_path = root.to_path_buf();
    for (i, segment) in path.0.iter().enumerate() {
        if i + 1 == path.0.len() {
            file_path.push(format!("{}.phx", segment));
        } else {
            file_path.push(segment);
        }
    }
    let mut mod_path = root.to_path_buf();
    for segment in &path.0 {
        mod_path.push(segment);
    }
    mod_path.push("mod.phx");

    let file_exists = file_path.exists();
    let mod_exists = mod_path.exists();

    match (file_exists, mod_exists) {
        (true, true) => Err(ResolveError::AmbiguousModule {
            path: path.clone(),
            file_path,
            mod_path,
            import_span,
        }),
        (true, false) => {
            let canon = file_path
                .canonicalize()
                .map_err(|_| ResolveError::MissingModule {
                    path: path.clone(),
                    import_span,
                    probed: vec![file_path.clone(), mod_path.clone()],
                })?;
            ensure_under_root(&canon, root_canon, import_span)?;
            Ok(canon)
        }
        (false, true) => {
            let canon = mod_path
                .canonicalize()
                .map_err(|_| ResolveError::MissingModule {
                    path: path.clone(),
                    import_span,
                    probed: vec![file_path.clone(), mod_path.clone()],
                })?;
            ensure_under_root(&canon, root_canon, import_span)?;
            Ok(canon)
        }
        (false, false) => Err(ResolveError::MissingModule {
            path: path.clone(),
            import_span,
            probed: vec![file_path, mod_path],
        }),
    }
}

/// Defensive check that a canonicalized path lives under the project root.
///
/// Not reachable from plain `import` syntax today (paths don't contain
/// `..`), but symlinks inside `mod.phx` discovery could in principle
/// escape the tree. `root_canon` is supplied pre-canonicalized by
/// [`resolve`] so this check stays O(1).
fn ensure_under_root(
    canon: &Path,
    root_canon: &Path,
    import_span: Span,
) -> Result<(), ResolveError> {
    if canon.starts_with(root_canon) {
        Ok(())
    } else {
        Err(ResolveError::EscapesRoot {
            requested_path: canon.to_path_buf(),
            import_span,
        })
    }
}

/// Build a display name for a file relative to `root`, with a fallback to
/// the absolute path if the relative computation fails.
fn display_name_for(file_path: &Path, root: &Path) -> String {
    file_path
        .strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| file_path.display().to_string())
}

/// Detect a cycle in the import graph using DFS with an on-stack set.
///
/// Returns the cycle as a sequence of module paths from the back-edge
/// target through the rest of the cycle, ending at the back-edge target
/// again. Returns `None` if no cycle exists.
///
/// The implementation is recursive. Realistic Phoenix projects have
/// import depths in the dozens at most, well within the default thread
/// stack. If pathological inputs ever surface (deep auto-generated module
/// trees, etc.) consider switching to an explicit stack — the structure
/// of the algorithm (visited / on-stack / parent-stack) translates
/// directly to an iterative form.
fn detect_cycle(edges: &HashMap<ModulePath, Vec<(ModulePath, Span)>>) -> Option<Vec<ModulePath>> {
    let mut visited: HashSet<ModulePath> = HashSet::new();
    let mut on_stack: HashSet<ModulePath> = HashSet::new();
    let mut stack: Vec<ModulePath> = Vec::new();

    // Sort starting roots for determinism (same cycle regardless of map
    // iteration order).
    let mut roots: Vec<&ModulePath> = edges.keys().collect();
    roots.sort();

    for root in roots {
        if visited.contains(root) {
            continue;
        }
        if let Some(cycle) = dfs_cycle(root, edges, &mut visited, &mut on_stack, &mut stack) {
            return Some(cycle);
        }
    }
    None
}

fn dfs_cycle(
    node: &ModulePath,
    edges: &HashMap<ModulePath, Vec<(ModulePath, Span)>>,
    visited: &mut HashSet<ModulePath>,
    on_stack: &mut HashSet<ModulePath>,
    stack: &mut Vec<ModulePath>,
) -> Option<Vec<ModulePath>> {
    visited.insert(node.clone());
    on_stack.insert(node.clone());
    stack.push(node.clone());

    if let Some(neighbors) = edges.get(node) {
        for (next, _span) in neighbors {
            if on_stack.contains(next) {
                // Back-edge: extract the cycle slice from the stack.
                let start = stack.iter().position(|m| m == next).unwrap_or(0);
                let mut cycle = stack[start..].to_vec();
                cycle.push(next.clone());
                return Some(cycle);
            }
            if !visited.contains(next)
                && let Some(cycle) = dfs_cycle(next, edges, visited, on_stack, stack)
            {
                return Some(cycle);
            }
        }
    }

    on_stack.remove(node);
    stack.pop();
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_file(dir: &Path, rel: &str, contents: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, contents).unwrap();
        path
    }

    fn modules_for(entry: &Path) -> Result<Vec<ResolvedSourceModule>, ResolveError> {
        let mut sm = SourceMap::new();
        resolve(entry, &mut sm)
    }

    #[test]
    fn entry_only_no_imports() {
        let td = TempDir::new().unwrap();
        let entry = write_file(td.path(), "main.phx", "function main() {}\n");
        let out = modules_for(&entry).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].is_entry);
        assert_eq!(out[0].module_path, ModulePath::entry());
    }

    #[test]
    fn entry_imports_sibling() {
        let td = TempDir::new().unwrap();
        write_file(
            td.path(),
            "helpers.phx",
            "public function add(a: Int, b: Int) -> Int { a + b }\n",
        );
        let entry = write_file(
            td.path(),
            "main.phx",
            "import helpers { add }\nfunction main() {}\n",
        );
        let out = modules_for(&entry).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].is_entry);
        assert_eq!(out[1].module_path, ModulePath(vec!["helpers".into()]));
        assert!(!out[1].is_entry);
    }

    #[test]
    fn nested_directory_module() {
        let td = TempDir::new().unwrap();
        write_file(
            td.path(),
            "models/user.phx",
            "public function makeUser() {}\n",
        );
        let entry = write_file(
            td.path(),
            "main.phx",
            "import models.user { makeUser }\nfunction main() {}\n",
        );
        let out = modules_for(&entry).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[1].module_path,
            ModulePath(vec!["models".into(), "user".into()])
        );
    }

    #[test]
    fn mod_phx_makes_directory_a_module() {
        let td = TempDir::new().unwrap();
        write_file(
            td.path(),
            "models/mod.phx",
            "public function modelsTop() {}\n",
        );
        let entry = write_file(
            td.path(),
            "main.phx",
            "import models { modelsTop }\nfunction main() {}\n",
        );
        let out = modules_for(&entry).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].module_path, ModulePath(vec!["models".into()]));
    }

    #[test]
    fn missing_module_error() {
        let td = TempDir::new().unwrap();
        let entry = write_file(
            td.path(),
            "main.phx",
            "import doesnotexist { foo }\nfunction main() {}\n",
        );
        match modules_for(&entry) {
            Err(ResolveError::MissingModule { path, probed, .. }) => {
                assert_eq!(path, ModulePath(vec!["doesnotexist".into()]));
                // Both probed paths should be surfaced so the diagnostic
                // can guide the user to either spelling (file or mod.phx).
                assert_eq!(
                    probed.len(),
                    2,
                    "expected two probed paths, got {:?}",
                    probed
                );
                assert!(
                    probed.iter().any(|p| p.ends_with("doesnotexist.phx")),
                    "expected probed to include `doesnotexist.phx`; got {:?}",
                    probed
                );
                assert!(
                    probed.iter().any(|p| p.ends_with("doesnotexist/mod.phx")),
                    "expected probed to include `doesnotexist/mod.phx`; got {:?}",
                    probed
                );
            }
            other => panic!("expected MissingModule, got {:?}", other),
        }
    }

    #[test]
    fn ambiguous_module_error() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "models.phx", "public function fromFile() {}\n");
        write_file(
            td.path(),
            "models/mod.phx",
            "public function fromMod() {}\n",
        );
        let entry = write_file(
            td.path(),
            "main.phx",
            "import models { fromFile }\nfunction main() {}\n",
        );
        match modules_for(&entry) {
            Err(ResolveError::AmbiguousModule { path, .. }) => {
                assert_eq!(path, ModulePath(vec!["models".into()]));
            }
            other => panic!("expected AmbiguousModule, got {:?}", other),
        }
    }

    #[test]
    fn cyclic_imports_error() {
        let td = TempDir::new().unwrap();
        write_file(
            td.path(),
            "a.phx",
            "import b { fromB }\npublic function fromA() {}\n",
        );
        write_file(
            td.path(),
            "b.phx",
            "import a { fromA }\npublic function fromB() {}\n",
        );
        let entry = write_file(
            td.path(),
            "main.phx",
            "import a { fromA }\nfunction main() {}\n",
        );
        match modules_for(&entry) {
            Err(ResolveError::CyclicImports { cycle, .. }) => {
                // Expect the cycle to involve `a` and `b` (in either order).
                let names: Vec<String> = cycle.iter().map(|m| m.dotted()).collect();
                assert!(
                    names.iter().any(|n| n == "a") && names.iter().any(|n| n == "b"),
                    "expected cycle to include a and b, got {:?}",
                    names
                );
            }
            other => panic!("expected CyclicImports, got {:?}", other),
        }
    }

    #[test]
    fn malformed_source_file_error() {
        let td = TempDir::new().unwrap();
        write_file(
            td.path(),
            "broken.phx",
            "this is not valid phoenix code @@@\n",
        );
        let entry = write_file(
            td.path(),
            "main.phx",
            "import broken { foo }\nfunction main() {}\n",
        );
        match modules_for(&entry) {
            Err(ResolveError::MalformedSourceFiles { files }) => {
                assert_eq!(files.len(), 1);
                let (path, diags) = &files[0];
                assert!(path.ends_with("broken.phx"));
                assert!(!diags.is_empty());
            }
            other => panic!("expected MalformedSourceFiles, got {:?}", other),
        }
    }

    #[test]
    fn malformed_source_files_paths_are_canonical() {
        // Pins the invariant that downstream consumers (the LSP's
        // edited-buffer dedup in `compute_resolve_error_outcome`) rely
        // on: every entry in `MalformedSourceFiles::files` carries the
        // canonical path of the file, not whatever path the resolver's
        // BFS happened to start with. Without this, the LSP's `p == ec`
        // dedup silently misses and parse diagnostics are duplicated in
        // the editor.
        let td = TempDir::new().unwrap();
        let broken = write_file(td.path(), "broken.phx", "this is not valid @@@\n");
        let entry = write_file(
            td.path(),
            "main.phx",
            "import broken { foo }\nfunction main() {}\n",
        );
        let broken_canon = broken.canonicalize().unwrap();
        match modules_for(&entry) {
            Err(ResolveError::MalformedSourceFiles { files }) => {
                assert!(
                    files.iter().any(|(p, _)| p == &broken_canon),
                    "expected the canonical path {:?} in MalformedSourceFiles; got {:?}",
                    broken_canon,
                    files.iter().map(|(p, _)| p).collect::<Vec<_>>()
                );
                // And every entry must already be canonical.
                for (p, _) in &files {
                    let canon = p
                        .canonicalize()
                        .expect("path in malformed list should exist on disk");
                    assert_eq!(
                        p, &canon,
                        "MalformedSourceFiles paths must be canonical (not {:?})",
                        p
                    );
                }
            }
            other => panic!("expected MalformedSourceFiles, got {:?}", other),
        }
    }

    #[test]
    fn malformed_files_are_accumulated() {
        // Two siblings each have a parse error.  Both should appear in
        // the returned `files` vector — bailing on the first would have
        // been a regression from the single-file flow that surfaces
        // every parser diagnostic at once.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "alpha.phx", "this is not valid @@@\n");
        write_file(td.path(), "beta.phx", "still not valid @@@\n");
        let entry = write_file(
            td.path(),
            "main.phx",
            "import alpha { foo }\nimport beta { bar }\nfunction main() {}\n",
        );
        match modules_for(&entry) {
            Err(ResolveError::MalformedSourceFiles { files }) => {
                assert_eq!(
                    files.len(),
                    2,
                    "expected both malformed files to be reported, got {:?}",
                    files
                );
                assert!(files.iter().any(|(p, _)| p.ends_with("alpha.phx")));
                assert!(files.iter().any(|(p, _)| p.ends_with("beta.phx")));
            }
            other => panic!("expected MalformedSourceFiles, got {:?}", other),
        }
    }

    #[test]
    fn output_is_deterministic() {
        // Run resolve twice; module ordering must match exactly.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "z.phx", "public function z() {}\n");
        write_file(td.path(), "a.phx", "public function a() {}\n");
        write_file(td.path(), "m.phx", "public function m() {}\n");
        let entry = write_file(
            td.path(),
            "main.phx",
            "import z { z }\nimport a { a }\nimport m { m }\nfunction main() {}\n",
        );
        let out1 = modules_for(&entry).unwrap();
        let out2 = modules_for(&entry).unwrap();
        let paths1: Vec<_> = out1.iter().map(|m| m.module_path.clone()).collect();
        let paths2: Vec<_> = out2.iter().map(|m| m.module_path.clone()).collect();
        assert_eq!(paths1, paths2);
        // Entry first, then sorted lexically: a, m, z.
        assert_eq!(paths1[0], ModulePath::entry());
        assert_eq!(paths1[1], ModulePath(vec!["a".into()]));
        assert_eq!(paths1[2], ModulePath(vec!["m".into()]));
        assert_eq!(paths1[3], ModulePath(vec!["z".into()]));
    }

    #[test]
    fn shared_dependency_visited_once() {
        // main → a, main → b, both a and b → c. c parsed exactly once.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "c.phx", "public function c() {}\n");
        write_file(
            td.path(),
            "a.phx",
            "import c { c }\npublic function a() {}\n",
        );
        write_file(
            td.path(),
            "b.phx",
            "import c { c }\npublic function b() {}\n",
        );
        let entry = write_file(
            td.path(),
            "main.phx",
            "import a { a }\nimport b { b }\nfunction main() {}\n",
        );
        let out = modules_for(&entry).unwrap();
        let c_count = out
            .iter()
            .filter(|m| m.module_path == ModulePath(vec!["c".into()]))
            .count();
        assert_eq!(c_count, 1);
    }

    #[test]
    fn entry_not_found_error() {
        let td = TempDir::new().unwrap();
        let bogus = td.path().join("does_not_exist.phx");
        match modules_for(&bogus) {
            Err(ResolveError::EntryNotFound { .. }) => {}
            other => panic!("expected EntryNotFound, got {:?}", other),
        }
    }

    #[test]
    fn entry_in_cycle_detected() {
        // The entry imports `a`, which imports the entry back via its module
        // name. Phoenix allows imports of any sibling file by its file-stem
        // module path, including the entry's own siblings — so `main.phx`
        // could in principle be imported as `main` from `a.phx`.
        let td = TempDir::new().unwrap();
        write_file(
            td.path(),
            "a.phx",
            "import main { fromMain }\npublic function fromA() {}\n",
        );
        let entry = write_file(
            td.path(),
            "main.phx",
            "import a { fromA }\npublic function fromMain() {}\nfunction main() {}\n",
        );
        match modules_for(&entry) {
            Err(ResolveError::CyclicImports { cycle, .. }) => {
                let names: Vec<String> = cycle.iter().map(|m| m.dotted()).collect();
                // The entry contributes the empty path; the back-edge target
                // is whichever of `<entry>` or `a` closes the cycle.
                assert!(
                    names.iter().any(|n| n == "a"),
                    "expected cycle to mention `a`, got {:?}",
                    names
                );
            }
            other => panic!("expected CyclicImports, got {:?}", other),
        }
    }

    #[cfg(unix)]
    #[test]
    fn escapes_root_via_symlink() {
        // Build a project root that contains a `models` directory. Inside
        // models we place a `mod.phx` that is actually a symlink to a file
        // outside the root; resolution should refuse to follow it.
        use std::os::unix::fs::symlink;

        let outside_dir = TempDir::new().unwrap();
        let outside_file = outside_dir.path().join("escaped.phx");
        fs::write(&outside_file, "public function leaked() {}\n").unwrap();

        let root_dir = TempDir::new().unwrap();
        let entry = write_file(
            root_dir.path(),
            "main.phx",
            "import models { leaked }\nfunction main() {}\n",
        );
        // Create models/mod.phx -> outside_file
        let models_dir = root_dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        symlink(&outside_file, models_dir.join("mod.phx")).unwrap();

        match modules_for(&entry) {
            Err(ResolveError::EscapesRoot { .. }) => {}
            other => panic!("expected EscapesRoot, got {:?}", other),
        }
    }

    #[test]
    fn module_path_display() {
        assert_eq!(ModulePath::entry().to_string(), "<entry>");
        assert_eq!(ModulePath(vec!["a".into(), "b".into()]).to_string(), "a.b");
    }

    #[test]
    fn module_path_dotted_empty() {
        assert_eq!(ModulePath::entry().dotted(), "");
    }

    #[test]
    fn intermediate_mod_phx_does_not_collide_with_nested_module() {
        // `import a.b` requires only `<root>/a/b.phx` (or `<root>/a/b/mod.phx`).
        // A coexisting `<root>/a/mod.phx` is the `a` module — independent of
        // `a.b` — and must not trigger an AmbiguousModule error.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "a/mod.phx", "public function topA() {}\n");
        write_file(td.path(), "a/b.phx", "public function nestedB() {}\n");
        let entry = write_file(
            td.path(),
            "main.phx",
            "import a.b { nestedB }\nfunction main() {}\n",
        );
        let out = modules_for(&entry).unwrap();
        // Exactly the entry and `a.b` — `a/mod.phx` is not pulled in
        // because nothing imports it.
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].module_path, ModulePath(vec!["a".into(), "b".into()]));
    }

    #[test]
    fn malformed_entry_file_surfaces_as_malformed() {
        // The entry-file's own parse failure must route through
        // MalformedSourceFiles (not silently produce an empty result).
        let td = TempDir::new().unwrap();
        let entry = write_file(td.path(), "main.phx", "this is not valid @@@\n");
        match modules_for(&entry) {
            Err(ResolveError::MalformedSourceFiles { files }) => {
                assert_eq!(files.len(), 1);
                assert!(files[0].0.ends_with("main.phx"));
                assert!(!files[0].1.is_empty());
            }
            other => panic!("expected MalformedSourceFiles, got {:?}", other),
        }
    }

    #[test]
    fn overlay_shadows_disk_contents() {
        // An entry on disk says one thing; an overlay supplies different
        // contents for the same canonical path. Resolution must use the
        // overlay version.
        let td = TempDir::new().unwrap();
        let entry = write_file(td.path(), "main.phx", "function main() {}\n");
        let entry_canon = entry.canonicalize().unwrap();

        let mut overlay = HashMap::new();
        overlay.insert(entry_canon, "import sib { greet }\nfunction main() {}\n");
        write_file(td.path(), "sib.phx", "public function greet() {}\n");

        let mut sm = SourceMap::new();
        let out = resolve_with_overlay(&entry, &mut sm, &overlay).unwrap();
        // The overlay introduced an import the on-disk file lacks; the
        // resolver should have followed it.
        assert!(
            out.iter()
                .any(|m| m.module_path == ModulePath(vec!["sib".into()])),
            "expected overlay-introduced import to be followed, got {:?}",
            out.iter().map(|m| &m.module_path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn overlay_with_non_canonical_key_is_canonicalized_and_matches() {
        // `resolve_with_overlay` canonicalizes overlay keys on entry, so
        // a key with `..` segments that resolves to the same file on
        // disk *does* match. Pins the convenience contract callers rely
        // on so the LSP doesn't have to mirror the canonicalize dance
        // for every overlay entry.
        let td = TempDir::new().unwrap();
        let entry = write_file(td.path(), "main.phx", "function main() {}\n");
        let canon = entry.canonicalize().unwrap();
        // Construct a non-canonical equivalent path: <td>/dummy/../main.phx.
        // The intermediate directory must exist on disk for
        // `canonicalize` to resolve the `..` segment.
        std::fs::create_dir(canon.parent().unwrap().join("dummy")).unwrap();
        let non_canon = canon
            .parent()
            .unwrap()
            .join("dummy")
            .join("..")
            .join("main.phx");
        // Sanity: this path is not equal as PathBuf to the canonical form.
        assert_ne!(non_canon, canon);

        let mut overlay = HashMap::new();
        overlay.insert(non_canon, "import sib { greet }\nfunction main() {}\n");
        write_file(td.path(), "sib.phx", "public function greet() {}\n");

        let mut sm = SourceMap::new();
        let out = resolve_with_overlay(&entry, &mut sm, &overlay).unwrap();
        // The overlay key canonicalizes to the same path as the entry,
        // so its contents win and the resolver follows the new
        // `import sib`.
        assert!(
            out.iter()
                .any(|m| m.module_path == ModulePath(vec!["sib".into()])),
            "canonicalized overlay key should be matched; got {:?}",
            out.iter().map(|m| &m.module_path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn overlay_for_imported_module_used_over_disk() {
        // An imported sibling exists on disk but is shadowed by an overlay
        // entry that points the importer at different exports. The overlay
        // contents should win.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "sib.phx", "public function onDisk() {}\n");
        let sib_canon = td.path().join("sib.phx").canonicalize().unwrap();
        let entry = write_file(
            td.path(),
            "main.phx",
            "import sib { fromOverlay }\nfunction main() {}\n",
        );

        let mut overlay = HashMap::new();
        overlay.insert(sib_canon, "public function fromOverlay() {}\n");

        let mut sm = SourceMap::new();
        // Without the overlay, sema would reject `fromOverlay` because it
        // isn't declared in the on-disk version. The resolver itself only
        // proves overlay use indirectly — by parsing the contents and
        // returning them; assert no parse / resolve error here.
        let out = resolve_with_overlay(&entry, &mut sm, &overlay).unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn overlay_key_collision_picks_one_arbitrarily() {
        // Two distinct overlay keys canonicalize to the same on-disk
        // path. The doc promises that exactly one of the colliding
        // values is used; which one is unspecified (HashMap iteration
        // order). Pin the contract so a future change can't silently
        // flip to "neither wins" / fall back to disk: exactly one
        // payload must make it through, and the resolver must not read
        // the on-disk contents.
        let td = TempDir::new().unwrap();
        let entry = write_file(td.path(), "main.phx", "function main() {}\n");
        let canon = entry.canonicalize().unwrap();
        // Build a second path that canonicalizes to the same file.
        std::fs::create_dir(canon.parent().unwrap().join("dummy")).unwrap();
        let alt = canon
            .parent()
            .unwrap()
            .join("dummy")
            .join("..")
            .join("main.phx");
        assert_ne!(alt, canon);
        write_file(
            td.path(),
            "from_canon.phx",
            "public function viaCanon() {}\n",
        );
        write_file(td.path(), "from_alt.phx", "public function viaAlt() {}\n");

        let mut overlay = HashMap::new();
        overlay.insert(
            canon.clone(),
            "import from_canon { viaCanon }\nfunction main() {}\n",
        );
        overlay.insert(alt, "import from_alt { viaAlt }\nfunction main() {}\n");

        let mut sm = SourceMap::new();
        let out = resolve_with_overlay(&entry, &mut sm, &overlay).unwrap();
        let module_paths: Vec<_> = out.iter().map(|m| m.module_path.dotted()).collect();
        let saw_canon = module_paths.iter().any(|p| p == "from_canon");
        let saw_alt = module_paths.iter().any(|p| p == "from_alt");
        assert!(
            saw_canon ^ saw_alt,
            "exactly one colliding overlay payload should win (last-iterated); \
             got modules={:?}",
            module_paths,
        );
    }

    #[test]
    fn entry_display_name_reproduces_caller_path() {
        // The entry's SourceMap display name must reproduce the caller-
        // supplied path verbatim so diagnostics from full-pipeline driver
        // commands keep the same prefix as the previous single-file flow
        // (which used `source_map.add(path, contents)` with the raw arg).
        let td = TempDir::new().unwrap();
        let entry = write_file(td.path(), "main.phx", "function main() {}\n");
        let mut sm = SourceMap::new();
        let out = resolve(&entry, &mut sm).unwrap();
        let entry_module = &out[0];
        assert_eq!(sm.name(entry_module.source_id), entry.display().to_string());
    }
}
