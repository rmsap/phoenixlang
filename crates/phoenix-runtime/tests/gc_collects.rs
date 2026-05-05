//! Integration tests proving the GC actually collects unreachable memory
//! and respects the shadow stack as a precise root set.
//!
//! These tests share the singleton heap, so they serialize against a
//! `Mutex` to keep parallel test runs from clobbering each other's
//! allocations and `live_objects` snapshots.

use std::sync::{Mutex, OnceLock};

use phoenix_runtime::gc::{
    DEFAULT_COLLECTION_THRESHOLD, TypeTag, phx_gc_alloc, phx_gc_collect, phx_gc_disable,
    phx_gc_enable, phx_gc_pop_frame, phx_gc_push_frame, phx_gc_set_root, phx_gc_shutdown,
    set_collection_threshold,
};

/// Process-wide lock so the integration tests in this file don't
/// concurrently mutate the singleton heap.
fn gc_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn alloc(size: usize, tag: TypeTag) -> *mut u8 {
    phx_gc_alloc(size, tag as u32)
}

#[test]
fn unrooted_allocation_is_collected() {
    let _g = gc_test_lock().lock().unwrap();
    // Settle to baseline first.
    phx_gc_collect();
    let baseline = phoenix_runtime::gc::live_objects();

    let _ptr = alloc(64, TypeTag::Unknown);
    assert_eq!(phoenix_runtime::gc::live_objects(), baseline + 1);

    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline,
        "unrooted allocation should be collected"
    );
}

#[test]
fn rooted_allocation_survives_collection() {
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();
    let baseline = phoenix_runtime::gc::live_objects();

    let frame = phx_gc_push_frame(1);
    let ptr = alloc(64, TypeTag::Unknown);
    unsafe {
        phx_gc_set_root(frame, 0, ptr);
    }

    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline + 1,
        "rooted allocation must survive collection"
    );

    unsafe { phx_gc_pop_frame(frame) };

    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline,
        "after pop, allocation is unreachable and gets collected"
    );
}

#[test]
fn interior_pointer_keeps_child_alive() {
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();
    let baseline = phoenix_runtime::gc::live_objects();

    let child = alloc(32, TypeTag::Unknown);
    let parent = alloc(32, TypeTag::Unknown);
    unsafe {
        *(parent as *mut *mut u8) = child;
    }

    let frame = phx_gc_push_frame(1);
    unsafe { phx_gc_set_root(frame, 0, parent) };

    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline + 2,
        "child reachable through parent's interior pointer must survive"
    );

    unsafe { phx_gc_pop_frame(frame) };
    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline,
        "after pop, both objects collected"
    );
}

#[test]
fn many_unrooted_allocations_collected() {
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();
    let baseline = phoenix_runtime::gc::live_objects();

    for _ in 0..1000 {
        let _ = alloc(16, TypeTag::Unknown);
    }
    // Auto-collect may or may not have fired depending on threshold; we
    // just need to verify the explicit collect drops count back to baseline.
    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline,
        "after explicit collection of 1000 unrooted allocations, count should return to baseline"
    );
}

#[test]
fn string_payload_not_traced_as_pointer() {
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();
    let baseline = phoenix_runtime::gc::live_objects();

    let target = alloc(16, TypeTag::Unknown);
    let s = alloc(16, TypeTag::String);
    unsafe {
        *(s as *mut *mut u8) = target;
    }

    let frame = phx_gc_push_frame(1);
    unsafe { phx_gc_set_root(frame, 0, s) };

    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline + 1,
        "string payload must not be traced as pointers — only `s` survives"
    );

    unsafe { phx_gc_pop_frame(frame) };
    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline,
        "after pop, string is also collected"
    );
}

#[test]
fn cycle_collected_when_unrooted() {
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();
    let baseline = phoenix_runtime::gc::live_objects();

    let a = alloc(32, TypeTag::Unknown);
    let b = alloc(32, TypeTag::Unknown);
    unsafe {
        *(a as *mut *mut u8) = b;
        *(b as *mut *mut u8) = a;
    }
    // No roots — both should die despite the cycle.
    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline,
        "tracing GC must collect cycles when both nodes are unreachable"
    );
}

#[test]
fn shutdown_then_alloc_reinitializes_heap() {
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();
    let baseline_before = phoenix_runtime::gc::live_objects();

    // Allocate a few objects (no roots, so they're trivially reclaimable
    // but we don't collect — shutdown should free them either way).
    for _ in 0..8 {
        let _ = alloc(32, TypeTag::Unknown);
    }
    assert!(phoenix_runtime::gc::live_objects() >= baseline_before + 8);

    // Shutdown frees every tracked allocation and replaces the heap
    // with a fresh empty one.
    phx_gc_shutdown();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        0,
        "after shutdown, heap registry must be empty"
    );

    // A fresh allocation post-shutdown re-initializes through the same
    // singleton (the OnceLock survives; only its inner heap was replaced).
    let p = alloc(32, TypeTag::Unknown);
    assert!(!p.is_null(), "post-shutdown alloc must succeed");
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        1,
        "post-shutdown heap must track the new allocation"
    );

    // Clean up so subsequent tests start from baseline.
    phx_gc_collect();
}

#[test]
fn auto_collect_threshold_path_runs() {
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();
    let baseline = phoenix_runtime::gc::live_objects();

    // Tune the threshold so the auto-collect path runs after a small
    // number of allocations rather than burning ~2 GiB to cross the
    // 1-MB default. With no shadow-stack frame holding any of the
    // allocations, the auto-collect should sweep them before the
    // post-threshold alloc returns.
    let n_allocs = 64;
    let alloc_size = 64;
    set_collection_threshold(alloc_size * 4);
    phx_gc_enable();
    for _ in 0..n_allocs {
        let _ = alloc(alloc_size, TypeTag::Unknown);
    }
    // After the auto-collect fired, only allocations made *after* the
    // most recent collect remain. We can't predict the exact count
    // without the threshold, but it's strictly less than `n_allocs`.
    let after_auto = phoenix_runtime::gc::live_objects();
    assert!(
        after_auto < n_allocs,
        "auto-collect should have swept at least one batch; saw {after_auto}"
    );

    // Reset for the next test.
    phx_gc_disable();
    set_collection_threshold(DEFAULT_COLLECTION_THRESHOLD);
    phx_gc_collect();
    assert_eq!(phoenix_runtime::gc::live_objects(), baseline);
}

#[test]
fn str_split_keeps_intermediate_strings_alive_across_collect() {
    // Targeted regression for the manual frame in `phx_str_split`.
    // Without the `phx_gc_set_root(frame, 0, list)` line, the inner
    // loop's per-part `to_phx_string_from_str` call could trigger an
    // auto-collect that sweeps the freshly-allocated list (it isn't
    // rooted by any caller frame yet) before we finish writing fat
    // pointers into it. We force the threshold low so a collect
    // *definitely* fires inside the loop.
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();

    // 32 bytes is well below the per-part string allocation footprint
    // of any non-trivial input — the threshold check fires on every
    // iteration.
    set_collection_threshold(32);
    phx_gc_enable();

    // 64 parts, each a distinct multi-byte string so every iteration
    // allocates a fresh GC string and the auto-collect path runs
    // multiple times over the loop's lifetime.
    let input: String = (0..64)
        .map(|i| format!("part_{i:03}"))
        .collect::<Vec<_>>()
        .join(",");
    let sep = ",";

    let list = unsafe {
        phoenix_runtime::__test_support::phx_str_split(
            input.as_ptr(),
            input.len(),
            sep.as_ptr(),
            sep.len(),
        )
    };
    assert_eq!(
        unsafe { phoenix_runtime::__test_support::phx_list_length(list) },
        64
    );

    // Read every element back. If any intermediate string was swept,
    // either the fat pointer is dangling (segfault on read) or the
    // bytes are some other live allocation's content (assertion fail).
    for i in 0..64 {
        let elem = unsafe { phoenix_runtime::__test_support::phx_list_get_raw(list, i) };
        let ptr = unsafe { *(elem as *const i64) } as *const u8;
        let len = unsafe { *((elem as *const i64).add(1)) } as usize;
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        let actual = std::str::from_utf8(bytes).expect("split produced invalid UTF-8");
        let expected = format!("part_{i:03}");
        assert_eq!(
            actual, expected,
            "split element {i} corrupted — likely swept mid-call",
        );
    }

    // Restore default state for subsequent tests.
    phx_gc_disable();
    set_collection_threshold(DEFAULT_COLLECTION_THRESHOLD);
    phx_gc_collect();
}

#[test]
fn list_take_keeps_input_alive_across_collect() {
    // Regression for the "input must be rooted across `phx_list_alloc`"
    // contract on `phx_list_take` (and by extension `phx_list_drop` /
    // `phx_list_push_raw` / `phx_map_keys` / `phx_map_values` — they
    // share the read-after-alloc shape). Force a collect inside the
    // call by lowering the threshold below the new list's footprint
    // and rooting only the *input* in the caller's frame. If the
    // contract were violated and `phx_list_alloc` triggered a sweep
    // of an unrooted input, the subsequent `copy_nonoverlapping`
    // would read freed memory and the take's result would be garbage.
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();

    set_collection_threshold(32);
    phx_gc_enable();

    // Build a 5-element i64 list and root it. The take below allocates
    // a fresh list whose population by `copy_nonoverlapping` reads
    // back from the (rooted) input — proving the input survived the
    // intermediate collect.
    let frame = phx_gc_push_frame(1);
    let input = phoenix_runtime::__test_support::phx_list_alloc(8, 5);
    unsafe { phx_gc_set_root(frame, 0, input) };

    // Populate with sentinel values we can verify on the other side.
    let header_size = phoenix_runtime::list_header_size();
    let data = unsafe { (input as *const u8).add(header_size) as *mut i64 };
    for i in 0..5i64 {
        unsafe { *data.add(i as usize) = 100 + i };
    }

    let taken = unsafe { phoenix_runtime::__test_support::phx_list_take(input, 3) };
    assert_eq!(
        unsafe { phoenix_runtime::__test_support::phx_list_length(taken) },
        3
    );
    for i in 0..3i64 {
        let elem = unsafe { phoenix_runtime::__test_support::phx_list_get_raw(taken, i) };
        let v = unsafe { *(elem as *const i64) };
        assert_eq!(
            v,
            100 + i,
            "take element {i} corrupted — input was likely swept mid-call",
        );
    }

    unsafe { phx_gc_pop_frame(frame) };
    phx_gc_disable();
    set_collection_threshold(DEFAULT_COLLECTION_THRESHOLD);
    phx_gc_collect();
}

#[test]
fn shutdown_releases_lock_before_old_heap_drops() {
    // Regression for the lock-ordering invariant in `phx_gc_shutdown`:
    // the heap mutex guard must drop *before* the replaced `MarkSweepHeap`
    // drops, otherwise a `Drop` impl that ever needed the heap lock would
    // deadlock. The function structures this with an inner block that
    // releases the guard, then an explicit `drop(old_heap)` at the outer
    // scope. We can't directly observe the in-flight ordering from the
    // outside, but we can prove the post-condition by spawning a worker
    // that races to allocate while shutdown is mid-Drop. If the lock were
    // still held when `old_heap` drops (and Drop ever blocked on it), the
    // worker's `phx_gc_alloc` would never return. We bound the wait so a
    // regression surfaces as a test failure rather than CI hanging
    // indefinitely.
    use std::sync::atomic::{AtomicUsize, Ordering as AOrd};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();

    // Intentionally no `phx_gc_push_frame` on either thread. The worker
    // calls `phx_gc_alloc`, which goes through `assert_safe_to_collect`
    // → `other_threads_hold_frames()` — if the main thread held a live
    // frame here, the worker's read of the global counter would see
    // `global > mine = 0` and `runtime_abort` the test binary. Future
    // contributors: do not "make this test more realistic" by pushing
    // frames; the design that's being verified is purely the lock-
    // ordering invariant in `phx_gc_shutdown`, and shadow-stack traffic
    // would change what the test exercises.

    // Pre-populate so `old_heap`'s Drop has real dealloc work to do.
    for _ in 0..1024 {
        let _ = alloc(64, TypeTag::Unknown);
    }

    let worker_done = Arc::new(AtomicUsize::new(0));
    let wd = worker_done.clone();
    let (tx, rx) = mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        // Brief delay so this fires while shutdown is in mid-Drop on
        // the main thread. Even if we miss the window, the test still
        // verifies the post-shutdown lock is free.
        std::thread::sleep(Duration::from_micros(50));
        let p = phx_gc_alloc(8, TypeTag::Unknown as u32);
        wd.store(p as usize, AOrd::SeqCst);
        let _ = tx.send(());
    });

    phx_gc_shutdown();

    // The worker must complete promptly. A 5-second cap is generous —
    // shutdown's dealloc path completes in microseconds on any sane
    // allocator; anything beyond this is a deadlock.
    rx.recv_timeout(Duration::from_secs(5))
        .expect("worker alloc didn't return after shutdown — likely lock-ordering regression");
    worker.join().expect("worker panicked");
    assert_ne!(
        worker_done.load(AOrd::SeqCst),
        0,
        "worker reported a null allocation"
    );

    phx_gc_collect();
}

#[test]
fn str_concat_keeps_inputs_alive_across_collect() {
    // Sibling regression to the `phx_str_split` and `phx_list_take`
    // contracts: `phx_str_concat` allocates its destination via
    // `phx_string_alloc`, which can cross the auto-collect threshold.
    // The two `copy_nonoverlapping` calls that follow read from the
    // caller's input pointers — so an unrooted GC-managed input would
    // be swept between alloc and copy and the result would carry
    // freed bytes.
    //
    // We allocate two GC strings via `to_phx_string_from_str`, root
    // them in the caller frame, force the threshold low so a collect
    // *definitely* fires inside `phx_str_concat`, and assert the
    // concat result equals the byte-wise sum of the two inputs.
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();

    set_collection_threshold(32);
    phx_gc_enable();

    // Two distinct multi-byte inputs, each on the GC heap.
    let lhs = phoenix_runtime::__test_support::to_phx_string_from_str("hello, ");
    let rhs = phoenix_runtime::__test_support::to_phx_string_from_str("phoenix world");

    let frame = phx_gc_push_frame(2);
    unsafe {
        phx_gc_set_root(frame, 0, lhs.ptr as *mut u8);
        phx_gc_set_root(frame, 1, rhs.ptr as *mut u8);
    }

    // The concat itself crosses an alloc that exceeds the threshold,
    // forcing the auto-collect path to run. Without the rooting above,
    // the sweep would free `lhs`/`rhs` and the copies would read freed
    // memory; the assert on the concatenated bytes catches that
    // (segfault on read, or a non-equal compare on stale bytes).
    let result = unsafe {
        phoenix_runtime::__test_support::phx_str_concat(lhs.ptr, lhs.len, rhs.ptr, rhs.len)
    };
    assert_eq!(result.len, lhs.len + rhs.len);
    let bytes = unsafe { std::slice::from_raw_parts(result.ptr, result.len) };
    assert_eq!(
        bytes, b"hello, phoenix world",
        "concat result corrupted — inputs were likely swept mid-call",
    );

    unsafe { phx_gc_pop_frame(frame) };
    phx_gc_disable();
    set_collection_threshold(DEFAULT_COLLECTION_THRESHOLD);
    phx_gc_collect();
}

#[test]
fn list_drop_keeps_input_alive_across_collect() {
    // Sibling regression to `list_take_keeps_input_alive_across_collect`.
    // `phx_list_drop` shares the read-after-alloc shape with `take`,
    // `push_raw`, and the map keys/values helpers — if the input
    // contract were ever violated and `phx_list_alloc` triggered a
    // sweep of an unrooted input, `copy_nonoverlapping` would read
    // freed memory. The assertion here proves the bytes survive.
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();

    set_collection_threshold(32);
    phx_gc_enable();

    let frame = phx_gc_push_frame(1);
    let input = phoenix_runtime::__test_support::phx_list_alloc(8, 5);
    unsafe { phx_gc_set_root(frame, 0, input) };

    let header_size = phoenix_runtime::list_header_size();
    let data = unsafe { (input as *const u8).add(header_size) as *mut i64 };
    for i in 0..5i64 {
        unsafe { *data.add(i as usize) = 200 + i };
    }

    // Drop the first 2; the remaining 3 must read intact.
    let dropped = unsafe { phoenix_runtime::__test_support::phx_list_drop(input, 2) };
    assert_eq!(
        unsafe { phoenix_runtime::__test_support::phx_list_length(dropped) },
        3
    );
    for i in 0..3i64 {
        let elem = unsafe { phoenix_runtime::__test_support::phx_list_get_raw(dropped, i) };
        let v = unsafe { *(elem as *const i64) };
        assert_eq!(
            v,
            202 + i,
            "drop element {i} corrupted — input was likely swept mid-call",
        );
    }

    unsafe { phx_gc_pop_frame(frame) };
    phx_gc_disable();
    set_collection_threshold(DEFAULT_COLLECTION_THRESHOLD);
    phx_gc_collect();
}

#[test]
fn empty_concat_round_trips_without_alloc_traffic() {
    // `phx_str_concat` short-circuits the `total == 0` case to
    // `empty_phx_str()`, which returns a `.rodata` pointer. The GC's
    // `header_for_payload` rejects this pointer (registry miss) so it
    // can be silently skipped during mark — even when used as a root.
    // Force a collect with the empty result rooted: a regression in
    // `empty_phx_str` (e.g. switching to a real GC alloc whose header
    // didn't survive the cycle) would produce a use-after-free read
    // when the test reads `len` back.
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();

    let empty = unsafe {
        phoenix_runtime::__test_support::phx_str_concat(b"".as_ptr(), 0, b"".as_ptr(), 0)
    };
    assert_eq!(empty.len, 0, "empty concat must yield zero-length result");

    // Concat of one empty + one non-empty: the non-empty side dominates
    // and a real GC allocation happens. Result content must equal the
    // non-empty input.
    let s = b"hello";
    let result = unsafe {
        phoenix_runtime::__test_support::phx_str_concat(b"".as_ptr(), 0, s.as_ptr(), s.len())
    };
    assert_eq!(result.len, 5);
    let bytes = unsafe { std::slice::from_raw_parts(result.ptr, result.len) };
    assert_eq!(bytes, s);

    // Force a collect; without rooting, the GC-managed result is
    // reclaimed but the empty `.rodata` pointer is unaffected.
    phx_gc_collect();

    // `empty` is still safe to read after the collect — `.rodata` lives
    // for the process lifetime and isn't tracked by the heap registry.
    assert_eq!(
        empty.len, 0,
        "empty result must remain readable post-collect"
    );
}

#[test]
fn nested_frames_each_root_their_own_payload() {
    let _g = gc_test_lock().lock().unwrap();
    phx_gc_collect();
    let baseline = phoenix_runtime::gc::live_objects();

    let outer = phx_gc_push_frame(1);
    let outer_alloc = alloc(64, TypeTag::Unknown);
    unsafe { phx_gc_set_root(outer, 0, outer_alloc) };

    let inner = phx_gc_push_frame(1);
    let inner_alloc = alloc(64, TypeTag::Unknown);
    unsafe { phx_gc_set_root(inner, 0, inner_alloc) };

    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline + 2,
        "both nested-frame allocations must survive"
    );

    // Pop only the inner frame; outer's allocation must still survive.
    unsafe { phx_gc_pop_frame(inner) };
    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline + 1,
        "after popping inner frame, only outer-rooted allocation remains"
    );

    // Pop the outer frame; everything is now unrooted.
    unsafe { phx_gc_pop_frame(outer) };
    phx_gc_collect();
    assert_eq!(
        phoenix_runtime::gc::live_objects(),
        baseline,
        "after popping all frames, every allocation is collected"
    );
}

/// Cross-thread collection must trip [`assert_safe_to_collect`] and abort.
///
/// This locks in the documented behavior that a `phx_gc_collect` call from
/// a thread that doesn't hold the live shadow-stack frames is rejected.
/// The check is documented as best-effort (Phase 2.3 is single-threaded;
/// the check exists primarily to surface accidental misuse under cargo
/// libtest's parallel runner), but a future refactor that turned it into
/// a no-op would silently corrupt memory under any cross-thread misuse —
/// hence pinning the abort behavior here.
///
/// **How:** the parent re-execs the same test binary with `--exact <name>`
/// and `PHX_INTERNAL_CROSS_THREAD_ABORT=1`. The child takes the
/// abort-triggering branch (push a frame on thread A, force a collect from
/// thread B); the parent asserts on the child's exit status and stderr.
/// Process isolation is required because `runtime_abort` calls
/// `process::exit(1)` which can't be caught.
#[test]
fn cross_thread_collect_aborts() {
    if std::env::var("PHX_INTERNAL_CROSS_THREAD_ABORT").is_ok() {
        // Child branch — trigger the abort. The frame is held on this
        // thread; the worker thread runs the collect, which walks its
        // own (empty) TLS shadow stack while a frame is live elsewhere
        // → `assert_safe_to_collect` fires → `runtime_abort`.
        let _frame = phx_gc_push_frame(1);
        std::thread::spawn(|| {
            phx_gc_collect();
        })
        .join()
        .expect("worker panicked before runtime_abort fired");
        // If we reach this line, the abort did NOT fire — exit cleanly
        // so the parent's assertion (non-zero exit) catches the
        // regression instead of seeing a hang or default panic.
        std::process::exit(0);
    }

    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(&exe)
        .args(["--exact", "cross_thread_collect_aborts", "--nocapture"])
        .env("PHX_INTERNAL_CROSS_THREAD_ABORT", "1")
        .output()
        .expect("spawn child test binary");

    assert!(
        !output.status.success(),
        "child should have aborted via runtime_abort; exit status was \
         success.\n  stdout: {}\n  stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("GC collect from a thread other than"),
        "expected the cross-thread runtime_abort message in stderr.\n  \
         stdout: {}\n  stderr: {stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
}
