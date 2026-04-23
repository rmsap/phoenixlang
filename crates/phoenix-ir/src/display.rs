//! Pretty-printer for the Phoenix IR.
//!
//! Produces a readable textual representation of the IR suitable for
//! snapshot testing and debugging.  The format is loosely inspired by
//! Cranelift's IR format.

use crate::block::BasicBlock;
use crate::instruction::{Instruction, Op, ValueId};
use crate::module::{IrFunction, IrModule};
use crate::terminator::Terminator;
use std::fmt;

impl fmt::Display for IrModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Skip generic template stubs (they contain `IrType::TypeVar` and
        // render as unspecialized signatures that would confuse snapshot
        // readers). All downstream consumers use `concrete_functions()` to
        // iterate post-monomorphization; the textual dump follows suit.
        for (i, func) in self.concrete_functions().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{func}")?;
        }
        Ok(())
    }
}

impl fmt::Display for IrFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Function signature
        write!(f, "func @{}(", self.name)?;
        for (i, (name, ty)) in self.param_names.iter().zip(&self.param_types).enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{name}: {ty}")?;
        }
        writeln!(f, ") -> {} {{", self.return_type)?;

        // Basic blocks
        for block in &self.blocks {
            write!(f, "{block}")?;
        }

        writeln!(f, "}}")
    }
}

impl fmt::Display for BasicBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Block header with parameters
        write!(f, "  {}", self.id)?;
        if !self.params.is_empty() {
            write!(f, "(")?;
            for (i, (val, ty)) in self.params.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{val}: {ty}")?;
            }
            write!(f, ")")?;
        }
        writeln!(f, ":")?;

        // Instructions
        for inst in &self.instructions {
            writeln!(f, "    {inst}")?;
        }

        // Terminator
        writeln!(f, "    {}", self.terminator)?;

        Ok(())
    }
}

impl fmt::Display for Instruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(result) = self.result {
            write!(f, "{result} = ")?;
        }
        write!(f, "{}", self.op)
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Constants
            Op::ConstI64(v) => write!(f, "const_i64 {v}"),
            Op::ConstF64(v) => write!(f, "const_f64 {v}"),
            Op::ConstBool(v) => write!(f, "const_bool {v}"),
            Op::ConstString(s) => write!(f, "const_string {s:?}"),

            // Int arithmetic
            Op::IAdd(a, b) => write!(f, "iadd {a}, {b}"),
            Op::ISub(a, b) => write!(f, "isub {a}, {b}"),
            Op::IMul(a, b) => write!(f, "imul {a}, {b}"),
            Op::IDiv(a, b) => write!(f, "idiv {a}, {b}"),
            Op::IMod(a, b) => write!(f, "imod {a}, {b}"),
            Op::INeg(a) => write!(f, "ineg {a}"),

            // Float arithmetic
            Op::FAdd(a, b) => write!(f, "fadd {a}, {b}"),
            Op::FSub(a, b) => write!(f, "fsub {a}, {b}"),
            Op::FMul(a, b) => write!(f, "fmul {a}, {b}"),
            Op::FDiv(a, b) => write!(f, "fdiv {a}, {b}"),
            Op::FMod(a, b) => write!(f, "fmod {a}, {b}"),
            Op::FNeg(a) => write!(f, "fneg {a}"),

            // Int comparison
            Op::IEq(a, b) => write!(f, "ieq {a}, {b}"),
            Op::INe(a, b) => write!(f, "ine {a}, {b}"),
            Op::ILt(a, b) => write!(f, "ilt {a}, {b}"),
            Op::IGt(a, b) => write!(f, "igt {a}, {b}"),
            Op::ILe(a, b) => write!(f, "ile {a}, {b}"),
            Op::IGe(a, b) => write!(f, "ige {a}, {b}"),

            // Float comparison
            Op::FEq(a, b) => write!(f, "feq {a}, {b}"),
            Op::FNe(a, b) => write!(f, "fne {a}, {b}"),
            Op::FLt(a, b) => write!(f, "flt {a}, {b}"),
            Op::FGt(a, b) => write!(f, "fgt {a}, {b}"),
            Op::FLe(a, b) => write!(f, "fle {a}, {b}"),
            Op::FGe(a, b) => write!(f, "fge {a}, {b}"),

            // String comparison
            Op::StringEq(a, b) => write!(f, "string_eq {a}, {b}"),
            Op::StringNe(a, b) => write!(f, "string_ne {a}, {b}"),
            Op::StringLt(a, b) => write!(f, "string_lt {a}, {b}"),
            Op::StringGt(a, b) => write!(f, "string_gt {a}, {b}"),
            Op::StringLe(a, b) => write!(f, "string_le {a}, {b}"),
            Op::StringGe(a, b) => write!(f, "string_ge {a}, {b}"),

            // Bool comparison
            Op::BoolEq(a, b) => write!(f, "bool_eq {a}, {b}"),
            Op::BoolNe(a, b) => write!(f, "bool_ne {a}, {b}"),

            // Logic
            Op::BoolNot(a) => write!(f, "bool_not {a}"),

            // String ops
            Op::StringConcat(a, b) => write!(f, "string_concat {a}, {b}"),

            // Struct ops
            Op::StructAlloc(name, fields) => {
                write!(f, "struct_alloc @{name}")?;
                write_value_list(f, fields)
            }
            Op::StructGetField(obj, idx) => write!(f, "struct_get_field {obj}, {idx}"),
            Op::StructSetField(obj, idx, val) => {
                write!(f, "struct_set_field {obj}, {idx}, {val}")
            }

            // Enum ops
            Op::EnumAlloc(name, variant, fields) => {
                write!(f, "enum_alloc @{name}:{variant}")?;
                if !fields.is_empty() {
                    write_value_list(f, fields)?;
                }
                Ok(())
            }
            Op::EnumDiscriminant(v) => write!(f, "enum_discriminant {v}"),
            Op::EnumGetField(v, variant, idx) => {
                write!(f, "enum_get_field {v}, variant={variant}, {idx}")
            }

            // Collection ops
            Op::ListAlloc(elems) => {
                write!(f, "list_alloc")?;
                write_value_list(f, elems)
            }
            Op::MapAlloc(pairs) => {
                write!(f, "map_alloc")?;
                if !pairs.is_empty() {
                    write!(f, " [")?;
                    for (i, (k, v)) in pairs.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{k}: {v}")?;
                    }
                    write!(f, "]")?;
                }
                Ok(())
            }

            // Closure ops
            Op::ClosureAlloc(func, captures) => {
                write!(f, "closure_alloc {func}")?;
                if !captures.is_empty() {
                    write_value_list(f, captures)?;
                }
                Ok(())
            }

            // Call ops
            Op::Call(func, type_args, args) => {
                write!(f, "call {func}")?;
                if !type_args.is_empty() {
                    write!(f, "<")?;
                    for (i, t) in type_args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{t}")?;
                    }
                    write!(f, ">")?;
                }
                write_value_list(f, args)
            }
            Op::CallIndirect(callee, args) => {
                write!(f, "call_indirect {callee}")?;
                write_value_list(f, args)
            }
            Op::BuiltinCall(name, args) => {
                write!(f, "builtin_call @{name}")?;
                write_value_list(f, args)
            }
            Op::DynAlloc(trait_name, concrete_type, value) => {
                write!(f, "dyn_alloc @{trait_name} for {concrete_type}, {value}")
            }
            Op::DynCall(trait_name, method_idx, receiver, args) => {
                write!(f, "dyn_call @{trait_name}[{method_idx}], {receiver}")?;
                write_value_list(f, args)
            }

            // Mutable variable ops
            Op::Alloca(ty) => write!(f, "alloca {ty}"),
            Op::Load(slot) => write!(f, "load {slot}"),
            Op::Store(slot, val) => write!(f, "store {slot}, {val}"),

            // Misc
            Op::Copy(v) => write!(f, "copy {v}"),
        }
    }
}

impl fmt::Display for Terminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Terminator::Jump { target, args } => {
                write!(f, "jump {target}")?;
                write_optional_args(f, args)
            }
            Terminator::Branch {
                condition,
                true_block,
                true_args,
                false_block,
                false_args,
            } => {
                write!(f, "branch {condition}, {true_block}")?;
                write_optional_args(f, true_args)?;
                write!(f, ", {false_block}")?;
                write_optional_args(f, false_args)
            }
            Terminator::Switch {
                value,
                cases,
                default,
                default_args,
            } => {
                write!(f, "switch {value}")?;
                for (disc, block, args) in cases {
                    write!(f, ", {disc} => {block}")?;
                    write_optional_args(f, args)?;
                }
                write!(f, ", default => {default}")?;
                write_optional_args(f, default_args)
            }
            Terminator::Return(Some(v)) => write!(f, "return {v}"),
            Terminator::Return(None) => write!(f, "return"),
            Terminator::Unreachable => write!(f, "unreachable"),
            Terminator::None => write!(f, "<no terminator>"),
        }
    }
}

/// Helper to write `(v0, v1, v2)`.
fn write_value_list(f: &mut fmt::Formatter<'_>, values: &[ValueId]) -> fmt::Result {
    write!(f, "(")?;
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{v}")?;
    }
    write!(f, ")")
}

/// Helper to write `(v0, v1)` only when the list is non-empty.
fn write_optional_args(f: &mut fmt::Formatter<'_>, args: &[ValueId]) -> fmt::Result {
    if !args.is_empty() {
        write_value_list(f, args)?;
    }
    Ok(())
}
