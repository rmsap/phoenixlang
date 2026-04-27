use super::*;
use crate::block::{BasicBlock, BlockId};
use crate::instruction::{Instruction, ValueId};
use crate::terminator::Terminator;

// ── Test builders ──────────────────────────────────────────────
//
// These builders exist so each test shows its *intent* — a function
// with these type params, these params, this return type, these
// instructions — not seven lines of struct construction per call.

/// Shorthand for `IrType::TypeVar(name.into())`.
fn tv(name: &str) -> IrType {
    IrType::TypeVar(name.into())
}

/// A generic `Op::Call` with no value arguments (sufficient for
/// monomorphization tests, which only care about the callee and
/// embedded type_args).
fn gcall(callee: u32, type_args: Vec<IrType>) -> Op {
    Op::Call(FuncId(callee), type_args, vec![])
}

/// Fluent builder for an `IrFunction` with a single entry block.
/// Non-empty `type_params` flip the slot variant to
/// [`crate::module::FunctionSlot::Template`] automatically.
struct FnBuilder {
    func: IrFunction,
    instrs: Vec<Instruction>,
    is_template: bool,
}

impl FnBuilder {
    fn new(id: u32, name: &str) -> Self {
        Self {
            func: IrFunction::new(
                FuncId(id),
                name.to_string(),
                Vec::new(),
                Vec::new(),
                IrType::Void,
                None,
            ),
            instrs: Vec::new(),
            is_template: false,
        }
    }

    fn generic(mut self, names: &[&str]) -> Self {
        self.func.type_param_names = names.iter().map(|s| (*s).to_string()).collect();
        self.is_template = !names.is_empty();
        self
    }

    fn params(mut self, types: Vec<IrType>) -> Self {
        self.func.param_types = types;
        self
    }

    fn ret(mut self, ty: IrType) -> Self {
        self.func.return_type = ty;
        self
    }

    fn instr(mut self, op: Op, result_type: IrType) -> Self {
        self.instrs.push(Instruction {
            result: Some(ValueId(0)),
            op,
            result_type,
            span: None,
        });
        self
    }

    fn build(mut self) -> crate::module::FunctionSlot {
        self.func.blocks.push(BasicBlock {
            id: BlockId(0),
            params: vec![],
            instructions: self.instrs,
            terminator: Terminator::Return(None),
        });
        if self.is_template {
            crate::module::FunctionSlot::Template(self.func)
        } else {
            crate::module::FunctionSlot::Concrete(self.func)
        }
    }
}

/// Build a module from a list of function slots, registering each in
/// `function_index` automatically.
fn module_of(slots: Vec<crate::module::FunctionSlot>) -> IrModule {
    let mut m = IrModule::new();
    for s in slots {
        let f = s.func();
        m.function_index.insert(f.name.clone(), f.id);
        m.functions.push(s);
    }
    m
}

/// Look up a function by name and return a reference.
fn lookup<'a>(m: &'a IrModule, name: &str) -> &'a IrFunction {
    m.functions[m.function_index[name].index()].func()
}

/// `true` iff the slot at `name` is a template.
fn is_template(m: &IrModule, name: &str) -> bool {
    m.functions[m.function_index[name].index()].is_template()
}

/// Destructure the `Op::Call` at `(block, instr)` within `func`,
/// asserting it is in fact a direct call.
fn call_at(func: &IrFunction, block: usize, instr: usize) -> (FuncId, &[IrType]) {
    match &func.blocks[block].instructions[instr].op {
        Op::Call(callee, targs, _) => (*callee, targs.as_slice()),
        other => panic!("expected Op::Call at [{block}][{instr}], got {other:?}"),
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[test]
fn specializes_identity_at_int_and_string() {
    // identity<T>(x: T) -> T, called at Int and String from main.
    let mut module = module_of(vec![
        FnBuilder::new(0, "identity")
            .generic(&["T"])
            .params(vec![tv("T")])
            .ret(tv("T"))
            .build(),
        FnBuilder::new(1, "main")
            .instr(gcall(0, vec![IrType::I64]), IrType::I64)
            .instr(gcall(0, vec![IrType::StringRef]), IrType::StringRef)
            .build(),
    ]);

    monomorphize(&mut module);

    let int_spec = lookup(&module, "identity__i64");
    assert!(!is_template(&module, "identity__i64"));
    assert_eq!(int_spec.param_types, vec![IrType::I64]);
    assert_eq!(int_spec.return_type, IrType::I64);

    let str_spec = lookup(&module, "identity__str");
    assert_eq!(str_spec.param_types, vec![IrType::StringRef]);

    // Template preserved as inert stub at FuncId(0).
    assert!(module.functions[0].is_template());

    // Call sites rewritten: targets point at specializations,
    // type_args are cleared.
    let main = lookup(&module, "main");
    for (i, expected_name) in ["identity__i64", "identity__str"].iter().enumerate() {
        let (callee, targs) = call_at(main, 0, i);
        assert_eq!(callee, module.function_index[*expected_name]);
        assert!(targs.is_empty(), "specialized call kept residual type_args");
    }
}

#[test]
fn specializes_multi_param_function() {
    // first<A, B>(a: A, b: B) -> A
    let mut module = module_of(vec![
        FnBuilder::new(0, "first")
            .generic(&["A", "B"])
            .params(vec![tv("A"), tv("B")])
            .ret(tv("A"))
            .build(),
        FnBuilder::new(1, "main")
            .instr(gcall(0, vec![IrType::I64, IrType::StringRef]), IrType::I64)
            .build(),
    ]);

    monomorphize(&mut module);

    let spec = lookup(&module, "first__i64__str");
    assert_eq!(spec.param_types, vec![IrType::I64, IrType::StringRef]);
    assert_eq!(spec.return_type, IrType::I64);
}

#[test]
fn recursion_through_generics_preserves_specialization() {
    // count<T>(x: T) -> Void { count(x) }  (self-call must stay specialized)
    let mut module = module_of(vec![
        FnBuilder::new(0, "count")
            .generic(&["T"])
            .params(vec![tv("T")])
            .instr(gcall(0, vec![tv("T")]), IrType::Void)
            .build(),
        FnBuilder::new(1, "main")
            .instr(gcall(0, vec![IrType::I64]), IrType::Void)
            .build(),
    ]);

    monomorphize(&mut module);

    let count_int_id = module.function_index["count__i64"];
    let (inner_callee, inner_targs) = call_at(lookup(&module, "count__i64"), 0, 0);
    assert_eq!(inner_callee, count_int_id);
    assert!(inner_targs.is_empty());
}

#[test]
fn uninstantiated_template_leaves_module_unchanged_up_to_erasure() {
    let mut module = module_of(vec![
        FnBuilder::new(0, "unused")
            .generic(&["T"])
            .params(vec![tv("T")])
            .ret(tv("T"))
            .build(),
    ]);

    monomorphize(&mut module);

    assert_eq!(module.functions.len(), 1);
    assert!(module.functions[0].is_template());
    assert!(module.concrete_functions().next().is_none());
}

#[test]
fn mangling_is_symbol_safe_for_reference_types() {
    // Mangled names must match [A-Za-z0-9_]: no angle brackets,
    // commas, parens, spaces, or arrows — even when the type arg is
    // a compound reference type like List / Map / closure.
    let cases: Vec<IrType> = vec![
        IrType::ListRef(Box::new(IrType::I64)),
        IrType::MapRef(Box::new(IrType::StringRef), Box::new(IrType::I64)),
        IrType::ClosureRef {
            param_types: vec![IrType::I64, IrType::Bool],
            return_type: Box::new(IrType::StringRef),
        },
        IrType::StructRef("Point".into(), Vec::new()),
        IrType::StructRef("Container".into(), vec![IrType::I64]),
        IrType::EnumRef("Option".into(), Vec::new()),
        IrType::EnumRef("Option".into(), vec![IrType::I64]),
        IrType::EnumRef("Result".into(), vec![IrType::StringRef, IrType::I64]),
    ];
    for ty in cases {
        let name = mangle("fn", std::slice::from_ref(&ty));
        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "mangled name `{name}` (from {ty:?}) contains non-symbol-safe chars"
        );
    }
}

/// Lock in the exact mangle grammar for `EnumRef` with args. Injectivity
/// is covered separately; this test guards against silent reformatting
/// (e.g. changing the `_E` terminator or the per-arg separator) that
/// would change the Cranelift symbol names Phoenix binaries are linked
/// against.
#[test]
fn mangles_enum_ref_with_args_verbatim() {
    assert_eq!(
        mangle_type(&IrType::EnumRef("Option".into(), Vec::new())),
        "e_Option"
    );
    assert_eq!(
        mangle_type(&IrType::EnumRef("Option".into(), vec![IrType::I64])),
        "e_Option__i64_E"
    );
    assert_eq!(
        mangle_type(&IrType::EnumRef(
            "Result".into(),
            vec![IrType::StringRef, IrType::I64]
        )),
        "e_Result__str__i64_E"
    );
    // Nesting is unambiguous thanks to the `_E` terminator.
    assert_eq!(
        mangle_type(&IrType::ListRef(Box::new(IrType::EnumRef(
            "Option".into(),
            vec![IrType::I64]
        )))),
        "L_e_Option__i64_E_E"
    );
}

/// Regression guard for the name/arg delimiter ambiguity: with a single-
/// underscore separator, `EnumRef("Opt", [StructRef("foo_i64")])` and
/// `EnumRef("Opt", [StructRef("foo"), I64])` would both mangle to
/// `e_Opt_s_foo_i64_E`. The `__` delimiter splits these cleanly because
/// Phoenix identifiers forbid `__`, so the boundary between name and
/// first arg (and between adjacent args) is unambiguous.
#[test]
fn enum_ref_mangle_is_injective_under_underscore_in_arg_names() {
    let a = IrType::EnumRef(
        "Opt".into(),
        vec![IrType::StructRef("foo_i64".into(), Vec::new())],
    );
    let b = IrType::EnumRef(
        "Opt".into(),
        vec![IrType::StructRef("foo".into(), Vec::new()), IrType::I64],
    );
    assert_ne!(mangle_type(&a), mangle_type(&b));
    assert_eq!(mangle_type(&a), "e_Opt__s_foo_i64_E");
    assert_eq!(mangle_type(&b), "e_Opt__s_foo__i64_E");
}

#[test]
fn specializes_at_reference_type_list_of_int() {
    let list_i64 = IrType::ListRef(Box::new(IrType::I64));
    let mut module = module_of(vec![
        FnBuilder::new(0, "wrap")
            .generic(&["T"])
            .params(vec![tv("T")])
            .ret(tv("T"))
            .build(),
        FnBuilder::new(1, "main")
            .instr(gcall(0, vec![list_i64.clone()]), list_i64.clone())
            .build(),
    ]);

    monomorphize(&mut module);

    let spec = lookup(&module, "wrap__L_i64_E");
    assert_eq!(spec.param_types, vec![list_i64.clone()]);
    assert_eq!(spec.return_type, list_i64);
}

#[test]
fn empty_template_body_does_not_panic() {
    // Pass A must handle zero-instruction blocks without panicking.
    let mut module = module_of(vec![
        FnBuilder::new(0, "noop")
            .generic(&["T"])
            .params(vec![tv("T")])
            .build(),
        FnBuilder::new(1, "main")
            .instr(gcall(0, vec![IrType::I64]), IrType::Void)
            .build(),
    ]);

    monomorphize(&mut module);
    assert!(module.function_index.contains_key("noop__i64"));
}

/// `substitute` must recurse into `EnumRef.args` so a `TypeVar` inside
/// an `Option<T>` or `Result<T, E>` position is replaced when the
/// template is specialized. Without this the backend would see an
/// unsubstituted `TypeVar` at a reference-type use site, which has no
/// Cranelift lowering.
#[test]
fn substitute_recurses_into_enum_ref_args() {
    let mut subst = HashMap::new();
    subst.insert("T".to_string(), IrType::I64);
    subst.insert("E".to_string(), IrType::StringRef);

    let ty = IrType::EnumRef("Result".into(), vec![tv("T"), tv("E")]);
    assert_eq!(
        substitute(&ty, &subst),
        IrType::EnumRef("Result".into(), vec![IrType::I64, IrType::StringRef])
    );

    // Nested: Option<List<T>>
    let nested = IrType::EnumRef("Option".into(), vec![IrType::ListRef(Box::new(tv("T")))]);
    assert_eq!(
        substitute(&nested, &subst),
        IrType::EnumRef(
            "Option".into(),
            vec![IrType::ListRef(Box::new(IrType::I64))]
        )
    );

    // Empty-args EnumRef is untouched (no TypeVars to substitute).
    let bare = IrType::EnumRef("Color".into(), Vec::new());
    assert_eq!(substitute(&bare, &subst), bare);
}

/// `contains_type_var` must recurse into every compound type constructor
/// — a missing arm would let an orphan `TypeVar` slip past the Pass A
/// `debug_assert` guard. Regression test for a prior miss on `EnumRef`.
#[test]
fn contains_type_var_recurses_into_every_compound() {
    assert!(contains_type_var(&tv("T")));
    assert!(contains_type_var(&IrType::ListRef(Box::new(tv("T")))));
    assert!(contains_type_var(&IrType::MapRef(
        Box::new(IrType::I64),
        Box::new(tv("V"))
    )));
    assert!(contains_type_var(&IrType::ClosureRef {
        param_types: vec![IrType::I64, tv("P")],
        return_type: Box::new(IrType::Void),
    }));
    assert!(contains_type_var(&IrType::EnumRef(
        "Option".into(),
        vec![tv("T")]
    )));
    // Deeply nested: Option<List<T>>.
    assert!(contains_type_var(&IrType::EnumRef(
        "Option".into(),
        vec![IrType::ListRef(Box::new(tv("T")))]
    )));

    // Atomic and concrete compound types report false.
    assert!(!contains_type_var(&IrType::I64));
    assert!(!contains_type_var(&IrType::StructRef(
        "Point".into(),
        Vec::new()
    )));
    assert!(!contains_type_var(&IrType::EnumRef(
        "Result".into(),
        vec![IrType::I64, IrType::StringRef]
    )));
    assert!(!contains_type_var(&IrType::EnumRef("Color".into(), vec![])));
}

#[test]
fn residual_type_var_erased_to_placeholder_when_no_specializations() {
    // Non-template function with an orphan TypeVar (e.g., empty list
    // literal with unresolved element type) has it erased to
    // GENERIC_PLACEHOLDER even when no monomorphization is needed.
    let mut module = module_of(vec![
        FnBuilder::new(0, "main")
            .instr(Op::ListAlloc(vec![]), IrType::ListRef(Box::new(tv("U"))))
            .build(),
    ]);

    monomorphize(&mut module);

    let instr = &module.functions[0].func().blocks[0].instructions[0];
    assert_eq!(
        instr.result_type,
        IrType::ListRef(Box::new(IrType::StructRef(
            crate::types::GENERIC_PLACEHOLDER.to_string(),
            Vec::new()
        )))
    );
}

/// An orphan `TypeVar` inside an `EnumRef` arg (e.g. `Option<T>` where
/// `T` was never bound) must be erased to `GENERIC_PLACEHOLDER` by
/// Pass D, matching the treatment of orphan TypeVars in `ListRef` /
/// `MapRef` positions. Without this, the backend would hit an
/// unsubstituted `TypeVar` at a reference-type use site.
#[test]
fn residual_type_var_in_enum_ref_args_erased_by_pass_d() {
    let mut module = module_of(vec![
        FnBuilder::new(0, "main")
            .instr(
                Op::ListAlloc(vec![]),
                IrType::EnumRef("Option".into(), vec![tv("T")]),
            )
            .build(),
    ]);

    monomorphize(&mut module);

    let instr = &module.functions[0].func().blocks[0].instructions[0];
    assert_eq!(
        instr.result_type,
        IrType::EnumRef(
            "Option".into(),
            vec![IrType::StructRef(
                crate::types::GENERIC_PLACEHOLDER.to_string(),
                Vec::new()
            )]
        )
    );
}

// ── Struct-monomorphization unit tests ─────────────────────────
//
// These tests target `monomorphize_structs` directly. They build a
// minimal `IrModule` with a generic struct registered in
// `struct_type_params` / `struct_layouts`, optionally register a
// method in `method_index`, and a caller function whose types
// reference the generic struct. After running `monomorphize`, they
// assert the expected post-pass state: specialized layout registered
// under the mangled name, method_index rewired, StructAlloc ops
// repointed, StructRef args erased, and (for the dyn case) the
// vtable rekeyed.

/// Helper: register a generic struct with the given field layout.
fn register_generic_struct(
    module: &mut IrModule,
    name: &str,
    type_params: &[&str],
    fields: Vec<(&str, IrType)>,
) {
    module.struct_type_params.insert(
        name.to_string(),
        type_params.iter().map(|s| s.to_string()).collect(),
    );
    module.struct_layouts.insert(
        name.to_string(),
        fields
            .into_iter()
            .map(|(n, t)| (n.to_string(), t))
            .collect(),
    );
}

/// Baseline: a `Container<T>` with a single field, used as
/// `Container<Int>` from `main`. After struct-mono, the layout
/// registered under `"Container__i64"` has the substituted field
/// type, and `main`'s StructRef / StructAlloc point at the mangled
/// name.
#[test]
fn struct_mono_registers_specialized_layout_and_rewrites_alloc() {
    let mut module = module_of(vec![
        FnBuilder::new(0, "main")
            .instr(
                Op::StructAlloc("Container".into(), vec![]),
                IrType::StructRef("Container".into(), vec![IrType::I64]),
            )
            .build(),
    ]);
    register_generic_struct(&mut module, "Container", &["T"], vec![("value", tv("T"))]);

    monomorphize(&mut module);

    // Specialized layout registered with the substituted field type.
    let specialized = module
        .struct_layouts
        .get("Container__i64")
        .expect("specialized layout missing");
    assert_eq!(specialized, &vec![("value".to_string(), IrType::I64)]);

    // StructAlloc name rewritten and result_type's args cleared.
    let instr = &module.functions[0].func().blocks[0].instructions[0];
    match &instr.op {
        Op::StructAlloc(name, _) => assert_eq!(name, "Container__i64"),
        other => panic!("expected StructAlloc, got {other:?}"),
    }
    assert_eq!(
        instr.result_type,
        IrType::StructRef("Container__i64".into(), vec![])
    );
}

/// Two distinct instantiations of the same generic struct must
/// produce two distinct specialized layouts and not clobber each
/// other's method_index entries.
#[test]
fn struct_mono_two_instantiations_dont_collide() {
    let mut module = module_of(vec![
        FnBuilder::new(0, "main")
            .instr(
                Op::StructAlloc("Box".into(), vec![]),
                IrType::StructRef("Box".into(), vec![IrType::I64]),
            )
            .instr(
                Op::StructAlloc("Box".into(), vec![]),
                IrType::StructRef("Box".into(), vec![IrType::StringRef]),
            )
            .build(),
    ]);
    register_generic_struct(&mut module, "Box", &["T"], vec![("v", tv("T"))]);

    monomorphize(&mut module);

    assert_eq!(
        module.struct_layouts.get("Box__i64"),
        Some(&vec![("v".to_string(), IrType::I64)])
    );
    assert_eq!(
        module.struct_layouts.get("Box__str"),
        Some(&vec![("v".to_string(), IrType::StringRef)])
    );
}

/// A method on a generic struct is cloned per-instantiation with
/// type-var substitution applied to its body, and `method_index` is
/// updated to point at the specialized `FuncId`.
#[test]
fn struct_mono_specializes_methods_and_updates_method_index() {
    // Template method `Container.get(self) -> T { self.value }`,
    // plus `main` constructing `Container<Int>`. The method is a
    // template because its parent struct is generic, even though it
    // has no method-level type params.
    let template_method_fn = FnBuilder::new(0, "Container.get")
        .params(vec![IrType::StructRef("Container".into(), vec![tv("T")])])
        .ret(tv("T"))
        .instr(Op::StructGetField(ValueId(0), 0), tv("T"))
        .build()
        .func()
        .clone();
    let template_method = crate::module::FunctionSlot::Template(template_method_fn);

    let mut module = module_of(vec![
        template_method,
        FnBuilder::new(1, "main")
            .instr(
                Op::StructAlloc("Container".into(), vec![]),
                IrType::StructRef("Container".into(), vec![IrType::I64]),
            )
            .build(),
    ]);
    register_generic_struct(&mut module, "Container", &["T"], vec![("value", tv("T"))]);
    module
        .method_index
        .insert(("Container".into(), "get".into()), FuncId(0));

    monomorphize(&mut module);

    // Specialized method registered under the mangled struct name.
    let spec_fid = module
        .method_index
        .get(&("Container__i64".to_string(), "get".to_string()))
        .copied()
        .expect("specialized method_index entry missing");
    let spec = module.functions[spec_fid.index()].func();
    assert!(!module.functions[spec_fid.index()].is_template());
    assert_eq!(spec.name, "Container__i64.get");
    // self param's StructRef args rewritten to empty (mangled form).
    assert_eq!(
        spec.param_types,
        vec![IrType::StructRef("Container__i64".into(), vec![])]
    );
    // Return type substituted T → Int.
    assert_eq!(spec.return_type, IrType::I64);
    // Field-access result_type substituted T → Int.
    let instr = &spec.blocks[0].instructions[0];
    assert_eq!(instr.result_type, IrType::I64);

    // Original template preserved as inert stub.
    assert!(module.functions[0].is_template());
}

/// `Op::DynAlloc` for a generic struct receiver is rewritten to the
/// mangled concrete name, and the `dyn_vtables` entry is rekeyed
/// from `("Box", "Show")` to `("Box__i64", "Show")` with method
/// FuncIds re-resolved through the specialized `method_index`.
#[test]
fn struct_mono_rekeys_dyn_vtables_for_generic_struct() {
    // Generic struct `Box<T>` with method `show(self) -> String`,
    // impl Show for Box, and a `main` that coerces `Box<Int>` to
    // `dyn Show`.
    // Bare-name template method on a generic struct — promoted to a
    // Template slot below.
    let template_show_fn = FnBuilder::new(0, "Box.show")
        .params(vec![IrType::StructRef("Box".into(), vec![tv("T")])])
        .ret(IrType::StringRef)
        .build()
        .func()
        .clone();
    let template_show = crate::module::FunctionSlot::Template(template_show_fn);

    // Build `main` via the canonical API so the value allocator stays in sync.
    let mut main = IrFunction::new(
        FuncId(1),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = main.create_block();
    let box_val = main.emit_value(
        entry,
        Op::StructAlloc("Box".into(), vec![]),
        IrType::StructRef("Box".into(), vec![IrType::I64]),
        None,
    );
    main.emit_value(
        entry,
        Op::DynAlloc("Show".into(), "Box".into(), box_val),
        IrType::DynRef("Show".into()),
        None,
    );

    let mut module = module_of(vec![
        template_show,
        crate::module::FunctionSlot::Concrete(main),
    ]);
    register_generic_struct(&mut module, "Box", &["T"], vec![("v", tv("T"))]);
    module
        .method_index
        .insert(("Box".into(), "show".into()), FuncId(0));
    module.dyn_vtables.insert(
        ("Box".into(), "Show".into()),
        vec![("show".into(), FuncId(0))],
    );

    monomorphize(&mut module);

    // Template vtable entry gone.
    assert!(
        !module
            .dyn_vtables
            .contains_key(&("Box".into(), "Show".into())),
        "template vtable entry should be dropped"
    );
    // Mangled vtable entry registered.
    let specialized_show_fid = module
        .method_index
        .get(&("Box__i64".to_string(), "show".to_string()))
        .copied()
        .expect("specialized method_index entry missing");
    let vt = module
        .dyn_vtables
        .get(&("Box__i64".to_string(), "Show".to_string()))
        .expect("rekeyed vtable entry missing");
    assert_eq!(vt, &vec![("show".to_string(), specialized_show_fid)]);

    // DynAlloc op's concrete name rewritten.
    let main_fid = module.function_index["main"];
    let dyn_instr = &module.functions[main_fid.index()].func().blocks[0].instructions[1];
    match &dyn_instr.op {
        Op::DynAlloc(trait_name, concrete, _) => {
            assert_eq!(trait_name, "Show");
            assert_eq!(concrete, "Box__i64");
        }
        other => panic!("expected DynAlloc, got {other:?}"),
    }
}

/// Nested generic struct (`Pair<T>` inside `Nested<T>`) converges
/// via the fixed-point worklist: specializing `Nested<Int>` must
/// enqueue `Pair<Int>`, and both layouts must be registered under
/// their respective mangled names.
#[test]
fn struct_mono_fixed_point_handles_nested_generics() {
    let mut module = module_of(vec![
        FnBuilder::new(0, "main")
            .instr(
                Op::StructAlloc("Nested".into(), vec![]),
                IrType::StructRef("Nested".into(), vec![IrType::I64]),
            )
            .build(),
    ]);
    register_generic_struct(
        &mut module,
        "Pair",
        &["T"],
        vec![("first", tv("T")), ("second", tv("T"))],
    );
    register_generic_struct(
        &mut module,
        "Nested",
        &["T"],
        vec![("p", IrType::StructRef("Pair".into(), vec![tv("T")]))],
    );

    monomorphize(&mut module);

    // Both specialized layouts registered.
    let pair_spec = module
        .struct_layouts
        .get("Pair__i64")
        .expect("Pair__i64 layout missing");
    assert_eq!(
        pair_spec,
        &vec![
            ("first".to_string(), IrType::I64),
            ("second".to_string(), IrType::I64),
        ]
    );
    let nested_spec = module
        .struct_layouts
        .get("Nested__i64")
        .expect("Nested__i64 layout missing");
    // After rewrite_struct_refs_in_type, nested StructRef is
    // mangled and args are cleared.
    assert_eq!(
        nested_spec,
        &vec![(
            "p".to_string(),
            IrType::StructRef("Pair__i64".into(), vec![])
        )]
    );
}

/// Struct-mono on a module with no generic structs registered is a
/// no-op: `struct_layouts` / `method_index` / `dyn_vtables` unchanged.
#[test]
fn struct_mono_no_op_when_no_generic_structs() {
    let mut module = module_of(vec![FnBuilder::new(0, "main").build()]);
    module
        .struct_layouts
        .insert("Plain".into(), vec![("x".into(), IrType::I64)]);
    let before = module.struct_layouts.clone();

    monomorphize(&mut module);

    assert_eq!(module.struct_layouts, before);
}

/// `for_each_type_mut` (the helper that drives every type substitution
/// in monomorphization) must reach all closure-related type
/// annotations: the `IrFunction.capture_types` vector and the
/// per-value type of an `Op::ClosureLoadCapture` result.
///
/// This pins the contract regardless of whether monomorphization
/// currently *invokes* substitution on closure functions defined inside
/// generic templates — that end-to-end gap is tracked separately (see
/// the cross-width fixture and `docs/known-issues.md`). When the gap
/// closes by cloning closure functions per enclosing-generic
/// substitution, this test guarantees the cloned bodies will have
/// their TypeVars correctly rewritten.
#[test]
fn for_each_type_mut_substitutes_capture_types_and_load_result() {
    use crate::module::IrFunction;

    // A toy closure function shaped like `__closure(env) -> T` whose
    // single capture is of type `T` and whose body performs
    // `Op::ClosureLoadCapture(env, 0)` returning `T`.
    let env_ty = IrType::ClosureRef {
        param_types: vec![],
        return_type: Box::new(tv("T")),
    };
    let mut func = IrFunction::new_closure(
        FuncId(0),
        "__closure_under_T".into(),
        vec![env_ty.clone()],
        vec!["__env".into()],
        tv("T"),
        None,
        vec![tv("T")],
    );
    let entry = func.create_block();
    let env = func.add_block_param(entry, env_ty);
    let loaded = func
        .emit(entry, Op::ClosureLoadCapture(env, 0), tv("T"), None)
        .expect("non-void emit");
    func.set_terminator(entry, Terminator::Return(Some(loaded)));

    // Substitute T -> Int. After this, capture_types[0], the recorded
    // type of `loaded` (queried via instruction_result_type, which
    // reads from the value allocator), the function's return type,
    // and the env-param's nested return_type must all be `I64`.
    let mut subst = std::collections::HashMap::new();
    subst.insert("T".to_string(), IrType::I64);
    crate::monomorphize::substitute_types_in_fn(&mut func, &subst);

    assert_eq!(
        func.capture_types,
        vec![IrType::I64],
        "capture_types[0] should have been substituted T -> I64"
    );
    assert_eq!(
        func.return_type,
        IrType::I64,
        "return type should have been substituted"
    );
    assert_eq!(
        func.instruction_result_type(loaded),
        Some(&IrType::I64),
        "Op::ClosureLoadCapture's per-value-allocator type should have \
         been substituted"
    );
    // The env parameter's nested ClosureRef return_type also rewrites.
    match &func.param_types[0] {
        IrType::ClosureRef { return_type, .. } => {
            assert_eq!(**return_type, IrType::I64);
        }
        other => panic!("env param should be ClosureRef, got {other:?}"),
    }
}

/// Companion to the test above: substitution at a *2-slot* type
/// (StringRef) must reach the same annotations. If
/// monomorphization ever does start cloning closure functions per
/// enclosing-generic substitution, this is the case where the
/// Cranelift backend's slot accounting will diverge from the
/// erased-placeholder fallback — pinning that the substitution
/// itself is correct keeps the eventual fix's blast radius
/// confined to the cloning machinery.
#[test]
fn for_each_type_mut_substitutes_capture_types_at_string() {
    use crate::module::IrFunction;

    let env_ty = IrType::ClosureRef {
        param_types: vec![],
        return_type: Box::new(tv("T")),
    };
    let mut func = IrFunction::new_closure(
        FuncId(0),
        "__closure_under_T_str".into(),
        vec![env_ty],
        vec!["__env".into()],
        tv("T"),
        None,
        vec![tv("T")],
    );
    let entry = func.create_block();
    let env_vid = func.add_block_param(
        entry,
        IrType::ClosureRef {
            param_types: vec![],
            return_type: Box::new(tv("T")),
        },
    );
    let loaded = func
        .emit(entry, Op::ClosureLoadCapture(env_vid, 0), tv("T"), None)
        .expect("non-void emit");
    func.set_terminator(entry, Terminator::Return(Some(loaded)));

    let mut subst = std::collections::HashMap::new();
    subst.insert("T".to_string(), IrType::StringRef);
    crate::monomorphize::substitute_types_in_fn(&mut func, &subst);

    assert_eq!(func.capture_types, vec![IrType::StringRef]);
    assert_eq!(func.return_type, IrType::StringRef);
    assert_eq!(
        func.instruction_result_type(loaded),
        Some(&IrType::StringRef)
    );
}
