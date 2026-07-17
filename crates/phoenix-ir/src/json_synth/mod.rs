//! JSON encoder / decoder synthesis.
//!
//! For every type reachable from a `json.encode(value)` or
//! `json.decode<T>(text)` call, these passes synthesize per-type IR
//! routines and record them in [`crate::module::IrModule::json_encoders`] /
//! [`crate::module::IrModule::json_decoders`]. Because the synthesized
//! routines are ordinary IR, all five backends execute them uniformly —
//! there is no per-backend serialization logic. Each call site then lowers
//! to an `Op::Call` of the routine for its static type (see
//! `lower_method_call`).
//!
//! Runs as a Pass 1.5 step (after declaration registration, before user
//! bodies are lowered) so the stubs exist before any call site — or any
//! sibling encoder/decoder — needs to reference them. Mirrors the two-pass
//! shape of [`crate::default_wrappers`].
//!
//! [`encode`] and [`decode`] hold the per-direction synthesis
//! ([`decode_emit`] carries the decoder side's shape-agnostic emit
//! toolkit); this module owns what the directions share: the type-key
//! scheme ([`encode_type_key`]), symbol mangling ([`sanitize`]), and
//! enum-layout lookups ([`enum_variant_index`]).

mod decode;
mod decode_emit;
mod encode;

pub(crate) use decode::synthesize_json_decoders;
pub(crate) use encode::synthesize_json_encoders;

use phoenix_sema::types::Type;

use crate::lower::LoweringContext;
use crate::types::{LIST_TYPE, MAP_TYPE, OPTION_ENUM};

/// A stable, collision-free key per encodable/decodable type: a scalar's
/// name, a struct's or non-generic enum's qualified name
/// (`"models.user::User"`), or an `Option<T>` parameterized by its element
/// key (`"Option<Int>"`). Shared with the `json.encode` / `json.decode`
/// dispatch in `lower_method_call`.
pub(crate) fn encode_type_key(ty: &Type) -> String {
    match ty {
        Type::Int => "Int".to_string(),
        Type::Float => "Float".to_string(),
        Type::Bool => "Bool".to_string(),
        Type::String => "String".to_string(),
        // Generic collections need a distinct encoder per instantiation.
        Type::Generic(name, args) if name == OPTION_ENUM && args.len() == 1 => {
            format!("Option<{}>", encode_type_key(&args[0]))
        }
        Type::Generic(name, args) if name == LIST_TYPE && args.len() == 1 => {
            format!("List<{}>", encode_type_key(&args[0]))
        }
        Type::Generic(name, args) if name == MAP_TYPE && args.len() == 2 => format!(
            "Map<{},{}>",
            encode_type_key(&args[0]),
            encode_type_key(&args[1])
        ),
        // Both structs and non-generic enums key on their qualified name.
        Type::Named(name) => name.clone(),
        other => unreachable!(
            "json encode: unsupported type reached synthesis ({other:?}) — \
             sema's `unsupported_json_encode_type` gate should reject it"
        ),
    }
}

/// Map a type key to a symbol-safe suffix: every non-alphanumeric character
/// (`::`, `<`, `>`, …) becomes `_` so the result is a valid object symbol.
/// Not collision-free on its own — the mangling can alias distinct keys
/// (e.g. `a::b` and a type literally named `a__b`) — so the stub-registration
/// helpers append the assigned (globally unique) `FuncId`. Dispatch never
/// relies on the symbol name: the `json_encoders` / `json_decoders` maps are
/// keyed by the unmangled key.
fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// The discriminant index of `variant` in `enum_name`'s IR layout, looked up
/// rather than assuming sema's variant order matches the layout order. The two
/// agree today (the layout is built from sema's variant list), but resolving
/// the index keeps every enum encoder correct if that ever changes.
fn enum_variant_index(ctx: &LoweringContext<'_>, enum_name: &str, variant: &str) -> u32 {
    ctx.module
        .enum_layouts
        .get(enum_name)
        .and_then(|vs| vs.iter().position(|(n, _)| n == variant))
        .unwrap_or_else(|| unreachable!("{enum_name} layout missing variant `{variant}`"))
        as u32
}
