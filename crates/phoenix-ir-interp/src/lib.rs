#![warn(missing_docs)]
//! IR-level interpreter for the Phoenix programming language.
//!
//! Executes a lowered [`IrModule`](phoenix_ir::module::IrModule) directly by
//! walking basic blocks, dispatching instructions, and following terminators.
//! The primary purpose is round-trip verification: running the same program
//! through both the AST interpreter and the IR interpreter and comparing
//! output ensures that IR lowering preserves semantics.
//!
//! # Usage
//!
//! ```no_run
//! # let program = todo!();
//! # let check_result = todo!();
//! let module = phoenix_ir::lower(&program, &check_result);
//! phoenix_ir_interp::run(&module)?;
//! # Ok::<(), phoenix_ir_interp::error::IrRuntimeError>(())
//! ```

mod builtins;
/// Shared error type and helpers.
pub mod error;
/// Core interpreter: instruction dispatch, call frames, execution loop.
pub mod interpreter;
/// Runtime value representation for the IR interpreter.
pub mod value;

use error::IrRuntimeError;
use interpreter::IrInterpreter;
use phoenix_ir::module::IrModule;
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

/// Run the `main` function in the given IR module, printing to stdout.
pub fn run(module: &IrModule) -> Result<(), IrRuntimeError> {
    let mut interp = IrInterpreter::new(module, Box::new(std::io::stdout()));
    interp.run()
}

/// Run the `main` function and capture all `print()` output as lines.
pub fn run_and_capture(module: &IrModule) -> Result<Vec<String>, IrRuntimeError> {
    let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
    let writer = SharedWriter(buffer.clone());
    let mut interp = IrInterpreter::new(module, Box::new(writer));
    interp.run()?;
    let bytes = buffer.borrow();
    let output = String::from_utf8_lossy(&bytes);
    let lines: Vec<String> = output.lines().map(|l| l.to_string()).collect();
    Ok(lines)
}

/// A writer that writes to a shared buffer.
struct SharedWriter(Rc<RefCell<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
