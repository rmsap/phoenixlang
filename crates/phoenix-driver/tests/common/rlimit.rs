//! Linux-only `RLIMIT_AS` capping for child processes, shared by every
//! test that needs a runaway child to die on allocation failure instead
//! of thrashing the host: `gc_bounded_memory.rs` (proves the GC collects
//! by capping below the leak-everything footprint) and
//! `gen_schema_fixtures.rs` (contains the parser error-recovery OOM).

use std::os::unix::process::CommandExt;
use std::process::Command;

/// Cap the child's virtual address space at `bytes` before exec. Past the
/// cap, `mmap`/`brk` fail in the child rather than growing without bound;
/// if `setrlimit` itself fails, the spawn fails with the OS error.
pub fn cap_address_space(cmd: &mut Command, bytes: u64) {
    unsafe {
        cmd.pre_exec(move || {
            let rlim = libc::rlimit {
                rlim_cur: bytes,
                rlim_max: bytes,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &rlim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
