//! Garbage collector for compiled Phoenix programs.
//!
//! Phase 2.3 baseline: tracing mark-and-sweep, single-threaded, with
//! precise stack roots (via [`shadow_stack`]) and conservative interior
//! scanning (every 8-byte-aligned word inside a heap payload is checked
//! against the allocation registry; if it matches a live header, the
//! object is reachable).
//!
//! Subordinate design decisions A–G are documented in
//! [`docs/design-decisions.md`](../../../../docs/design-decisions.md).

pub(crate) mod heap;
pub(crate) mod shadow_stack;

use std::sync::{Mutex, MutexGuard, OnceLock};

pub(crate) use heap::MarkSweepHeap;

use crate::runtime_abort;

/// Lock the singleton heap from an `extern "C"` entry point. Routes
/// the poisoned-mutex case through `runtime_abort` so the failure mode
/// is FFI-safe (`panic!` would unwind across the C ABI). In single-
/// threaded operation the mutex can never be poisoned, but a future
/// concurrency story would mean a panic on one thread leaves the lock
/// poisoned for every subsequent C-ABI call — easier to land here once
/// than at every `extern "C"` site.
///
/// The single-threaded invariant is enforced at collection time, not
/// here — see [`MarkSweepHeap::collect`]. Allocations alone don't
/// sweep anyone's roots, so registering a fresh allocation from any
/// thread is harmless. Sweeping with a stale root set is what
/// corrupts memory, so that's where we gate.
fn lock_heap() -> MutexGuard<'static, MarkSweepHeap> {
    match heap().lock() {
        Ok(g) => g,
        Err(_) => runtime_abort("GC heap mutex poisoned"),
    }
}

/// Type tag stored in every object header.
///
/// `Unknown` is the conservative default — the GC scans the payload
/// for interior pointers. `String` is special-cased: its payload is
/// raw UTF-8 bytes, never pointers, so the scan is skipped.
///
/// The typed variants `List` / `Map` / `Closure` / `Struct` / `Enum`
/// are threaded through their respective allocation sites as of
/// phase 2.7 see `crates/phoenix-runtime/src/{list,map}_methods.rs`
/// and `crates/phoenix-cranelift/src/translate/{data,calls,enum_helpers}.rs`).
/// Conservative interior scanning still runs on those payloads; trace
/// tables (GC subordinate decision C — replace the scan with a
/// per-tag mark function) are queued behind a pause-distribution
/// regression signal that the bench has not yet shown.
/// `Dyn` remains declared-but-unused — no codegen site is emitting
/// `dyn Trait` allocations through a tag-aware path today.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TypeTag {
    /// Untyped allocation — payload is scanned conservatively.
    /// Default fallback when an unknown tag value is decoded.
    Unknown = 0,
    /// UTF-8 string bytes — payload is NOT scanned (strings hold no pointers).
    String = 1,
    /// `List<T>` — payload starts with the list header (`length`,
    /// `capacity`, `elem_size`), then element data. Conservatively scanned.
    List = 2,
    /// `Map<K, V>` — payload starts with the map header (`length`,
    /// `capacity`, `key_size`, `val_size`), then key/value pairs.
    /// Conservatively scanned.
    Map = 3,
    /// Closure environment — `[fn_ptr, capture_0, ...]`. Conservatively scanned.
    Closure = 4,
    /// User struct. Conservatively scanned.
    Struct = 5,
    /// User enum variant. Conservatively scanned.
    Enum = 6,
    /// `dyn Trait` data block. Conservatively scanned.
    Dyn = 7,
}

// Compile-time fence on the 7-bit type-tag field (`ObjectHeader` bits
// 1..8). A 128th variant would silently corrupt the size field at
// runtime in release builds — catch it at build time instead.
//
// The §C design-decisions text also flags a 16-variant ceiling on the
// narrower field that returns when Phase 2.7 reinstates size classes;
// that's a softer budget, this is the hard one.
const _TYPE_TAG_FITS_IN_7_BITS: () = {
    assert!(
        (TypeTag::Dyn as u8) < 128,
        "TypeTag enum exceeded the 7-bit budget reserved by ObjectHeader",
    );
};

impl TypeTag {
    /// Whether the GC should scan this payload's interior for pointers.
    fn scans_interior(self) -> bool {
        !matches!(self, TypeTag::String)
    }

    /// Convert from a `u32` (the C-ABI representation passed by codegen).
    ///
    /// Unrecognized values fall back to [`TypeTag::Unknown`] (the
    /// conservative scan keeps the GC correct). The C-ABI entry points
    /// (`phx_gc_alloc`) `debug_assert!` the value before calling this;
    /// the function itself is total so it can be tested in any build.
    pub fn from_u32(tag: u32) -> Self {
        match tag {
            1 => TypeTag::String,
            2 => TypeTag::List,
            3 => TypeTag::Map,
            4 => TypeTag::Closure,
            5 => TypeTag::Struct,
            6 => TypeTag::Enum,
            7 => TypeTag::Dyn,
            _ => TypeTag::Unknown,
        }
    }
}

/// Object header placed immediately before every GC-managed payload.
///
/// Layout (8 bytes, little-endian):
/// - bit    0     — mark bit
/// - bits   1..8  — 7-bit type tag ([`TypeTag`]; values 0..127)
/// - bits   8..32 — reserved (forwarding pointer slot for future moving GC)
/// - bits  32..64 — payload size in bytes
///
/// No `Debug` impl: the default `derive(Debug)` would just print
/// `ObjectHeader { word: <u64> }`, which is unhelpful when debugging
/// mark/sweep failures. If a future debugging session needs decoded
/// output, hand-write a `Debug` impl that prints `mark`, `tag`, and
/// `payload_size` separately.
#[repr(C)]
pub(crate) struct ObjectHeader {
    word: u64,
}

impl ObjectHeader {
    pub(crate) fn new(payload_size: usize, tag: TypeTag) -> Self {
        // The 7-bit budget is enforced at compile time by
        // `_TYPE_TAG_FITS_IN_7_BITS` above — no runtime check needed.
        let size_bits = (payload_size as u64) << 32;
        let tag_bits = (tag as u64) << 1;
        Self {
            word: size_bits | tag_bits,
        }
    }

    pub(crate) fn payload_size(&self) -> usize {
        (self.word >> 32) as usize
    }

    pub(crate) fn tag(&self) -> TypeTag {
        TypeTag::from_u32(((self.word >> 1) & 0x7F) as u32)
    }

    pub(crate) fn is_marked(&self) -> bool {
        self.word & 1 != 0
    }

    pub(crate) fn set_mark(&mut self) {
        self.word |= 1;
    }

    pub(crate) fn clear_mark(&mut self) {
        self.word &= !1;
    }
}

/// Header size in bytes. Payload is placed immediately after.
pub(crate) const HEADER_SIZE: usize = std::mem::size_of::<ObjectHeader>();

/// Default collection threshold: collect when this many bytes have
/// been allocated since the last collection.
pub const DEFAULT_COLLECTION_THRESHOLD: usize = 1024 * 1024;

/// Abstract heap interface. One impl in Phase 2.3 ([`MarkSweepHeap`]);
/// Phase 2.4 plugs in a WASM-GC-backed impl behind the same trait.
pub(crate) trait GcHeap {
    /// Allocate `size` bytes with the given type tag. Returns a pointer
    /// to the payload (caller-visible); the header is at `payload - 8`.
    fn alloc(&mut self, size: usize, tag: TypeTag) -> *mut u8;

    /// Force a collection cycle. Reads roots from the shadow stack.
    fn collect(&mut self);

    /// Approximate live-bytes count after the last collection.
    fn live_bytes(&self) -> usize;

    /// Total number of live objects (debug helper).
    fn live_objects(&self) -> usize;
}

/// Process-wide singleton heap. Initialized lazily on first allocation.
fn heap() -> &'static Mutex<MarkSweepHeap> {
    static HEAP: OnceLock<Mutex<MarkSweepHeap>> = OnceLock::new();
    HEAP.get_or_init(|| Mutex::new(MarkSweepHeap::new_singleton()))
}

// ── C ABI entry points ──────────────────────────────────────────────

/// Allocate `size` bytes and register the allocation with the GC.
///
/// `type_tag` is interpreted via [`TypeTag::from_u32`]. Unrecognized
/// values default to `Unknown` (conservative scan of the payload).
///
/// Returns a pointer to the **payload** (the header is at `payload - 8`).
/// The payload is zeroed.
///
/// # Safety
///
/// The returned pointer is owned by the GC; do not call `free` on it.
/// The payload is valid until the next collection in which no shadow-stack
/// frame holds a reference to it.
#[unsafe(no_mangle)]
pub extern "C" fn phx_gc_alloc(size: usize, type_tag: u32) -> *mut u8 {
    // Codegen drift detector: out-of-range tag in debug builds trips
    // here so a stale `TypeTag` constant in the Cranelift backend
    // surfaces in CI. Release builds fall through to the conservative
    // `Unknown` scan inside `from_u32` and stay correct.
    debug_assert!(
        type_tag <= TypeTag::Dyn as u32,
        "phx_gc_alloc: unrecognized type tag {type_tag}; codegen out of sync \
         with runtime"
    );
    let tag = TypeTag::from_u32(type_tag);
    let mut h = lock_heap();
    h.alloc(size, tag)
}

/// Push a shadow-stack frame with `n_roots` slots, all initialized to null.
/// Returns an opaque frame handle (a `*mut Frame`) that the caller stashes
/// in a stack slot and passes to [`phx_gc_set_root`] / [`phx_gc_pop_frame`].
#[unsafe(no_mangle)]
pub extern "C" fn phx_gc_push_frame(n_roots: usize) -> *mut shadow_stack::Frame {
    shadow_stack::push_frame(n_roots)
}

/// Pop the most recently pushed shadow-stack frame.
///
/// `frame` must be the handle returned by the matching
/// [`phx_gc_push_frame`] (used as a sanity check).
///
/// # Safety
///
/// `frame` must be the pointer returned by the matching push.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_gc_pop_frame(frame: *mut shadow_stack::Frame) {
    unsafe { shadow_stack::pop_frame(frame) }
}

/// Write `ptr` into root slot `idx` of the given frame.
///
/// # Safety
///
/// `frame` must be a valid handle returned by [`phx_gc_push_frame`] and
/// `idx` must be `< n_roots` for that frame.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn phx_gc_set_root(
    frame: *mut shadow_stack::Frame,
    idx: usize,
    ptr: *mut u8,
) {
    unsafe { shadow_stack::set_root(frame, idx, ptr) }
}

/// Force a collection cycle. Rust-only API used by tests and benchmarks;
/// not exported to the C ABI because no compiled-code path needs to
/// trigger collection explicitly (allocation sites do it via the
/// threshold).
pub fn phx_gc_collect() {
    let mut h = lock_heap();
    h.collect();
}

/// Enable threshold-triggered automatic collection. Off by default so
/// allocations from un-instrumented code can't accidentally trigger a
/// collection that frees everything (no shadow-stack frame ⇒ no roots).
/// Cranelift's `gc_roots` emission calls this once at process start.
#[unsafe(no_mangle)]
pub extern "C" fn phx_gc_enable() {
    let mut h = lock_heap();
    h.set_auto_collect(true);
}

/// Disable threshold-triggered automatic collection.
#[unsafe(no_mangle)]
pub extern "C" fn phx_gc_disable() {
    let mut h = lock_heap();
    h.set_auto_collect(false);
}

/// Tear down the GC: free every tracked allocation and reset the heap.
///
/// Despite the "shutdown" name, this is `replace(heap, fresh)` — the
/// singleton `OnceLock` survives, only its inner `MarkSweepHeap` is
/// swapped out and dropped. A fresh allocation after this call
/// transparently goes through the new empty heap. The name is preserved
/// because it's part of the codegen-emitted C ABI; behavior is reset.
///
/// Called from the generated C `main` wrapper after `phx_main` returns so
/// that compiled binaries terminate without "still reachable" allocations
/// under valgrind. Not part of the steady-state ABI — a fresh allocation
/// after this function would re-initialize the heap correctly.
///
/// # Single-threaded only
///
/// Safe only when no other thread can observe Phoenix-allocated memory.
/// In Phase 2.3 the runtime is single-threaded (shadow stack lives in
/// TLS, mutator runs on the calling thread), so the C `main` wrapper is
/// the sole legitimate caller. If another thread allocated between the
/// `mem::replace` below and `drop(old)`, that thread's freshly-issued
/// pointers would be valid but `drop(old)` would race on the global
/// allocator's free list with whatever Drop is doing — and a thread
/// holding a shadow-stack frame against an `old` allocation would see
/// it freed out from under it. Phase 4.3 (concurrency) revisits this.
///
/// # Leftover shadow-stack frames are harmless
///
/// If the caller has a frame still pushed when shutdown runs (a misuse
/// — the C `main` wrapper pops nothing because it pushed nothing), the
/// frame's root slots point into the freed old-heap allocations.
/// That's not a use-after-free in this function: the slots are *read*
/// only by the next collection, which queries the new heap's
/// `header_for_payload` — and the new heap's `headers` set never
/// contained those addresses, so the lookups return `None` and the
/// stale roots are silently ignored. Subsequent reads via those slots
/// from compiled code remain UB on the user's side, but the GC stays
/// internally consistent.
#[unsafe(no_mangle)]
pub extern "C" fn phx_gc_shutdown() {
    // Invariant: the old heap's `Drop` impl must run *after* the mutex
    // is released. If it ran while the lock was still held and Drop
    // (or the global allocator under it) ever allocated through the
    // GC, we'd deadlock on the same mutex. Today the Drop body only
    // calls `dealloc`, but the scope below makes the order structural
    // — the mutex guard drops at the closing brace, then `_old` drops
    // at the end of the function body. Reverse-lexical drop order
    // would do the wrong thing here, so we let the explicit scope
    // express it.
    let old_heap = {
        let mut h = lock_heap();
        std::mem::replace(&mut *h, MarkSweepHeap::new_singleton())
    };
    // Explicit `drop` makes the ordering structural — `old_heap`'s
    // destructor runs *here*, after the inner block has already released
    // the heap mutex. A future contributor who collapses the inner block
    // would notice immediately: `old_heap` would otherwise be unused
    // outside it. The
    // [`shutdown_releases_lock_before_old_heap_drops`](../../tests/gc_collects.rs)
    // integration test exercises the same invariant from the outside.
    drop(old_heap);
}

/// Approximate live-bytes count after the last collection. Public for
/// tests / benchmarks; not part of the C ABI.
pub fn live_bytes() -> usize {
    lock_heap().live_bytes()
}

/// Override the auto-collect byte threshold. Public for tests /
/// benchmarks that want to force the threshold-driven sweep path
/// without allocating a megabyte first; not part of the C ABI.
pub fn set_collection_threshold(threshold: usize) {
    lock_heap().set_threshold(threshold);
}

/// Total number of live objects tracked by the heap. Public for tests
/// / benchmarks; not part of the C ABI.
pub fn live_objects() -> usize {
    lock_heap().live_objects()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let h = ObjectHeader::new(128, TypeTag::List);
        assert_eq!(h.payload_size(), 128);
        assert_eq!(h.tag(), TypeTag::List);
        assert!(!h.is_marked());

        let mut h = h;
        h.set_mark();
        assert!(h.is_marked());
        assert_eq!(h.payload_size(), 128, "size preserved across mark");
        assert_eq!(h.tag(), TypeTag::List, "tag preserved across mark");

        h.clear_mark();
        assert!(!h.is_marked());
    }

    #[test]
    fn header_size_is_eight() {
        assert_eq!(HEADER_SIZE, 8);
    }

    #[test]
    fn type_tag_round_trip() {
        for tag in [
            TypeTag::Unknown,
            TypeTag::String,
            TypeTag::List,
            TypeTag::Map,
            TypeTag::Closure,
            TypeTag::Struct,
            TypeTag::Enum,
            TypeTag::Dyn,
        ] {
            let h = ObjectHeader::new(0, tag);
            assert_eq!(h.tag(), tag, "tag {:?} round-trips", tag);
        }
    }

    #[test]
    fn out_of_range_tag_defaults_to_unknown() {
        // `from_u32` itself is total: bad tag → Unknown so the
        // conservative scan keeps the GC correct. The codegen-drift
        // assertion lives in `phx_gc_alloc` (the C-ABI entry point),
        // so this test runs in both debug and release builds.
        assert_eq!(TypeTag::from_u32(999), TypeTag::Unknown);
        assert_eq!(TypeTag::from_u32(8), TypeTag::Unknown);
        assert_eq!(TypeTag::from_u32(u32::MAX), TypeTag::Unknown);
    }

    #[test]
    fn string_skips_interior_scan() {
        assert!(!TypeTag::String.scans_interior());
        assert!(TypeTag::Unknown.scans_interior());
        assert!(TypeTag::List.scans_interior());
        assert!(TypeTag::Struct.scans_interior());
    }
}
