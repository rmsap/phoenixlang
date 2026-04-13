//! Pass 1: Declaration registration.
//!
//! Walks all top-level declarations to register struct/enum layouts,
//! create function stubs, and populate the module's lookup tables.

use crate::instruction::FuncId;
use crate::lower::{LoweringContext, lower_type};
use crate::module::IrFunction;
use crate::types::IrType;
use phoenix_parser::ast::{Declaration, FunctionDecl, Program};

impl<'a> LoweringContext<'a> {
    /// Pass 1: Register all declarations.
    pub(crate) fn register_declarations(&mut self, program: &Program) {
        for decl in &program.declarations {
            match decl {
                Declaration::Struct(s) => self.register_struct(s),
                Declaration::Enum(e) => self.register_enum(e),
                Declaration::Function(f) => {
                    self.register_function(f);
                }
                Declaration::Impl(imp) => {
                    // ImplBlock has `methods` directly (trait_name is optional).
                    for method in &imp.methods {
                        self.register_method(&imp.type_name, method);
                    }
                }
                Declaration::Trait(_)
                | Declaration::TypeAlias(_)
                | Declaration::Endpoint(_)
                | Declaration::Schema(_) => {
                    // Traits and type aliases don't produce IR entities.
                    // Endpoints and schemas are for codegen, not compilation.
                }
            }
        }

        // Register inline methods and trait impls on structs.
        for decl in &program.declarations {
            if let Declaration::Struct(s) = decl {
                for method in &s.methods {
                    self.register_method(&s.name, method);
                }
                for trait_impl in &s.trait_impls {
                    for method in &trait_impl.methods {
                        self.register_method(&s.name, method);
                    }
                }
            }
            if let Declaration::Enum(e) = decl {
                for method in &e.methods {
                    self.register_method(&e.name, method);
                }
                for trait_impl in &e.trait_impls {
                    for method in &trait_impl.methods {
                        self.register_method(&e.name, method);
                    }
                }
            }
        }
    }

    /// Register a struct's layout (field names and types).
    fn register_struct(&mut self, s: &phoenix_parser::ast::StructDecl) {
        if let Some(info) = self.check.structs.get(&s.name) {
            let fields: Vec<(String, IrType)> = info
                .fields
                .iter()
                .map(|f| (f.name.clone(), lower_type(&f.ty, self.check)))
                .collect();
            self.module.struct_layouts.insert(s.name.clone(), fields);
        }
    }

    /// Register an enum's layout (variant names and field types).
    fn register_enum(&mut self, e: &phoenix_parser::ast::EnumDecl) {
        if let Some(info) = self.check.enums.get(&e.name) {
            let check = self.check;
            let variants: Vec<(String, Vec<IrType>)> = info
                .variants
                .iter()
                .map(
                    |(name, fields): &(String, Vec<phoenix_sema::types::Type>)| {
                        let ir_fields: Vec<IrType> =
                            fields.iter().map(|t| lower_type(t, check)).collect();
                        (name.clone(), ir_fields)
                    },
                )
                .collect();
            self.module.enum_layouts.insert(e.name.clone(), variants);
        }
    }

    /// Register a top-level function: create a stub and add it to the index.
    fn register_function(&mut self, f: &FunctionDecl) -> FuncId {
        let func_id = FuncId(self.module.functions.len() as u32);

        let (param_types, param_names) = self.lower_params(f);
        let return_type = self.resolve_return_type(f);

        let func = IrFunction::new(
            func_id,
            f.name.clone(),
            param_types,
            param_names,
            return_type,
            Some(f.span),
        );

        self.module.functions.push(func);
        self.module.function_index.insert(f.name.clone(), func_id);
        func_id
    }

    /// Register a method: create a stub with an explicit `self` parameter
    /// and add it to the method index.
    fn register_method(&mut self, type_name: &str, method: &FunctionDecl) {
        let func_id = FuncId(self.module.functions.len() as u32);
        let mangled_name = format!("{type_name}.{}", method.name);

        // Look up method info from sema for parameter types and return type.
        let method_info = self
            .check
            .methods
            .get(type_name)
            .and_then(|methods| methods.get(&method.name));

        let (mut param_types, mut param_names) = if let Some(info) = method_info {
            // Use sema-resolved parameter types (excludes self).
            let types: Vec<IrType> = info
                .params
                .iter()
                .map(|t| lower_type(t, self.check))
                .collect();
            // Sema MethodInfo doesn't store param names, so get them from AST.
            let names: Vec<String> = method
                .params
                .iter()
                .filter(|p| p.name != "self")
                .map(|p| p.name.clone())
                .collect();
            (types, names)
        } else {
            self.lower_params(method)
        };

        // Check if this method has a `self` parameter by inspecting the AST.
        let has_self = method.params.first().is_some_and(|p| p.name == "self");

        if has_self {
            // Determine the self type from sema info.
            let self_type = if self.check.structs.contains_key(type_name) {
                IrType::StructRef(type_name.to_string())
            } else {
                IrType::EnumRef(type_name.to_string())
            };
            param_types.insert(0, self_type);
            param_names.insert(0, "self".to_string());
        }

        let return_type = if let Some(info) = method_info {
            lower_type(&info.return_type, self.check)
        } else {
            self.resolve_return_type(method)
        };

        let func = IrFunction::new(
            func_id,
            mangled_name.clone(),
            param_types,
            param_names,
            return_type,
            Some(method.span),
        );

        self.module.functions.push(func);
        self.module
            .method_index
            .insert((type_name.to_string(), method.name.clone()), func_id);
    }

    /// Lower a function's parameters to IR types and names.
    /// Excludes `self` parameters (those are handled separately for methods).
    fn lower_params(&self, f: &FunctionDecl) -> (Vec<IrType>, Vec<String>) {
        if let Some(info) = self.check.functions.get(&f.name) {
            let types: Vec<IrType> = info
                .params
                .iter()
                .map(|t| lower_type(t, self.check))
                .collect();
            let names = info.param_names.clone();
            (types, names)
        } else {
            // Fallback: use AST parameter info directly (for methods not
            // in the function registry).
            let types: Vec<IrType> = f
                .params
                .iter()
                .filter(|p| p.name != "self")
                .map(|p| self.resolve_type_expr_fallback(&p.type_annotation))
                .collect();
            let names: Vec<String> = f
                .params
                .iter()
                .filter(|p| p.name != "self")
                .map(|p| p.name.clone())
                .collect();
            (types, names)
        }
    }

    /// Resolve the return type of a function.
    fn resolve_return_type(&self, f: &FunctionDecl) -> IrType {
        if let Some(info) = self.check.functions.get(&f.name) {
            lower_type(&info.return_type, self.check)
        } else {
            IrType::Void
        }
    }

    /// Fallback type resolution from a TypeExpr (when sema info is unavailable).
    fn resolve_type_expr_fallback(&self, _type_expr: &phoenix_parser::ast::TypeExpr) -> IrType {
        // Minimal fallback — in practice, sema should always have the info.
        IrType::Void
    }
}
