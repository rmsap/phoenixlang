//! Pre-scan that decides which synthesized `phx_print_*` helpers a
//! given IR module actually needs.
//!
//! Running this once up-front lets [`ModuleBuilder`](super::ModuleBuilder)
//! skip declaring (and emitting bodies for) helpers — and the data
//! segments they own, such as the `"true\n"` / `"false\n"` bool
//! literals — that no `print` call in the module would reach. The
//! result is a smaller emitted `.wasm` and, more importantly, a
//! data-section layout that PR 3's `Op::ConstString` can rely on:
//! string constants always start at a known offset (0 when no bools
//! are printed, or just past the bool literals when they are).

use std::collections::HashMap;

use phoenix_ir::instruction::{Op, ValueId};
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

#[derive(Default, Copy, Clone, Debug)]
pub(super) struct HelperUsage {
    /// Any `print(int)` call appeared in the module.
    pub(super) print_i64: bool,
    /// Any `print(bool)` call appeared in the module.
    pub(super) print_bool: bool,
}

impl HelperUsage {
    /// `phx_print_str` is the leaf helper both `phx_print_i64` and
    /// `phx_print_bool` bottom out in, so it's needed whenever either
    /// caller is. PR 2 has no user-facing `print(string)`; PR 3 will
    /// make `print_str` independently reachable.
    pub(super) fn print_str(self) -> bool {
        self.print_i64 || self.print_bool
    }

    /// Walk every concrete function in `m` looking for
    /// `BuiltinCall("print", [arg])` and classify the argument
    /// type. Single-pass: Phoenix IR is SSA, so by the time the
    /// verifier accepts a function every `ValueId` use has a
    /// preceding definition in linearized block-then-instruction
    /// order. We record types as we see definitions (block params
    /// and instruction results) and look them up when we hit a
    /// matching `BuiltinCall`.
    pub(super) fn scan(m: &IrModule) -> Self {
        let mut usage = HelperUsage::default();
        for func in m.concrete_functions() {
            let mut value_type: HashMap<ValueId, IrType> = HashMap::new();
            for block in &func.blocks {
                for (vid, ty) in &block.params {
                    value_type.insert(*vid, ty.clone());
                }
                for instr in &block.instructions {
                    if let Some(vid) = instr.result {
                        value_type.insert(vid, instr.result_type.clone());
                    }
                    let Op::BuiltinCall(name, args) = &instr.op else {
                        continue;
                    };
                    if name != "print" {
                        continue;
                    }
                    let Some(arg) = args.first() else {
                        continue;
                    };
                    let arg_ty = value_type.get(arg);
                    // The IR is SSA and we walk defs-before-uses within
                    // each block, so the print argument's type *should*
                    // always be in `value_type` by the time we reach
                    // this BuiltinCall. A `None` here means either the
                    // verifier let a use-before-def slip through, or
                    // this scan grew a multi-block walk that doesn't
                    // respect defs-dominate-uses ordering. Catch it in
                    // debug builds so the regression points at the
                    // scan rather than at translation's "pre-scan vs.
                    // translation disagree" error downstream.
                    debug_assert!(
                        arg_ty.is_some(),
                        "wasm32-linear helper-usage scan: `print` arg \
                         {arg:?} has no recorded type by the time the \
                         BuiltinCall is visited",
                    );
                    match arg_ty {
                        Some(IrType::I64) => usage.print_i64 = true,
                        Some(IrType::Bool) => usage.print_bool = true,
                        _ => {}
                    }
                }
            }
        }
        usage
    }
}
