//! Calls a Codira library from Rust, both ways.
//!
//! Rust is the one caller that can use either route, so this example shows
//! both and the reason to pick one:
//!
//! 1. **Direct C ABI** (`libloading`) -- the same thing the C, Python and
//!    TypeScript callers do. No Codira dependency at all; the library is just
//!    a shared object. Limited to `@export("C")` functions that do not call
//!    out to `extern` symbols.
//!
//! 2. **Hosting the runtime** (`codira_runtime`) -- resolves the library's
//!    `extern "C"` dependencies, gives access to *every* public function by
//!    name rather than only the exported ones, and is what hot reloading
//!    runs on.
//!
//!     cargo run -- ../codira/target/mod.codiralib

use std::{env, path::PathBuf, process::ExitCode};

use codira_runtime::Runtime;

/// Accumulates check results so a failure anywhere sets the exit status.
#[derive(Default)]
struct Checks {
    failures: u32,
}

impl Checks {
    fn check<T: PartialEq + std::fmt::Display>(&mut self, label: &str, actual: T, expected: T) {
        let ok = actual == expected;
        if ok {
            println!("  ok   {label} = {actual}");
        } else {
            self.failures += 1;
            println!("  FAIL {label} = {actual} (expected {expected})");
        }
    }
}

fn main() -> ExitCode {
    let Some(library_path) = env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: codira-interop-rust <path to .codiralib>");
        return ExitCode::from(2);
    };

    let mut checks = Checks::default();

    println!("via the C ABI (libloading):");
    if let Err(error) = direct_ffi(&library_path, &mut checks) {
        eprintln!("  direct FFI failed: {error}");
        return ExitCode::FAILURE;
    }

    println!("via the Codira runtime:");
    if let Err(error) = hosted_runtime(&library_path, &mut checks) {
        eprintln!("  hosting the runtime failed: {error}");
        return ExitCode::FAILURE;
    }

    if checks.failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Calls the exported symbols directly, exactly as a C program would.
fn direct_ffi(library_path: &PathBuf, checks: &mut Checks) -> Result<(), libloading::Error> {
    // SAFETY: loading a library runs its initialisers, and calling through a
    // symbol trusts the declared signature. Both are the same trust a C
    // caller extends, and the signatures below match the `@export("C")`
    // declarations in the `.code` source.
    unsafe {
        let library = libloading::Library::new(library_path)?;

        let add: libloading::Symbol<'_, unsafe extern "C" fn(i64, i64) -> i64> =
            library.get(b"codira_add")?;
        let scale: libloading::Symbol<'_, unsafe extern "C" fn(f64, f64) -> f64> =
            library.get(b"codira_scale")?;
        let length_squared: libloading::Symbol<'_, unsafe extern "C" fn(f64, f64) -> f64> =
            library.get(b"codira_length_squared")?;
        let div: libloading::Symbol<'_, unsafe extern "C" fn(i64, i64) -> i64> =
            library.get(b"codira_div")?;
        let modulo: libloading::Symbol<'_, unsafe extern "C" fn(i64, i64) -> i64> =
            library.get(b"codira_mod")?;

        checks.check("codira_add(20, 22)", add(20, 22), 42);
        checks.check("codira_scale(1.5, 4.0)", scale(1.5, 4.0), 6.0);
        checks.check(
            "codira_length_squared(3, 4)",
            length_squared(3.0, 4.0),
            25.0,
        );
        checks.check("codira_div(47, 5)", div(47, 5), 9);
        checks.check("codira_mod(47, 5)", modulo(47, 5), 2);
    }

    Ok(())
}

/// Loads the library into a Codira runtime and invokes functions by name.
///
/// This reaches `codira_hypot`, which the direct route cannot: it calls the
/// `extern "C"` symbol `sqrt`, and resolving that is precisely what the
/// runtime's linking step does.
fn hosted_runtime(library_path: &PathBuf, checks: &mut Checks) -> Result<(), anyhow::Error> {
    // SAFETY: the library is the one this example just built; hosting it
    // means trusting its `get_info` table, the same assumption `codira start`
    // makes.
    let mut runtime = unsafe { Runtime::builder(library_path).finish() }?;

    let hypot: f64 = runtime
        .invoke("codira_hypot", (3.0f64, 4.0f64))
        .unwrap_or_else(|error| error.wait(&mut runtime));
    checks.check("codira_hypot(3, 4)", hypot, 5.0);

    let sum: f64 = runtime
        .invoke("codira_sum_of_squares", (3.0f64, 4.0f64))
        .unwrap_or_else(|error| error.wait(&mut runtime));
    checks.check("codira_sum_of_squares(3, 4)", sum, 25.0);

    Ok(())
}
