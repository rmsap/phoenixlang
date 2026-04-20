//! Module-level compilation context.
//!
//! Wraps a Cranelift [`ObjectModule`] and tracks the mapping between
//! Phoenix function IDs and Cranelift function IDs.

use std::collections::HashMap;

use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{self, isa};
use cranelift_module::{Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use target_lexicon::Triple;

use crate::abi::build_signature;
use crate::builtins::RuntimeFunctions;
use crate::error::CompileError;
use phoenix_ir::instruction::FuncId as PhxFuncId;
use phoenix_ir::module::IrModule;

/// Module-level state for code generation.
pub struct CompileContext {
    /// The Cranelift object module being built.
    pub module: ObjectModule,
    /// Mapping from Phoenix FuncId to Cranelift FuncId.
    pub func_ids: HashMap<PhxFuncId, cranelift_module::FuncId>,
    /// The calling convention for the target.
    pub call_conv: CallConv,
    /// Imported runtime functions.
    pub runtime: RuntimeFunctions,
}

impl CompileContext {
    /// Create a new compile context targeting the host platform.
    pub fn new(ir_module: &IrModule) -> Result<Self, CompileError> {
        // Set up the ISA for the host triple, debug mode (no optimization).
        let mut flag_builder = settings::builder();
        flag_builder
            .set("opt_level", "none")
            .map_err(CompileError::from_display)?;
        flag_builder
            .set("is_pic", "true")
            .map_err(CompileError::from_display)?;

        let isa_builder = isa::lookup(Triple::host())
            .map_err(|e| CompileError::new(format!("unsupported host target: {e}")))?;
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(CompileError::from_display)?;

        let call_conv = isa.default_call_conv();

        let builder = ObjectBuilder::new(
            isa,
            "phoenix_module",
            cranelift_module::default_libcall_names(),
        )
        .map_err(CompileError::from_display)?;

        let mut module = ObjectModule::new(builder);

        // Declare runtime functions.
        let runtime = RuntimeFunctions::declare(&mut module, call_conv)?;

        // Declare all concrete Phoenix functions in the Cranelift module.
        // Generic templates are skipped via `concrete_functions()` — their
        // param/return types contain `IrType::TypeVar` which has no
        // Cranelift representation. The monomorphized specializations are
        // declared alongside.
        //
        // Monomorphization produces symbol-safe names (see
        // `phoenix_ir::monomorphize`'s mangling grammar), so the only
        // mangling needed here is the `.` → `__` step for method names
        // (`TypeName.method` → `TypeName__method`).
        let mut func_ids = HashMap::new();
        for func in ir_module.concrete_functions() {
            let sig = build_signature(&func.param_types, &func.return_type, call_conv);
            let linkage = if func.name == "main" {
                Linkage::Export
            } else {
                Linkage::Local
            };
            let cl_name = format!("phx_{}", func.name.replace('.', "__"));
            let cl_id = module.declare_function(&cl_name, linkage, &sig)?;
            func_ids.insert(func.id, cl_id);
        }

        Ok(Self {
            module,
            func_ids,
            call_conv,
            runtime,
        })
    }
}
