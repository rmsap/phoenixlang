//! `json.encode` / `json.decode` for the tree-walk interpreter.
//!
//! The compiled backends run synthesized per-type IR routines
//! (`phoenix-ir`'s `json_synth`); this interpreter works directly on runtime
//! [`Value`]s instead, guided by the sema-recorded static types
//! ([`Interpreter::json_encode_spans`] / [`Interpreter::json_decode_types`],
//! seeded in `multi_module.rs`). The two implementations are pinned against
//! each other by the cross-backend fixture matrix.

use phoenix_sema::types::OPTION_ENUM;

use crate::value::Value;

use super::{Interpreter, Result, RuntimeError, err_val, error, none_val, ok_val, some_val};

/// Constructs a `JsonError::<variant>(message)` value.
fn json_error_value(variant: &str, msg: &str) -> Value {
    Value::EnumVariant(
        "JsonError".to_string(),
        variant.to_string(),
        vec![Value::String(msg.to_string())],
    )
}

/// Unwrap a decoded `Result<T, JsonError>` runtime value: `Ok(v)` yields the
/// payload; anything else (an `Err(JsonError)`) comes back as `Err` for the
/// caller to propagate unchanged.
fn unwrap_decoded(decoded: Value) -> std::result::Result<Value, Value> {
    match decoded {
        Value::EnumVariant(_, ref v, inner) if v == "Ok" => {
            Ok(inner.into_iter().next().unwrap_or(Value::Void))
        }
        other => Err(other),
    }
}

impl Interpreter {
    /// Recursively encode a runtime value to a JSON string.
    ///
    /// Scalars route through `Value`'s `Display` (the same rendering as
    /// `toString`), so they match the compiled backends' `toString`-based
    /// encoders; strings use the shared `phoenix_runtime::json_escape`; a
    /// struct emits an object with its fields in declaration order;
    /// `Option<T>` encodes as `null`/passthrough and other enums are
    /// adjacently tagged (`{"type":"V","value":[…]}`); a `List<T>` becomes an
    /// array and a `Map<String, V>` an object (insertion order). This covers
    /// scalars, structs, `Option`, non-generic enums, `List`, and
    /// `Map<String, _>`; richer shapes (non-`String`-key maps, generic enums)
    /// arrive with later slices and are gated in sema.
    pub(super) fn json_encode_value(&self, value: &Value) -> Result<String> {
        match value {
            Value::String(s) => Ok(phoenix_runtime::json_escape(s)),
            Value::Int(_) | Value::Float(_) | Value::Bool(_) => Ok(value.to_string()),
            Value::Struct(name, fields) => {
                let def = self.structs.get(name).ok_or_else(|| RuntimeError {
                    message: format!("json.encode: unknown struct `{name}`"),
                    try_return_value: None,
                })?;
                let mut parts = Vec::with_capacity(def.field_names.len());
                for fname in &def.field_names {
                    let fv = fields.get(fname).ok_or_else(|| RuntimeError {
                        message: format!("json.encode: struct `{name}` is missing field `{fname}`"),
                        try_return_value: None,
                    })?;
                    // Field-name keys are identifiers, so raw quoting is valid
                    // JSON (matching the synthesized IR encoders).
                    parts.push(format!("\"{}\":{}", fname, self.json_encode_value(fv)?));
                }
                Ok(format!("{{{}}}", parts.join(",")))
            }
            // `Option<T>`: None → null, Some(x) → encode(x). Other enums are
            // adjacently tagged.
            Value::EnumVariant(enum_name, variant, fields) if enum_name == OPTION_ENUM => {
                match variant.as_str() {
                    "None" => Ok("null".to_string()),
                    "Some" => self.json_encode_value(&fields[0]),
                    other => error(format!("json.encode: unexpected Option variant `{other}`")),
                }
            }
            Value::EnumVariant(_, variant, fields) => {
                // Variant names are identifiers, so raw quoting is valid JSON.
                if fields.is_empty() {
                    Ok(format!("{{\"type\":\"{variant}\"}}"))
                } else {
                    let mut parts = Vec::with_capacity(fields.len());
                    for fv in fields {
                        parts.push(self.json_encode_value(fv)?);
                    }
                    Ok(format!(
                        "{{\"type\":\"{variant}\",\"value\":[{}]}}",
                        parts.join(",")
                    ))
                }
            }
            // `List<T>` → array.
            Value::List(elems) => {
                let mut parts = Vec::with_capacity(elems.len());
                for e in elems {
                    parts.push(self.json_encode_value(e)?);
                }
                Ok(format!("[{}]", parts.join(",")))
            }
            // `Map<String, V>` → object. Sema guarantees String keys for this
            // slice, so the empty case is unambiguously `{}`.
            Value::Map(entries) => {
                let mut parts = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    let Value::String(ks) = k else {
                        return error(
                            "json.encode: Map with non-String keys is not supported yet"
                                .to_string(),
                        );
                    };
                    parts.push(format!(
                        "{}:{}",
                        phoenix_runtime::json_escape(ks),
                        self.json_encode_value(v)?
                    ));
                }
                Ok(format!("{{{}}}", parts.join(",")))
            }
            other => error(format!(
                "json.encode does not support this value yet: {other}"
            )),
        }
    }

    /// Decode `text` into a `Result<T, JsonError>` runtime value guided by
    /// the target type `ty`. Parse errors become
    /// `Err(ParseError(msg))`; a shape mismatch becomes `Err(TypeMismatch)`.
    /// Malformed *input* is always a returned `Err(JsonError)` value; the
    /// outer `Result`'s `Err` is reserved for internal inconsistencies
    /// (sema/interpreter tables out of sync).
    pub(super) fn json_decode(&self, text: &str, ty: &phoenix_sema::types::Type) -> Result<Value> {
        match serde_json::from_str::<serde_json::Value>(text) {
            Err(e) => Ok(err_val(json_error_value("ParseError", &e.to_string()))),
            Ok(dom) => self.json_decode_value(&dom, ty),
        }
    }

    /// Build a `Result<T, JsonError>` from a parsed DOM node and target type:
    /// scalars, `Option<T>`, `List<T>`, and non-generic structs and enums (the
    /// composite shapes each dispatch to their own helper).
    fn json_decode_value(
        &self,
        dom: &serde_json::Value,
        ty: &phoenix_sema::types::Type,
    ) -> Result<Value> {
        use phoenix_sema::types::Type;
        let mismatch = |name: &str| Ok(err_val(json_error_value("TypeMismatch", name)));
        match ty {
            Type::Int => match dom.as_i64() {
                Some(i) => Ok(ok_val(Value::Int(i))),
                None => mismatch("expected Int"),
            },
            // A JSON integer is a valid Float too.
            Type::Float => match dom.as_f64() {
                Some(f) => Ok(ok_val(Value::Float(f))),
                None => mismatch("expected Float"),
            },
            Type::Bool => match dom.as_bool() {
                Some(b) => Ok(ok_val(Value::Bool(b))),
                None => mismatch("expected Bool"),
            },
            Type::String => match dom.as_str() {
                Some(s) => Ok(ok_val(Value::String(s.to_string()))),
                None => mismatch("expected String"),
            },
            // `Option<T>`: `null` → `None`, else decode `T` and wrap `Some`.
            Type::Generic(name, args) if name == OPTION_ENUM && args.len() == 1 => {
                if dom.is_null() {
                    return Ok(ok_val(none_val()));
                }
                match unwrap_decoded(self.json_decode_value(dom, &args[0])?) {
                    Ok(v) => Ok(ok_val(some_val(v))),
                    Err(other) => Ok(other), // Err(JsonError) propagates.
                }
            }
            // `List<T>`: require a JSON array, decode each element (a decode
            // error propagates), then build the list.
            Type::Generic(name, args) if name == "List" && args.len() == 1 => {
                let Some(arr) = dom.as_array() else {
                    return mismatch("expected array");
                };
                let mut elems = Vec::with_capacity(arr.len());
                for elem in arr {
                    match unwrap_decoded(self.json_decode_value(elem, &args[0])?) {
                        Ok(v) => elems.push(v),
                        Err(other) => return Ok(other), // Err(JsonError) propagates.
                    }
                }
                Ok(ok_val(Value::List(elems)))
            }
            // `Map<String, V>`: require a JSON object, decode each entry's
            // value (a decode error propagates), then build the map. Entries
            // iterate in serde's key order — identical to the compiled
            // backends' DOM iteration. The `String`-key guard keeps the
            // planned non-`String`-key pairs form from silently routing
            // through the object decoder if only sema's gate is relaxed.
            Type::Generic(name, args)
                if name == "Map" && args.len() == 2 && args[0] == Type::String =>
            {
                let Some(obj) = dom.as_object() else {
                    return mismatch("expected object");
                };
                let mut pairs = Vec::with_capacity(obj.len());
                for (k, child) in obj {
                    match unwrap_decoded(self.json_decode_value(child, &args[1])?) {
                        Ok(v) => pairs.push((Value::String(k.clone()), v)),
                        Err(other) => return Ok(other), // Err(JsonError) propagates.
                    }
                }
                Ok(ok_val(Value::Map(pairs)))
            }
            // A non-generic struct or enum: require an object and build it.
            Type::Named(name) => {
                if let Some(fields) = self.json_struct_fields.get(name) {
                    self.json_decode_struct(dom, name, fields)
                } else if let Some(variants) = self.json_enum_variants.get(name) {
                    self.json_decode_enum(dom, name, variants)
                } else {
                    // A miss here is an internal bug, not bad input: sema's
                    // gate admitted the type, so `seed_from_resolved` must know
                    // it. Surface it as an interpreter error rather than
                    // masking it as a plausible `Err(TypeMismatch)`.
                    error(format!(
                        "json.decode target `{name}` passed sema's gate but has no \
                         struct/enum entry — sema/interpreter tables are out of sync"
                    ))
                }
            }
            other => Ok(err_val(json_error_value(
                "TypeMismatch",
                &format!("json.decode does not support {other} yet"),
            ))),
        }
    }

    /// Decode a non-generic struct: require an object, decode each field (an
    /// absent `Option` field → `None` — absent ≡ null, see design-decisions
    /// §Phase 4.6 B; any other missing field → MissingField; a field error
    /// propagates), then build. Mirrors the synthesized IR struct decoder.
    fn json_decode_struct(
        &self,
        dom: &serde_json::Value,
        struct_name: &str,
        fields: &[(String, phoenix_sema::types::Type)],
    ) -> Result<Value> {
        use phoenix_sema::types::Type;
        if !dom.is_object() {
            return Ok(err_val(json_error_value("TypeMismatch", "expected object")));
        }
        let mut field_values = std::collections::BTreeMap::new();
        for (fname, fty) in fields {
            let Some(child) = dom.get(fname) else {
                let is_option = matches!(
                    fty,
                    Type::Generic(n, args) if n == OPTION_ENUM && args.len() == 1
                );
                if is_option {
                    field_values.insert(fname.clone(), none_val());
                    continue;
                }
                return Ok(err_val(json_error_value("MissingField", fname)));
            };
            match unwrap_decoded(self.json_decode_value(child, fty)?) {
                Ok(v) => {
                    field_values.insert(fname.clone(), v);
                }
                Err(other) => return Ok(other), // Err(JsonError) propagates.
            }
        }
        Ok(ok_val(Value::Struct(struct_name.to_string(), field_values)))
    }

    /// Decode an adjacently-tagged enum: require an object, read the `"type"`
    /// string discriminator, and — for a variant with fields — decode its
    /// `"value"` array positionally. Mirrors the synthesized IR enum decoder.
    fn json_decode_enum(
        &self,
        dom: &serde_json::Value,
        enum_name: &str,
        variants: &[(String, Vec<phoenix_sema::types::Type>)],
    ) -> Result<Value> {
        let mismatch = |name: &str| Ok(err_val(json_error_value("TypeMismatch", name)));
        if !dom.is_object() {
            return mismatch("expected object");
        }
        let Some(tag_node) = dom.get("type") else {
            return Ok(err_val(json_error_value("MissingField", "type")));
        };
        let Some(tag) = tag_node.as_str() else {
            return mismatch("expected a string \"type\" discriminator");
        };
        let Some((vname, ftys)) = variants.iter().find(|(n, _)| n == tag) else {
            return mismatch(&format!("unknown enum variant: {tag}"));
        };
        if ftys.is_empty() {
            return Ok(ok_val(Value::EnumVariant(
                enum_name.to_string(),
                vname.clone(),
                vec![],
            )));
        }
        let Some(value_node) = dom.get("value") else {
            return Ok(err_val(json_error_value("MissingField", "value")));
        };
        let Some(arr) = value_node.as_array() else {
            return mismatch("expected a \"value\" array");
        };
        let mut field_vals = Vec::with_capacity(ftys.len());
        for (i, fty) in ftys.iter().enumerate() {
            let Some(elem) = arr.get(i) else {
                return mismatch("too few elements in \"value\" array");
            };
            match unwrap_decoded(self.json_decode_value(elem, fty)?) {
                Ok(v) => field_vals.push(v),
                Err(other) => return Ok(other), // Err(JsonError) propagates.
            }
        }
        Ok(ok_val(Value::EnumVariant(
            enum_name.to_string(),
            vname.clone(),
            field_vals,
        )))
    }
}
