//! [`TypeLayout`] — the single source of truth for per-`IrType` layout.
//!
//! Owns slot count, Cranelift-type expansion, and load/store codegen
//! against a base pointer. Every other module in the crate that needs
//! to read or write a Phoenix value routes through here.

use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{InstBuilder, MemFlags, Type as CraneliftType, Value};
use cranelift_frontend::FunctionBuilder;
use phoenix_ir::types::IrType;

use super::SLOT_SIZE;
use crate::types::POINTER_TYPE;

// Static Cranelift-type slices, one per layout shape. All referenced
// constants (`POINTER_TYPE`, `cl::I64`, etc.) are `const`, so these slices
// are `&'static [CraneliftType]` with no allocation.
const CL_EMPTY: &[CraneliftType] = &[];
const CL_I64_SLICE: &[CraneliftType] = &[cl::I64];
const CL_F64_SLICE: &[CraneliftType] = &[cl::F64];
const CL_I8_SLICE: &[CraneliftType] = &[cl::I8];
const CL_STRING: &[CraneliftType] = &[POINTER_TYPE, cl::I64];
const CL_POINTER: &[CraneliftType] = &[POINTER_TYPE];
/// `dyn Trait` ABI — `(data_ptr, vtable_ptr)`.  Both are pointer-sized.
/// See [`IrType::DynRef`](phoenix_ir::types::IrType::DynRef) and the
/// rationale in `docs/design-decisions.md`.
const CL_DYN: &[CraneliftType] = &[POINTER_TYPE, POINTER_TYPE];

/// Layout of a single Phoenix IR type — slot count, Cranelift-type
/// expansion, and load/store codegen.
#[derive(Copy, Clone, Debug)]
pub(crate) struct TypeLayout {
    cl_types: &'static [CraneliftType],
    slots: usize,
}

impl TypeLayout {
    /// Resolve the layout for an IR type. This is the single place where
    /// per-type layout knowledge lives; adding a new `IrType` variant
    /// requires one arm here.
    ///
    /// The match is exhaustive (no `_ =>` wildcard) on purpose: adding a
    /// new `IrType` variant must produce a compile error here so silent
    /// miscompilation of multi-slot types is impossible.
    ///
    /// # Cross-crate invariant — 16-byte fat pointers
    ///
    /// The runtime's `phx_list_contains` / `phx_map_*` use `elem_size == 16`
    /// as a heuristic for "`StringRef` fat pointer — compare by content".
    /// `DynRef` is also 16 bytes and is indistinguishable from a string at
    /// the runtime boundary — today this is fine because `List<dyn Trait>`
    /// is not yet supported end-to-end (see known-issues.md). When it lands,
    /// `phoenix-runtime/src/list_methods.rs` must be taught a different
    /// discriminator (e.g. a per-list element-kind tag) before this layout
    /// acquires a third 16-byte variant.
    #[must_use]
    pub(crate) fn of(ty: &IrType) -> Self {
        match ty {
            IrType::I64 => Self {
                cl_types: CL_I64_SLICE,
                slots: 1,
            },
            IrType::F64 => Self {
                cl_types: CL_F64_SLICE,
                slots: 1,
            },
            IrType::Bool => Self {
                cl_types: CL_I8_SLICE,
                slots: 1,
            },
            // Void occupies no memory and expands to zero Cranelift
            // values; it exists only as a type-system sentinel for "no
            // return value" and must never be stored to memory.
            IrType::Void => Self {
                cl_types: CL_EMPTY,
                slots: 0,
            },
            // Strings are fat pointers: (ptr, len) → 2 slots.
            // See cross-crate invariant note on `TypeLayout::of`.
            IrType::StringRef => Self {
                cl_types: CL_STRING,
                slots: 2,
            },
            // Trait objects are (data_ptr, vtable_ptr) → 2 slots, parallel
            // to StringRef but with a second pointer instead of a length.
            IrType::DynRef(_) => Self {
                cl_types: CL_DYN,
                slots: 2,
            },
            // All other reference types are opaque heap pointers.
            IrType::StructRef(_)
            | IrType::EnumRef(_, _)
            | IrType::ListRef(_)
            | IrType::MapRef(_, _)
            | IrType::ClosureRef { .. } => Self {
                cl_types: CL_POINTER,
                slots: 1,
            },
            // TypeVar should be eliminated by monomorphization before any
            // layout query reaches the backend. Reaching here means a
            // generic template leaked past monomorphization.
            IrType::TypeVar(name) => unreachable!(
                "TypeLayout::of on IrType::TypeVar({name}) — monomorphization \
                 should have eliminated all type variables before codegen"
            ),
        }
    }

    /// Number of 8-byte slots this value occupies in memory.
    #[must_use]
    pub(crate) fn slots(&self) -> usize {
        self.slots
    }

    /// Total size of this value in bytes (`slots * SLOT_SIZE`).
    #[must_use]
    pub(crate) fn size_bytes(&self) -> i64 {
        (self.slots * SLOT_SIZE) as i64
    }

    /// Cranelift-type expansion, one entry per slot in storage order.
    #[must_use]
    pub(crate) fn cl_types(&self) -> &'static [CraneliftType] {
        self.cl_types
    }

    /// Load from heap memory. `slot_offset` is in SLOT_SIZE units.
    ///
    /// Returns one `Value` per Cranelift type in storage order — one for
    /// single-slot types, two for `StringRef` (pointer, then length),
    /// zero for `Void`.
    pub(crate) fn load(
        &self,
        builder: &mut FunctionBuilder,
        base_ptr: Value,
        slot_offset: usize,
    ) -> Vec<Value> {
        self.cl_types
            .iter()
            .enumerate()
            .map(|(i, &cl_ty)| {
                builder.ins().load(
                    cl_ty,
                    MemFlags::new(),
                    base_ptr,
                    ((slot_offset + i) * SLOT_SIZE) as i32,
                )
            })
            .collect()
    }

    /// Store into heap memory. `vals.len()` must equal `cl_types().len()`.
    pub(crate) fn store(
        &self,
        builder: &mut FunctionBuilder,
        base_ptr: Value,
        slot_offset: usize,
        vals: &[Value],
    ) {
        debug_assert_eq!(
            vals.len(),
            self.cl_types.len(),
            "TypeLayout::store: vals.len() ({}) must equal cl_types().len() ({})",
            vals.len(),
            self.cl_types.len()
        );
        for (i, &val) in vals.iter().enumerate() {
            builder.ins().store(
                MemFlags::new(),
                val,
                base_ptr,
                ((slot_offset + i) * SLOT_SIZE) as i32,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::{Function, Signature, UserFuncName};
    use cranelift_codegen::isa::CallConv;
    use cranelift_frontend::FunctionBuilderContext;

    /// Returns a representative `Vec<IrType>` covering every variant,
    /// including a composite `ListRef(StringRef)`. Forces a test failure
    /// when a new `IrType` variant is added without a `TypeLayout::of` arm.
    fn sample_ir_types() -> Vec<IrType> {
        vec![
            IrType::I64,
            IrType::F64,
            IrType::Bool,
            IrType::Void,
            IrType::StringRef,
            IrType::StructRef("X".into()),
            IrType::EnumRef("Y".into(), Vec::new()),
            IrType::EnumRef("Option".into(), vec![IrType::I64]),
            IrType::ListRef(Box::new(IrType::I64)),
            IrType::ListRef(Box::new(IrType::StringRef)),
            IrType::MapRef(Box::new(IrType::I64), Box::new(IrType::I64)),
            IrType::ClosureRef {
                param_types: vec![],
                return_type: Box::new(IrType::Void),
            },
            IrType::DynRef("Test".into()),
        ]
    }

    #[test]
    fn dyn_ref_is_two_slots() {
        let layout = TypeLayout::of(&IrType::DynRef("Test".into()));
        assert_eq!(layout.slots(), 2);
        assert_eq!(layout.cl_types(), &[POINTER_TYPE, POINTER_TYPE]);
        assert_eq!(layout.size_bytes(), 16);
    }

    #[test]
    fn slots_matches_cl_types_len() {
        for ty in sample_ir_types() {
            let layout = TypeLayout::of(&ty);
            assert_eq!(
                layout.slots(),
                layout.cl_types().len(),
                "slots() must match cl_types().len() for {ty:?}"
            );
        }
    }

    #[test]
    fn size_bytes_matches_slots_times_slot_size() {
        for ty in sample_ir_types() {
            let layout = TypeLayout::of(&ty);
            assert_eq!(
                layout.size_bytes(),
                (layout.slots() * SLOT_SIZE) as i64,
                "size_bytes() must equal slots() * SLOT_SIZE for {ty:?}"
            );
        }
    }

    #[test]
    fn string_ref_is_two_slots() {
        let layout = TypeLayout::of(&IrType::StringRef);
        assert_eq!(layout.slots(), 2);
        assert_eq!(layout.cl_types(), &[POINTER_TYPE, cl::I64]);
        assert_eq!(layout.size_bytes(), 16);
    }

    /// Pin Void's layout: zero slots, zero cl_types, zero size. This lets
    /// all the `slots() == cl_types().len()` invariants hold uniformly.
    #[test]
    fn void_is_zero_slots() {
        let layout = TypeLayout::of(&IrType::Void);
        assert_eq!(layout.slots(), 0);
        assert_eq!(layout.cl_types(), CL_EMPTY);
        assert_eq!(layout.size_bytes(), 0);
    }

    #[test]
    fn all_other_ref_types_are_one_slot() {
        let one_slot_refs = [
            IrType::StructRef("X".into()),
            IrType::EnumRef("Y".into(), Vec::new()),
            IrType::ListRef(Box::new(IrType::I64)),
            IrType::MapRef(Box::new(IrType::I64), Box::new(IrType::I64)),
            IrType::ClosureRef {
                param_types: vec![],
                return_type: Box::new(IrType::Void),
            },
        ];
        for ty in one_slot_refs {
            let layout = TypeLayout::of(&ty);
            assert_eq!(layout.slots(), 1, "for {ty:?}");
            assert_eq!(layout.cl_types(), &[POINTER_TYPE], "for {ty:?}");
        }
    }

    /// Create a throwaway function-builder context for testing load/store
    /// emission in isolation.
    fn test_builder_ctx() -> (Function, FunctionBuilderContext) {
        let sig = Signature::new(CallConv::SystemV);
        let func = Function::with_name_signature(UserFuncName::default(), sig);
        (func, FunctionBuilderContext::new())
    }

    /// Count occurrences of `needle` in the function's IR display.
    fn count_opcode(func: &Function, needle: &str) -> usize {
        func.display().to_string().matches(needle).count()
    }

    #[test]
    fn store_emits_two_stores_for_string_ref() {
        let (mut func, mut fb_ctx) = test_builder_ctx();
        {
            let mut builder = FunctionBuilder::new(&mut func, &mut fb_ctx);
            let block = builder.create_block();
            builder.switch_to_block(block);
            builder.seal_block(block);
            let base = builder.ins().iconst(cl::I64, 0);
            let v0 = builder.ins().iconst(cl::I64, 1);
            let v1 = builder.ins().iconst(cl::I64, 2);
            TypeLayout::of(&IrType::StringRef).store(&mut builder, base, 3, &[v0, v1]);
            builder.ins().return_(&[]);
            builder.finalize();
        }
        assert_eq!(count_opcode(&func, "store"), 2);
        let text = func.display().to_string();
        assert!(text.contains("+24"), "expected store at +24; got:\n{text}");
        assert!(text.contains("+32"), "expected store at +32; got:\n{text}");
    }

    #[test]
    fn load_emits_two_loads_for_string_ref() {
        let (mut func, mut fb_ctx) = test_builder_ctx();
        {
            let mut builder = FunctionBuilder::new(&mut func, &mut fb_ctx);
            let block = builder.create_block();
            builder.switch_to_block(block);
            builder.seal_block(block);
            let base = builder.ins().iconst(cl::I64, 0);
            let vals = TypeLayout::of(&IrType::StringRef).load(&mut builder, base, 2);
            assert_eq!(vals.len(), 2);
            builder.ins().return_(&[]);
            builder.finalize();
        }
        assert_eq!(count_opcode(&func, "load"), 2);
        let text = func.display().to_string();
        assert!(text.contains("+16"), "expected load at +16; got:\n{text}");
        assert!(text.contains("+24"), "expected load at +24; got:\n{text}");
    }

    #[test]
    fn store_emits_single_store_for_i64() {
        let (mut func, mut fb_ctx) = test_builder_ctx();
        {
            let mut builder = FunctionBuilder::new(&mut func, &mut fb_ctx);
            let block = builder.create_block();
            builder.switch_to_block(block);
            builder.seal_block(block);
            let base = builder.ins().iconst(cl::I64, 0);
            let v = builder.ins().iconst(cl::I64, 42);
            TypeLayout::of(&IrType::I64).store(&mut builder, base, 1, &[v]);
            builder.ins().return_(&[]);
            builder.finalize();
        }
        assert_eq!(count_opcode(&func, "store"), 1);
    }

    #[test]
    fn void_store_is_a_no_op() {
        let (mut func, mut fb_ctx) = test_builder_ctx();
        {
            let mut builder = FunctionBuilder::new(&mut func, &mut fb_ctx);
            let block = builder.create_block();
            builder.switch_to_block(block);
            builder.seal_block(block);
            let base = builder.ins().iconst(cl::I64, 0);
            TypeLayout::of(&IrType::Void).store(&mut builder, base, 0, &[]);
            builder.ins().return_(&[]);
            builder.finalize();
        }
        assert_eq!(count_opcode(&func, "store"), 0);
    }

    #[test]
    #[should_panic(expected = "TypeLayout::store")]
    fn store_panics_on_length_mismatch() {
        let (mut func, mut fb_ctx) = test_builder_ctx();
        let mut builder = FunctionBuilder::new(&mut func, &mut fb_ctx);
        let block = builder.create_block();
        builder.switch_to_block(block);
        builder.seal_block(block);
        let base = builder.ins().iconst(cl::I64, 0);
        let v = builder.ins().iconst(cl::I64, 1);
        // StringRef wants 2 values; pass 1 → debug_assert_eq! trips.
        TypeLayout::of(&IrType::StringRef).store(&mut builder, base, 0, &[v]);
    }
}
