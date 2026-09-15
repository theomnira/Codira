//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! Code to perform tests on Codira code.

use codira_compiler::{Config, DisplayColor, PathOrInline, RelativePathBuf};
use codira_runtime::Runtime;

/// The type of test to create
#[derive(Copy, Clone)]
pub enum TestMode {
    /// Compile the code to ensure it compiles and run the `main` function which
    /// should not panic
    CompileAndRun,

    /// Only compile the code to ensure its valid Codira code
    Compile,

    /// Compile the code but it should fail to compile
    ShouldNotCompile,
}

impl TestMode {
    /// Returns true if the Codira code of the test should be compiled
    fn should_compile(self) -> bool {
        matches!(self, TestMode::CompileAndRun | TestMode::Compile)
    }

    /// Returns true if the Codira code should be invoked
    fn should_run(self) -> bool {
        matches!(self, TestMode::CompileAndRun)
    }
}

/// Run a Codira test with the specified `code`.
#[allow(clippy::let_unit_value)]
pub fn run_test(code: &str, mode: TestMode) {
    // Construct a temporary path to store the output files
    let out_dir = tempdir::TempDir::new("codira_test_")
        .expect("could not create temporary directory for test output");

    // Construct a driver to compile the code with
    let (mut driver, file_id) = codira_compiler::Driver::with_file(
        Config {
            out_dir: Some(out_dir.path().to_path_buf()),
            ..Config::default()
        },
        PathOrInline::Inline {
            rel_path: RelativePathBuf::from("mod.code"),
            contents: code.to_owned(),
        },
    )
    .expect("unable to create driver from test input");

    // Check if the code compiles (and whether thats ok)
    let compiler_errors = driver
        .emit_diagnostics_to_string(DisplayColor::Auto)
        .expect("error emitting errors");
    match (compiler_errors, mode.should_compile()) {
        (Some(errors), true) => {
            panic!("code contains compiler errors:\n{errors}");
        }
        (None, false) => {
            panic!("Code that should have caused the error compiled successfully");
        }
        _ => (),
    };

    if !mode.should_run() {
        return;
    }

    // Write the library to the output so we can run it
    driver
        .write_all_assemblies(true)
        .expect("error emitting assemblies");

    // Create a runtime
    let assembly_path = driver.assembly_output_path_from_file(file_id);
    let builder = Runtime::builder(assembly_path);

    // Safety: We compiled the codira code ourselves, therefor loading the codiralib
    // is safe
    let runtime = unsafe { builder.finish() }.expect("error creating runtime for test assembly");

    // Find the main function
    assert!(
        runtime.get_function_definition("main").is_some(),
        "Could not find `main` function"
    );

    // Call the main function
    let _: () = runtime
        .invoke("main", ())
        .expect("error calling main function");
}
