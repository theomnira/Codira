//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{
    path::{Path, PathBuf},
    thread::sleep,
    time::{Duration, Instant},
};

use codira_compiler::{Config, DisplayColor, Driver, PathOrInline, RelativePathBuf};
use codira_hir_input::Fixture;
use codira_runtime::{InitError, Runtime, RuntimeBuilder};

/// Implements a compiler that generates and temporarily stores a `*.codiralib`
/// library corresponding to a single source file.
pub struct CompileTestDriver {
    _temp_output_dir: tempfile::TempDir,
    _temp_workspace: Option<tempfile::TempDir>,
    out_path: PathBuf,
    driver: Driver,
}

impl CompileTestDriver {
    /// Constructs a new `CompilerTestDriver` from a fixture that describes an
    /// entire codira project. So it file structure should look something like
    /// this:
    ///
    /// ```text
    /// codira.toml
    /// src/
    ///    mod.code
    /// ```
    pub fn from_fixture(text: &str) -> Self {
        Self::from_fixture_with(text, |_| {})
    }

    /// Like [`CompileTestDriver::from_fixture`], letting `configure` adjust
    /// the compiler driver (for instance [`Driver::set_no_hot_reload`])
    /// before it checks and generates code.
    pub fn from_fixture_with(text: &str, configure: impl FnOnce(&mut Driver)) -> Self {
        let temp_output_dir = tempfile::TempDir::new().unwrap();
        let config = Config {
            out_dir: Some(temp_output_dir.path().to_path_buf()),
            ..Config::default()
        };

        // Write the contents of the fixture to a temporary directory
        let temp_source_dir = tempfile::TempDir::new().unwrap();
        for entry in Fixture::parse(text) {
            let path = entry.relative_path.to_path(temp_source_dir.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, entry.text).unwrap();
        }

        // Initialize the driver from the fixture content
        let (_, mut driver) =
            Driver::with_package_path(temp_source_dir.path().join("codira.toml"), config).unwrap();
        configure(&mut driver);
        if let Some(compiler_errors) = driver
            .emit_diagnostics_to_string(DisplayColor::Disable)
            .expect("could not create diagnostics")
        {
            panic!("compiler errors:\n{compiler_errors}")
        }

        driver.write_all_assemblies(true).unwrap();
        let out_path = temp_output_dir.path().join("mod.codiralib");

        CompileTestDriver {
            _temp_output_dir: temp_output_dir,
            _temp_workspace: Some(temp_source_dir),
            driver,
            out_path,
        }
    }

    /// Constructs a new `CompileTestDriver` from a single Codira source.
    pub fn from_file(text: &str) -> Self {
        Self::from_file_with(text, |_| {})
    }

    /// Constructs a new `CompileTestDriver` from a single Codira source,
    /// letting `configure` adjust the compiler driver before it generates
    /// code.
    pub fn from_file_with(text: &str, configure: impl FnOnce(&mut Driver)) -> Self {
        Self::from_file_with_config(text, Config::default(), configure)
    }

    /// Like [`CompileTestDriver::from_file_with`], with `config` (for
    /// instance another optimization level) instead of the default; its
    /// output directory is replaced by a temporary one.
    pub fn from_file_with_config(
        text: &str,
        config: Config,
        configure: impl FnOnce(&mut Driver),
    ) -> Self {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = Config {
            out_dir: Some(temp_dir.path().to_path_buf()),
            ..config
        };
        let input = PathOrInline::Inline {
            rel_path: RelativePathBuf::from("mod.code"),
            contents: text.to_owned(),
        };
        let (mut driver, file_id) = Driver::with_file(config, input).unwrap();
        configure(&mut driver);
        if let Some(compiler_errors) = driver
            .emit_diagnostics_to_string(DisplayColor::Disable)
            .expect("could not generate compiler diagnostics")
        {
            panic!("compiler errors:\n{compiler_errors}")
        }

        driver.write_all_assemblies(true).unwrap();
        let out_path = driver.assembly_output_path_from_file(file_id);

        CompileTestDriver {
            _temp_output_dir: temp_dir,
            _temp_workspace: None,
            driver,
            out_path,
        }
    }

    /// Updates the text of the Codira source and ensures that the generated
    /// assembly has been recompiled.
    pub fn update_file(&mut self, path: impl AsRef<codira_paths::RelativePath>, text: &str) {
        self.driver.set_file_text(path, text).unwrap();

        let compiler_errors = self
            .driver
            .emit_diagnostics_to_string(DisplayColor::Disable)
            .expect("error creating diagnostics");
        if let Some(compiler_errors) = compiler_errors {
            panic!("compiler errors:\n{compiler_errors}")
        }
        self.driver.write_all_assemblies(true).unwrap();
    }

    /// The `codira build --explain-alloc` report of the compiled source.
    pub fn allocation_report(&self) -> String {
        self.driver.allocation_report()
    }

    /// Returns the path to the generated `*.codiralib` library.
    pub fn lib_path(&self) -> &Path {
        &self.out_path
    }
}

impl std::fmt::Debug for CompileTestDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompilerTestDriver")
            .field("_temp_dir", &self._temp_output_dir)
            .field("out_path", &self.out_path)
            .finish()
    }
}

/// Implements a compiler that generates, temporarily stores, and hot reloads a
/// `*.codiralib` library corresponding to a single source file.
///
/// This allows testing of Codira constructs that depend on hot-reloading.
pub struct CompileAndRunTestDriver {
    driver: CompileTestDriver,

    /// The runtime created by this instance.
    pub runtime: Runtime,
}

impl std::fmt::Debug for CompileAndRunTestDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompileAndRunTestDriver")
            .field("driver", &self.driver)
            .finish()
    }
}

impl CompileAndRunTestDriver {
    /// The `codira build --explain-alloc` report of the compiled source.
    pub fn allocation_report(&self) -> String {
        self.driver.allocation_report()
    }

    /// Constructs a `CompileAndRunTestDriver` from a single Codira source file
    /// and a `config_fn` that allows modification of a [`RuntimeBuilder`].
    pub fn from_fixture(
        fixture: &str,
        config_fn: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder,
    ) -> Result<Self, InitError> {
        let driver = CompileTestDriver::from_fixture(fixture);
        let builder = Runtime::builder(driver.lib_path());

        // Safety: We compiled the library ourselves, therefor loading the codiralib is
        // safe.
        let build = config_fn(builder);
        let runtime = unsafe { build.finish() }?;

        Ok(Self { driver, runtime })
    }

    /// Constructs a `CompileAndRunTestDriver` from a single Codira source file
    /// and a `config_fn` that allows modification of a [`RuntimeBuilder`].
    pub fn new(
        text: &str,
        config_fn: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder,
    ) -> Result<Self, InitError> {
        let driver = CompileTestDriver::from_file(text);
        let builder = Runtime::builder(driver.lib_path());

        // Safety: We compiled the library ourselves, therefor loading the codiralib is
        // safe.
        let build = config_fn(builder);
        let runtime = unsafe { build.finish() }?;

        Ok(Self { driver, runtime })
    }

    /// Like [`CompileAndRunTestDriver::from_fixture`], letting `configure`
    /// adjust the compiler driver first.
    pub fn from_fixture_with_driver(
        fixture: &str,
        configure: impl FnOnce(&mut Driver),
    ) -> Result<Self, InitError> {
        let driver = CompileTestDriver::from_fixture_with(fixture, configure);
        // Safety: We compiled the library ourselves, therefor loading the codiralib is
        // safe.
        let runtime = unsafe { Runtime::builder(driver.lib_path()).finish() }?;
        Ok(Self { driver, runtime })
    }

    /// Like [`CompileAndRunTestDriver::new`], but compiles with fuel
    /// instrumentation (`codira build --fuel`).
    pub fn with_fuel_instrumentation(
        text: &str,
        config_fn: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder,
    ) -> Result<Self, InitError> {
        let driver = CompileTestDriver::from_file_with(text, |d| d.set_fuel_instrumentation(true));
        let builder = Runtime::builder(driver.lib_path());

        // Safety: We compiled the library ourselves, therefor loading the codiralib is
        // safe.
        let build = config_fn(builder);
        let runtime = unsafe { build.finish() }?;

        Ok(Self { driver, runtime })
    }

    /// The path of the entry library.
    pub fn lib_path(&self) -> &Path {
        self.driver.lib_path()
    }

    /// Like [`CompileAndRunTestDriver::new`], but compiles with frame
    /// allocation on or off (`codira build --frame-alloc`).
    pub fn with_frame_allocation(
        text: &str,
        enabled: bool,
        config_fn: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder,
    ) -> Result<Self, InitError> {
        let driver = CompileTestDriver::from_file_with(text, |d| d.set_frame_allocation(enabled));
        let builder = Runtime::builder(driver.lib_path());

        // Safety: We compiled the library ourselves, therefor loading the codiralib is
        // safe.
        let build = config_fn(builder);
        let runtime = unsafe { build.finish() }?;

        Ok(Self { driver, runtime })
    }

    /// Updates the text of the Codira source and ensures that the generated
    /// assembly has been reloaded.
    ///
    /// A reference to the borrowed `runtime` is used as an argument to allow
    /// moving of the existing borrow inside the update function. This
    /// obviates the necessity for `update` to use the `Runtime`.
    pub fn update_file(&mut self, path: impl AsRef<codira_paths::RelativePath>, text: &str) {
        self.driver.update_file(path, text);

        let start_time = Instant::now();

        // Safety: We compiled the library ourselves, therefor updating the runtime is
        // safe.
        while !unsafe { self.runtime.update() } {
            let now = Instant::now();
            if now - start_time > Duration::from_secs(10) {
                panic!("runtime did not update after recompilation within 10 seconds");
            } else {
                sleep(Duration::from_millis(1));
            }
        }
    }
}
