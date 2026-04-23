# `dyn Trait` — Runtime Trait Dispatch

Phoenix has two ways to write code that works against any type implementing
a trait:

- **Static dispatch** with a generic bound — `function f<T: Trait>(x: T)`.
  The compiler emits one specialized copy per concrete `T`. Zero runtime
  overhead. Each call site monomorphizes.
- **Dynamic dispatch** with `dyn Trait` — `function f(x: dyn Trait)`. The
  value carries a vtable pointer at runtime; the call goes through one
  indirection. Use it when you want one function to accept *several*
  unrelated implementors at the same call site, or to store mixed
  implementors behind a single type.

Both are first-class. Pick whichever fits.

## Quick example

```phoenix
trait Drawable {
    function draw(self) -> String
}

struct Circle { Int radius }
  impl Drawable for Circle {
      function draw(self) -> String { return "circle" }
}

struct Square { Int side }
  impl Drawable for Square {
      function draw(self) -> String { return "square" }
}

// Accepts any Drawable, dispatches at runtime.
function render(s: dyn Drawable) -> String {
    return s.draw()
}

function main() {
    print(render(Circle(3)))   // "circle"
    print(render(Square(5)))   // "square"
}
```

The `dyn Drawable` parameter is a fat pointer: `(data_ptr, vtable_ptr)`.
The Phoenix compiler inserts the wrap automatically wherever a concrete
implementor flows into a `dyn Trait` slot.

## When the wrap happens

The compiler inserts the concrete-to-`dyn` coercion at every assignment
boundary:

```phoenix
function takeDyn(x: dyn Drawable) { ... }

function main() {
    // 1. Function call argument:
    takeDyn(Circle(1))

    // 2. Let binding with a `dyn` annotation:
    let d: dyn Drawable = Circle(1)

    // 3. Reassignment to a `let mut` typed `dyn`:
    let mut m: dyn Drawable = Circle(1)
    m = Square(2)

    // 4. Function return:
    let f = function() -> dyn Drawable { Circle(1) }

    // 5. Struct field typed `dyn`:
    let s = Scene(Circle(1))   // where Scene { dyn Drawable hero }

    // 6. Enum variant field typed `dyn`:
    let v = Held(Circle(1))    // where Held(dyn Drawable)
}
```

You don't write the wrap explicitly. The trait's method calls (`d.draw()`)
look identical to a method call on a concrete type — the runtime indirection
is invisible at the source level.

## Object safety

A trait can only be used as `dyn Trait` if it is **object-safe**.
Concretely, no method's parameter or return types may mention `Self`:

```phoenix
trait Cloneable {
    function clone(self) -> Self    // ❌ returns Self
}

trait Eq {
    function eq(self, other: Self) -> Bool   // ❌ takes Self by value
}

trait Drawable {
    function draw(self) -> String    // ✅ no Self in params/return
}
```

Non-object-safe traits remain perfectly usable as **generic bounds**:

```phoenix
// Even though `Eq` is not object-safe, `<T: Eq>` is fine —
// monomorphization fills in the concrete type and the generated code
// knows what `Self` is at the call site.
function dedup<T: Eq>(xs: List<T>) -> List<T> { ... }
```

The compiler reports the violation when you try `dyn Cloneable`, with a
suggestion to use `<T: Cloneable>` instead.

## When to choose which

| Situation | Use |
| --- | --- |
| One concrete type per call site, want zero overhead | `<T: Trait>` (static) |
| Heterogeneous collection / plugin-like API | `dyn Trait` (dynamic) |
| Trait method signature mentions `Self` | `<T: Trait>` (forced — not object-safe) |
| Library API where users supply their own implementors | usually `dyn Trait` |
| Hot inner loop, dispatch is expensive relative to body | `<T: Trait>` (inlines) |

If you're unsure, start with `<T: Trait>`. Switch to `dyn Trait` only when
you actually need heterogeneity or when monomorphization is making your
binary unreasonably large.

## Comparing the two side-by-side

```phoenix
function describeStatic<T: Drawable>(item: T) -> String { item.draw() }
function describeDyn(item: dyn Drawable) -> String      { item.draw() }
```

`describeStatic(Circle(1))` and `describeDyn(Circle(1))` produce identical
output. The difference is what the compiler does:

- `describeStatic` is monomorphized — one machine-code copy per concrete
  type that flows in. No vtable lookup at the call site.
- `describeDyn` is one machine-code copy that loads the function pointer
  from the vtable on every call.

## Limitations and gotchas

These are tracked in `docs/known-issues.md` and will be resolved in later
phases. None of them are fundamental — they are work-in-progress gaps.

- **`dyn Foo + Bar` (multi-bound trait objects)** is not supported.
  Pick the most specific single trait.
- **Supertraits (`trait Sub: Super`)** are not modeled in sema, so
  `dyn Sub → dyn Super` upcasting is not available.
- **`List<dyn Trait>` literals** (`[Circle(1), Square(2)]` typed
  `List<dyn Drawable>`) do not currently compile end-to-end.
  Bidirectional inference into list-literal contexts is the missing
  piece. Sema accepts the program; IR lowering then fails to materialize
  the per-element wrap.
- **`<T: Trait>` → `dyn Trait` coercion in compiled mode** does not work
  yet — `function wrap<T: Drawable>(x: T) -> dyn Drawable { x }` runs
  under `phoenix run` but panics at IR lowering under `phoenix build`.
  Wrap concrete values *outside* the generic function as a workaround.
- **`<T: Trait>` method calls in compiled mode** fall into the same
  monomorphization-vs-lowering gap. Same workaround.
- **`dyn Trait` over generic structs** (`struct Box<T> { ... } impl Trait for Box`
  used as `dyn Trait`) is blocked by the broader generic-struct gap in
  compiled mode and lands as part of that fix.
- **Default arguments typed `dyn Trait`** are not supported in compiled
  mode (default arguments themselves are not yet supported in compiled
  mode). Pass every argument explicitly.

## What `dyn` actually compiles to

A `dyn Trait` value is a 2-slot fat pointer:

```
struct DynRef {
    void*      data_ptr;     // points at the heap-allocated concrete value
    void**     vtable_ptr;   // points at a static array of function pointers
}
```

The vtable for each `(concrete_type, trait)` pair is emitted once into
read-only data. Method dispatch on `x.method()` where `x: dyn Trait`
loads the function pointer from `vtable_ptr[method_slot * pointer_size]`
and calls it indirectly with `data_ptr` prepended as the first argument.

There's no runtime allocation overhead beyond what the concrete type
already costs — wrapping `Circle(3)` into a `dyn Drawable` does not
copy or re-box; it just builds the fat pointer pair from the existing
heap allocation.

## Further reading

- [`docs/design-decisions.md`](design-decisions.md) — the rationale
  behind explicit `dyn`, the vtable ABI, and the object-safety rules.
- [`docs/known-issues.md`](known-issues.md) — full list of `dyn`-adjacent
  limitations and their target fix phases.
- [`docs/phases/phase-2.md`](phases/phase-2.md) — where the `dyn Trait`
  work fits in the compilation roadmap.
