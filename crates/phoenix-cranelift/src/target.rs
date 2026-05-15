//! Compilation target abstraction for the Cranelift backend.
//!
//! The backend can emit native object files (today) and WebAssembly
//! modules (incrementally during Phase 2.4 — see
//! `docs/design-decisions.md` §Phase 2.4 WebAssembly compilation).
//! [`Target`] is the single parameter that selects between them;
//! [`crate::compile`] and downstream callers (driver, tests, benches)
//! thread it through so the choice stays explicit at every entry point.
//!
//! In Phase 2.4 PR 1 only [`Target::Native`] produces output. The two
//! WASM variants are accepted at the CLI and propagated through the
//! pipeline so the abstraction is in place; they return a
//! "not-yet-implemented" `CompileError` until their codegen lands in
//! PR 2+ (linear-memory) and PR 5+ (WASM GC).

/// Compilation target for the Cranelift backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Target {
    /// Host triple, native object + system linker, shadow-stack rooted
    /// `MarkSweepHeap`. The only target that produces output in
    /// Phase 2.4 PR 1.
    #[default]
    Native,
    /// 32-bit WebAssembly with a linear-memory `MarkSweepHeap` port;
    /// shadow stack reused as today's linked-list-on-heap. Lands in
    /// Phase 2.4 PR 2+. The exact wasm32 ABI variant (preview1 vs
    /// preview2 vs unknown-unknown) is settled by PR 2's Cranelift
    /// ISA config.
    Wasm32Linear,
    /// 32-bit WebAssembly with WASM GC managed refs backing the heap;
    /// shadow-stack emission suppressed per
    /// [decision A](../../docs/design-decisions.md#a-root-finding-precise-via-shadow-stack).
    /// Lands in Phase 2.4 PR 5+. ABI variant settled by PR 5's
    /// Cranelift ISA config.
    Wasm32Gc,
}

impl Target {
    /// Every variant in declaration order. Single source of truth that
    /// drives [`Target::from_cli`] and [`Target::all_cli_names`] so
    /// adding a variant only requires updating this slice and the
    /// exhaustive `as_cli` match (the compiler enforces the latter).
    const ALL: &'static [Self] = &[Self::Native, Self::Wasm32Linear, Self::Wasm32Gc];

    /// Whether this target emits WebAssembly (vs a native object).
    /// Used to gate the system-linker step and the host-import surface.
    pub fn is_wasm(self) -> bool {
        matches!(self, Self::Wasm32Linear | Self::Wasm32Gc)
    }

    /// The CLI spelling used by `phoenix build --target`. The inverse
    /// of [`Target::from_cli`]. The match is exhaustive so adding a
    /// variant to the enum without registering its CLI name is a
    /// compile error.
    pub fn as_cli(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Wasm32Linear => "wasm32-linear",
            Self::Wasm32Gc => "wasm32-gc",
        }
    }

    /// Parse a `--target` CLI string into a [`Target`]. Returns `None`
    /// for unrecognized values so the driver can produce a uniform
    /// "unknown target" diagnostic listing every accepted spelling.
    pub fn from_cli(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_cli() == s)
    }

    /// Every [`Target`] variant's CLI spelling. The driver renders this
    /// list in the "unknown --target" diagnostic so a typo points at
    /// the right answer.
    pub fn all_cli_names() -> Vec<&'static str> {
        Self::ALL.iter().map(|t| t.as_cli()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant the test suite knows about. Kept in sync with
    /// [`Target::ALL`] by [`all_variants_present_in_all_slice`] below
    /// so a new variant that's missing from `Target::ALL` (and would
    /// therefore be invisible to `from_cli` / `all_cli_names`) trips a
    /// test failure instead of a silent CLI gap.
    const VARIANTS_FOR_TEST: &[Target] = &[Target::Native, Target::Wasm32Linear, Target::Wasm32Gc];

    #[test]
    fn default_is_native() {
        assert_eq!(Target::default(), Target::Native);
    }

    #[test]
    fn is_wasm_matches_variants() {
        assert!(!Target::Native.is_wasm());
        assert!(Target::Wasm32Linear.is_wasm());
        assert!(Target::Wasm32Gc.is_wasm());
    }

    #[test]
    fn from_cli_round_trip() {
        for variant in VARIANTS_FOR_TEST {
            assert_eq!(Target::from_cli(variant.as_cli()), Some(*variant));
        }
    }

    #[test]
    fn from_cli_rejects_unknown() {
        assert_eq!(Target::from_cli("wasm"), None);
        assert_eq!(Target::from_cli(""), None);
        assert_eq!(Target::from_cli("Native"), None); // case-sensitive
    }

    #[test]
    fn all_cli_names_matches_from_cli() {
        for name in Target::all_cli_names() {
            assert!(
                Target::from_cli(name).is_some(),
                "all_cli_names() lists {name} but from_cli rejects it",
            );
        }
    }

    /// Bidirectional check: every variant must appear in [`Target::ALL`]
    /// (and therefore in `all_cli_names()` / be reachable via
    /// `from_cli`). Without this, a variant added to the enum + `as_cli`
    /// match but forgotten in `ALL` would silently disappear from the
    /// CLI without any compile or test signal.
    #[test]
    fn all_variants_present_in_all_slice() {
        for variant in VARIANTS_FOR_TEST {
            assert!(
                Target::ALL.contains(variant),
                "variant {variant:?} missing from Target::ALL — \
                 from_cli/all_cli_names will silently skip it",
            );
        }
        assert_eq!(
            Target::ALL.len(),
            VARIANTS_FOR_TEST.len(),
            "Target::ALL and VARIANTS_FOR_TEST disagree on the variant count",
        );
    }
}
