//! `TypeTag` discriminants emitted by codegen — hand-mirrored from the
//! `TypeTag` enum in `phoenix-runtime`.
//!
//! **ABI invariant.** The Cranelift IR type at the call site is `I32`
//! (signedness-agnostic — Cranelift ints don't carry a sign). The
//! runtime reads the same register as `u32`. On x86_64 SysV the u32 arg
//! goes in `esi`; the upper 32 bits of `rsi` are undefined per ABI and
//! Rust's `extern "C" fn(_: u32)` lowering reads only the low 32 bits,
//! so encoding the immediate via `i64::from(tag)` and feeding it to
//! `iconst(I32, _)` is information-preserving for any tag in
//! `0..=u32::MAX`. Drift between this file and the runtime enum is
//! caught by [`tests::type_tag_matches_runtime`] below.

/// `TypeTag::Closure` — closure-environment payload (fn-ptr + captures).
pub const CLOSURE: u32 = 4;
/// `TypeTag::Struct` — user struct payload.
pub const STRUCT: u32 = 5;
/// `TypeTag::Enum` — user enum-variant payload (discriminant + payload).
pub const ENUM: u32 = 6;

#[cfg(test)]
mod tests {
    use super::{CLOSURE, ENUM, STRUCT};
    use cranelift_codegen::ir::types::{I32, I64};
    use cranelift_codegen::isa;
    use cranelift_codegen::settings::{self, Configurable};
    use cranelift_module::{Module, default_libcall_names};
    use cranelift_object::{ObjectBuilder, ObjectModule};
    use phoenix_runtime::gc::TypeTag;
    use target_lexicon::Triple;

    /// Drift detector: the hand-mirrored `type_tag::*` constants must
    /// match the live `TypeTag` enum in `phoenix-runtime`. Two layers:
    ///
    /// 1. Equality against the live enum — catches a *swap* (e.g.
    ///    `Closure` and `Struct` trading discriminants while staying in
    ///    range), which the runtime's `debug_assert!` cannot see.
    /// 2. Equality against the absolute byte values — catches a
    ///    *renumber* of the runtime enum that the codegen-side constant
    ///    silently followed (the constants would still equal
    ///    `TypeTag::X as u32`, but every previously-compiled object on
    ///    a mismatched runtime build would read the wrong tag).
    ///
    /// Together these mean either side moving without the other is a
    /// test failure rather than a runtime mistag.
    #[test]
    fn type_tag_matches_runtime() {
        assert_eq!(CLOSURE, TypeTag::Closure as u32);
        assert_eq!(STRUCT, TypeTag::Struct as u32);
        assert_eq!(ENUM, TypeTag::Enum as u32);

        assert_eq!(CLOSURE, 4);
        assert_eq!(STRUCT, 5);
        assert_eq!(ENUM, 6);
    }

    /// Pin the Cranelift-side ABI of `phx_gc_alloc` so a change to the
    /// `RuntimeFunctions::declare` call (or the runtime's Rust signature)
    /// is a unit-test failure rather than a runtime mistag discovered
    /// only when a fixture executes. Mirrors the runtime's
    /// `phx_gc_alloc(size: usize, type_tag: u32) -> *mut u8`.
    #[test]
    fn gc_alloc_signature_matches_runtime_abi() {
        let mut flag_builder = settings::builder();
        flag_builder.set("is_pic", "true").unwrap();
        let isa_builder = isa::lookup(Triple::host()).expect("host triple supported");
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .expect("ISA finishes");
        let call_conv = isa.default_call_conv();
        let builder = ObjectBuilder::new(isa, "drift_test", default_libcall_names())
            .expect("ObjectBuilder constructs");
        let mut module = ObjectModule::new(builder);

        let rt = crate::builtins::RuntimeFunctions::declare(&mut module, call_conv)
            .expect("declare runtime functions");

        let decl = module.declarations().get_function_decl(rt.gc_alloc);
        let sig = &decl.signature;
        let params: Vec<_> = sig.params.iter().map(|p| p.value_type).collect();
        let returns: Vec<_> = sig.returns.iter().map(|r| r.value_type).collect();
        assert_eq!(
            params,
            vec![I64, I32],
            "phx_gc_alloc params drifted from (size: I64, tag: I32)",
        );
        assert_eq!(
            returns,
            vec![I64],
            "phx_gc_alloc return drifted from I64 (pointer)",
        );
    }
}
