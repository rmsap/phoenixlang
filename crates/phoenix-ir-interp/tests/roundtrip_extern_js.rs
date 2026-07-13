//! `extern js` host-FFI binding for the IR interpreter.
//!
//! Mirrors the AST interpreter's host-binding tests so both interpreters share
//! one host-stub contract (`phoenix_common::host`) and produce identical output
//! — the foundation for the five-backend interop matrix.

mod common;

use common::lower_to_ir;
use phoenix_common::host::{CallbackHandle, HostValue};
use phoenix_ir_interp::run_with_host_capture;

#[test]
fn extern_js_return_value_marshals_back() {
    let module = lower_to_ir(
        "extern js { function getLength(s: String) -> Int }\n\
         function main() { print(getLength(\"abc\") + 1) }",
    );
    let lines = run_with_host_capture(&module, |interp| {
        interp.register_host(
            "js",
            "getLength",
            Box::new(|_ctx, args| match args.into_iter().next() {
                Some(HostValue::Str(s)) => Ok(HostValue::Int(s.len() as i64)),
                _ => Err("expected a string".to_string()),
            }),
        );
    })
    .unwrap();
    assert_eq!(lines, vec!["4".to_string()]);
}

#[test]
fn extern_js_callback_invokes_phoenix_closure() {
    let module = lower_to_ir(
        "extern js { function callNow(cb: () -> Void) }\n\
         function main() { callNow(function() { print(\"called back\") }) }",
    );
    let lines = run_with_host_capture(&module, |interp| {
        interp.register_host(
            "js",
            "callNow",
            Box::new(|ctx, args| match args.into_iter().next() {
                Some(HostValue::Callback(h)) => {
                    ctx.call_callback(h, vec![])?;
                    Ok(HostValue::Void)
                }
                _ => Err("expected a callback".to_string()),
            }),
        );
    })
    .unwrap();
    assert_eq!(lines, vec!["called back".to_string()]);
}

#[test]
fn extern_js_jsvalue_round_trips_through_host() {
    let module = lower_to_ir(
        "extern js {\n\
           function getEl(id: String) -> JsValue\n\
           function tagOf(e: JsValue) -> String\n\
         }\n\
         function main() {\n\
           let e: JsValue = getEl(\"root\")\n\
           print(tagOf(e))\n\
         }",
    );
    let lines = run_with_host_capture(&module, |interp| {
        interp.register_host("js", "getEl", Box::new(|_c, _a| Ok(HostValue::JsValue(7))));
        interp.register_host(
            "js",
            "tagOf",
            Box::new(|_c, args| match args.into_iter().next() {
                Some(HostValue::JsValue(7)) => Ok(HostValue::Str("DIV".to_string())),
                other => Err(format!("unexpected handle: {other:?}")),
            }),
        );
    })
    .unwrap();
    assert_eq!(lines, vec!["DIV".to_string()]);
}

#[test]
fn extern_js_unbound_host_errors_cleanly() {
    let module = lower_to_ir(
        "extern js { function alert(message: String) }\n\
         function main() { alert(\"x\") }",
    );
    let err = run_with_host_capture(&module, |_interp| {})
        .expect_err("an unbound extern call should error");
    assert!(
        err.message.contains("no host binding registered") && err.message.contains("js.alert"),
        "expected a clean unbound-host error, got: {}",
        err.message
    );
}

#[test]
fn extern_js_npm_module_dispatches_by_module() {
    // An `extern js "pkg" { ... }` extern dispatches to the
    // binding registered under the *package* module — the `(module, name)`
    // registry keying, parity with the AST interpreter.
    let module = lower_to_ir(
        "extern js \"left-pad\" { function leftPad(s: String, width: Int) -> String }\n\
         function main() { print(leftPad(\"4\", 3)) }",
    );
    let lines = run_with_host_capture(&module, |interp| {
        interp.register_host(
            "left-pad",
            "leftPad",
            Box::new(|_ctx, args| {
                let mut it = args.into_iter();
                match (it.next(), it.next()) {
                    (Some(HostValue::Str(s)), Some(HostValue::Int(w))) => {
                        Ok(HostValue::Str(format!("{s:>width$}", width = w as usize)))
                    }
                    other => Err(format!("unexpected args: {other:?}")),
                }
            }),
        );
    })
    .unwrap();
    assert_eq!(lines, vec!["  4".to_string()]);
}

#[test]
fn extern_js_npm_module_does_not_fall_back_to_the_ambient_host() {
    // A binding registered under the ambient `js` module must NOT satisfy a
    // same-named extern declared against an npm package — that would silently
    // mis-route the call. The unbound error names the package.
    let module = lower_to_ir(
        "extern js \"left-pad\" { function leftPad(s: String, width: Int) -> String }\n\
         function main() { print(leftPad(\"4\", 3)) }",
    );
    let err = run_with_host_capture(&module, |interp| {
        interp.register_host(
            "js",
            "leftPad",
            Box::new(|_ctx, _args| Err("the ambient binding must not be reached".to_string())),
        );
    })
    .expect_err("an npm extern with only an ambient binding should error");
    assert!(
        err.message.contains("no host binding registered")
            && err.message.contains("left-pad.leftPad"),
        "expected an unbound-host error naming the package, got: {}",
        err.message
    );
}

#[test]
fn extern_js_callback_can_call_another_extern() {
    // Re-entrancy: the host `run` invokes the Phoenix callback, which itself
    // calls a *second* extern (`shout`). The registry must stay populated for the
    // duration of the outer host call so the nested dispatch resolves instead of
    // reporting "no host binding" — parity with the AST interpreter's guard.
    let module = lower_to_ir(
        "extern js {\n\
           function run(cb: () -> Void)\n\
           function shout(s: String) -> String\n\
         }\n\
         function main() { run(function() { print(shout(\"hi\")) }) }",
    );
    let lines = run_with_host_capture(&module, |interp| {
        interp.register_host(
            "js",
            "run",
            Box::new(|ctx, args| match args.into_iter().next() {
                Some(HostValue::Callback(h)) => {
                    ctx.call_callback(h, vec![])?;
                    Ok(HostValue::Void)
                }
                _ => Err("expected a callback".to_string()),
            }),
        );
        interp.register_host(
            "js",
            "shout",
            Box::new(|_ctx, args| match args.into_iter().next() {
                Some(HostValue::Str(s)) => Ok(HostValue::Str(s.to_uppercase())),
                _ => Err("expected a string".to_string()),
            }),
        );
    })
    .expect("a nested extern through a callback should dispatch");
    assert_eq!(lines, vec!["HI".to_string()]);
}

#[test]
fn extern_js_named_args_reorder_to_positional() {
    // Named args on an extern call must arrive at the host in declared parameter
    // order — `pair` is declared `(name, id)` but called `(id:, name:)`. Parity
    // with the AST interpreter (both reorder before marshalling).
    let module = lower_to_ir(
        "extern js { function pair(name: String, id: Int) -> String }\n\
         function main() { print(pair(id: 7, name: \"x\")) }",
    );
    let lines = run_with_host_capture(&module, |interp| {
        interp.register_host(
            "js",
            "pair",
            Box::new(|_ctx, args| {
                let mut it = args.into_iter();
                match (it.next(), it.next()) {
                    (Some(HostValue::Str(name)), Some(HostValue::Int(id))) => {
                        Ok(HostValue::Str(format!("{name}={id}")))
                    }
                    _ => Err("expected (String, Int) in declared order".to_string()),
                }
            }),
        );
    })
    .unwrap();
    assert_eq!(lines, vec!["x=7".to_string()]);
}

#[test]
fn extern_js_callback_receives_marshalled_args() {
    // The host invokes the Phoenix callback *with* a value (the `setTimeout(cb,
    // x)` shape), exercising the inbound-arg marshalling in `call_callback` that
    // the empty-arg callback tests never reach. Parity with the AST interpreter.
    let module = lower_to_ir(
        "extern js { function withValue(cb: (Int) -> Void) }\n\
         function main() { withValue(function(n: Int) { print(n + 1) }) }",
    );
    let lines = run_with_host_capture(&module, |interp| {
        interp.register_host(
            "js",
            "withValue",
            Box::new(|ctx, args| match args.into_iter().next() {
                Some(HostValue::Callback(h)) => {
                    ctx.call_callback(h, vec![HostValue::Int(41)])?;
                    Ok(HostValue::Void)
                }
                _ => Err("expected a callback".to_string()),
            }),
        );
    })
    .unwrap();
    assert_eq!(lines, vec!["42".to_string()]);
}

#[test]
fn extern_js_host_error_surfaces_cleanly() {
    // A host function returning `Err` must surface as a clean runtime error
    // carrying the host's message — not a panic, not a swallowed failure.
    let module = lower_to_ir(
        "extern js { function boom() }\n\
         function main() { boom() }",
    );
    let err = run_with_host_capture(&module, |interp| {
        interp.register_host(
            "js",
            "boom",
            Box::new(|_ctx, _args| Err("host blew up".to_string())),
        );
    })
    .expect_err("a host function returning Err should error");
    assert!(
        err.message.contains("host blew up"),
        "expected the host error message to surface, got: {}",
        err.message
    );
}

#[test]
fn extern_js_host_returning_callback_is_rejected() {
    // A host that hands back a callback handle (not a receivable value) is
    // marshalled-in as a clean error rather than a value Phoenix cannot
    // represent.
    let module = lower_to_ir(
        "extern js { function evil() }\n\
         function main() { evil() }",
    );
    let err = run_with_host_capture(&module, |interp| {
        interp.register_host(
            "js",
            "evil",
            Box::new(|_ctx, _args| Ok(HostValue::Callback(CallbackHandle(0)))),
        );
    })
    .expect_err("a host returning a callback handle should error");
    assert!(
        err.message.contains("callback handle"),
        "expected a clean rejection of the returned callback, got: {}",
        err.message
    );
}

#[test]
fn jsvalue_equality_is_by_handle() {
    // Sema permits `==` on `JsValue`, so the IR interpreter must compare opaque
    // handles by identity: the same host handle equals itself; distinct handles
    // do not. Parity with the AST interpreter's `Value` equality.
    let module = lower_to_ir(
        "extern js {\n\
           function getEl(id: String) -> JsValue\n\
         }\n\
         function main() {\n\
           let a: JsValue = getEl(\"x\")\n\
           let b: JsValue = getEl(\"x\")\n\
           let c: JsValue = getEl(\"y\")\n\
           print(a == a)\n\
           print(a == b)\n\
           print(a == c)\n\
         }",
    );
    let lines = run_with_host_capture(&module, |interp| {
        interp.register_host(
            "js",
            "getEl",
            Box::new(|_c, args| match args.into_iter().next() {
                Some(HostValue::Str(s)) if s == "y" => Ok(HostValue::JsValue(2)),
                Some(HostValue::Str(_)) => Ok(HostValue::JsValue(1)),
                _ => Err("expected a string".to_string()),
            }),
        );
    })
    .unwrap();
    assert_eq!(
        lines,
        vec!["true".to_string(), "true".to_string(), "false".to_string()]
    );
}
