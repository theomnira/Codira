//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! `Driver` is a stateful compiler frontend that enables incremental
//! compilation by retaining state from previous compilation.

use codira_codegen::{
    AssemblyIr, CodeGenDatabase, EidosOptions, ModuleGroup, ModuleGroupId, TargetAssembly,
};
use codira_hir::{AstDatabase, DiagnosticSink, Module};
use codira_hir_input::{FileId, PackageSet, SourceDatabase, SourceRoot, SourceRootId};
use codira_paths::RelativePathBuf;

use crate::{
    compute_source_relative_path, db::CompilerDatabase, ensure_package_output_dir, is_source_file,
    PathOrInline, RelativePath,
};

mod config;
mod display_color;

use std::{
    collections::{HashMap, HashSet},
    convert::TryInto,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use codira_project::{Package, LOCKFILE_NAME};

/// How long to wait between attempts to take the output-directory lock.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// How many attempts before giving up and reporting the lock as stuck.
///
/// Bounded on purpose -- see `Driver::acquire_filesystem_output_lock`. The
/// total (30 seconds) is long enough to outlast any realistic concurrent
/// build's write phase, and short enough that a wedged directory reports
/// itself instead of consuming a CI job's entire timeout.
const LOCK_ACQUIRE_ATTEMPTS: u32 = 60;
use walkdir::WalkDir;

pub use self::{config::Config, display_color::DisplayColor};
use crate::diagnostics_snippets::{display_path, emit_hir_diagnostic, emit_syntax_error};

pub const WORKSPACE: SourceRootId = SourceRootId(0);

/// The source root of the standard-library files a program imports (see
/// `crate::std_library`).
pub const STD_SOURCE_ROOT: SourceRootId = SourceRootId(1);

/// Which compiler phase produced a diagnostic.
///
/// Worth distinguishing because the two say different things about the
/// compiler: a syntax error is a parser that cannot read the language,
/// while a semantic error is a parser that could and a type system that
/// then objected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticKind {
    Syntax,
    Semantic,
}

impl std::fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiagnosticKind::Syntax => f.write_str("syntax"),
            DiagnosticKind::Semantic => f.write_str("semantic"),
        }
    }
}

/// A single diagnostic, reduced to what a tool needs to report or compare it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckDiagnostic {
    pub kind: DiagnosticKind,
    /// 1-based line number, as a human would cite it.
    pub line: u32,
    pub message: String,
    /// The source text the diagnostic points at, as written (empty for a
    /// syntax error): what an `undefined value` is the undefined value of.
    pub highlight: String,
}

/// Aggregate counts produced by a single diagnostic pass.
///
/// Exists so a caller that wants both a human-rendered report and the
/// summary counts derived from it does not have to walk every module's
/// diagnostics twice to get them -- see `Driver::emit_diagnostics_with_counts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiagnosticCounts {
    pub total_files: usize,
    pub clean_files: usize,
    pub syntax: usize,
    pub semantic: usize,
}

/// What `Driver::sync_source_directory` found to be different on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncSummary {
    /// Files on disk the driver did not know about.
    pub added: usize,
    /// Files whose contents differ from what the driver last saw.
    pub updated: usize,
    /// Files the driver knew about that are no longer on disk.
    pub removed: usize,
}

impl SyncSummary {
    /// The number of files that changed in any way.
    pub fn total(&self) -> usize {
        self.added + self.updated + self.removed
    }

    /// Whether the driver already matched the disk.
    pub fn is_unchanged(&self) -> bool {
        self.total() == 0
    }
}

/// Every diagnostic belonging to one source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiagnostics {
    /// Path relative to the checked source directory, using `/` separators.
    pub relative_path: String,
    pub diagnostics: Vec<CheckDiagnostic>,
}

impl FileDiagnostics {
    /// Whether this file is free of diagnostics.
    pub fn is_clean(&self) -> bool {
        self.diagnostics.is_empty()
    }

    /// The number of diagnostics of a given kind.
    pub fn count_of(&self, kind: DiagnosticKind) -> usize {
        self.diagnostics.iter().filter(|d| d.kind == kind).count()
    }
}

pub struct Driver {
    db: CompilerDatabase,
    out_dir: PathBuf,

    source_root: SourceRoot,
    path_to_file_id: HashMap<RelativePathBuf, FileId>,
    file_id_to_path: HashMap<FileId, RelativePathBuf>,
    next_file_id: usize,

    module_to_temp_assembly_path: HashMap<Module, PathBuf>,

    emit_ir: bool,

    /// The standard-library files loaded for the program's imports, by path
    /// relative to the standard library's directory, and the source root
    /// they form (see `Driver::refresh_std_package`).
    std_files: std::collections::BTreeMap<String, FileId>,
    std_source_root: SourceRoot,
    /// Whether the package set includes the standard library.
    std_loaded: bool,
    /// Whether every library written gets a proof certificate
    /// ([`Driver::set_certify`]).
    certify: bool,
    /// Whether every library written has its constant-time functions'
    /// machine code checked ([`Driver::set_check_constant_time`]).
    check_constant_time: bool,
    /// Whether every library written is superoptimised
    /// ([`Driver::set_superoptimize`]).
    superoptimize: bool,
    /// The directory checked by `with_source_directory` is the standard
    /// library itself, whose `root` it already is.
    is_std_itself: bool,
}

impl Driver {
    /// Constructs a driver with a specific configuration.
    /// The salsa database backing this driver.
    ///
    /// Exposed so tooling can run individual queries -- profilers, IDE
    /// integrations, and anything that needs to ask the compiler a question
    /// without driving a whole build.
    pub fn db(&self) -> &CompilerDatabase {
        &self.db
    }

    pub fn with_config(config: Config, out_dir: PathBuf) -> Self {
        Self {
            db: CompilerDatabase::new(&config),
            out_dir,
            source_root: SourceRoot::default(),
            path_to_file_id: HashMap::default(),
            file_id_to_path: HashMap::default(),
            next_file_id: 0,
            module_to_temp_assembly_path: HashMap::default(),
            emit_ir: config.emit_ir,
            std_files: std::collections::BTreeMap::new(),
            std_source_root: SourceRoot::default(),
            std_loaded: false,
            certify: false,
            check_constant_time: false,
            superoptimize: false,
            is_std_itself: false,
        }
    }

    /// Constructs a driver with a configuration and a single file.
    pub fn with_file(config: Config, path: PathOrInline) -> anyhow::Result<(Driver, FileId)> {
        let out_dir = config.out_dir.clone().unwrap_or_else(|| {
            std::env::current_dir().expect("could not determine current working directory")
        });

        let mut driver = Driver::with_config(config, out_dir);

        // Get the path and contents of the path
        let (rel_path, text) = match path {
            PathOrInline::Path(p) => (
                RelativePathBuf::from_path("mod.code").unwrap(),
                std::fs::read_to_string(p)?,
            ),
            PathOrInline::Inline { rel_path, contents } => (rel_path, contents),
        };

        // Store the file information in the database together with the source root
        let file_id = FileId(driver.next_file_id as u32);
        driver.next_file_id += 1;
        driver.db.set_file_text(file_id, Arc::from(text));
        driver.db.set_file_source_root(file_id, WORKSPACE);
        driver.source_root.insert_file(file_id, rel_path.clone());
        driver
            .db
            .set_source_root(WORKSPACE, Arc::new(driver.source_root.clone()));

        let mut package_set = PackageSet::default();
        package_set.add_package(WORKSPACE);
        driver.db.set_packages(Arc::new(package_set));

        driver.path_to_file_id.insert(rel_path, file_id);
        driver.refresh_std_package()?;

        Ok((driver, file_id))
    }

    /// Constructs a driver with a package manifest directory
    pub fn with_package_path<P: AsRef<Path>>(
        package_path: P,
        config: Config,
    ) -> Result<(Package, Driver), anyhow::Error> {
        // Load the manifest file as a package
        let package = Package::from_file(package_path)?;

        // Determine output directory
        let output_dir = ensure_package_output_dir(&package, &config)
            .map_err(|e| anyhow::anyhow!("could not create package output directory: {e}"))?;

        let native_output_dir = output_dir.join("native");
        let target = config.target.clone();

        // Construct the driver
        let mut driver = Driver::with_config(config, output_dir);
        driver.apply_eidos_settings(package.manifest().eidos());

        // Compile the package's C, C++ and assembly, if it has any. This
        // happens once, when the package is loaded: a watch or a daemon
        // session does not see a change to a native source until the
        // package is loaded again.
        let native = package.manifest().native();
        if !native.is_empty() {
            let inputs = crate::native_build::compile_native_sources(
                package.root(),
                native,
                &target,
                &native_output_dir,
            )?;
            driver.db.set_native_inputs(Arc::new(inputs));
        }

        // Iterate over all files in the source directory of the package and store their
        // information in the database
        let source_directory = package.source_directory();
        if !source_directory.is_dir() {
            anyhow::bail!("the source directory does not exist")
        }

        for source_file_path in iter_source_files(&source_directory) {
            let relative_path = compute_source_relative_path(&source_directory, &source_file_path)?;

            // Load the contents of the file
            let file_contents = std::fs::read_to_string(&source_file_path).map_err(|e| {
                anyhow::anyhow!(
                    "could not read contents of '{}': {}",
                    source_file_path.display(),
                    e
                )
            })?;

            let file_id = driver.alloc_file_id(&relative_path)?;
            driver.db.set_file_text(file_id, Arc::from(file_contents));
            driver.db.set_file_source_root(file_id, WORKSPACE);
            driver
                .source_root
                .insert_file(file_id, relative_path.clone());
        }

        // Store the source root in the database
        driver
            .db
            .set_source_root(WORKSPACE, Arc::new(driver.source_root.clone()));

        let mut package_set = PackageSet::default();
        package_set.add_package(WORKSPACE);
        driver.db.set_packages(Arc::new(package_set));
        driver.refresh_std_package()?;

        Ok((package, driver))
    }

    /// Constructs a driver over every `.code` file under `source_directory`,
    /// with no manifest involved.
    ///
    /// `with_package_path` is the build path: it needs a `codira.toml` because
    /// it is going to *write* a `.codiralib`, and the manifest is what names
    /// it. Checking writes nothing, so requiring a manifest would only mean
    /// that a directory of sources -- the standard library being the case
    /// that matters -- could not be type-checked at all without first being
    /// dressed up as a package it is not.
    pub fn with_source_directory<P: AsRef<Path>>(
        source_directory: P,
        config: Config,
    ) -> Result<Driver, anyhow::Error> {
        let source_directory = source_directory.as_ref();
        if !source_directory.is_dir() {
            anyhow::bail!(
                "'{}' is not a directory of Codira sources",
                source_directory.display()
            );
        }

        // Checking produces no artifacts, so the output directory is never
        // written to; it only has to be a path the driver can hold.
        let out_dir = config
            .out_dir
            .clone()
            .unwrap_or_else(|| source_directory.to_path_buf());
        let mut driver = Driver::with_config(config, out_dir);

        for source_file_path in iter_source_files(source_directory) {
            let relative_path = compute_source_relative_path(source_directory, &source_file_path)?;

            let file_contents = std::fs::read_to_string(&source_file_path).map_err(|e| {
                anyhow::anyhow!(
                    "could not read contents of '{}': {}",
                    source_file_path.display(),
                    e
                )
            })?;

            let file_id = driver.alloc_file_id(&relative_path)?;
            driver.db.set_file_text(file_id, Arc::from(file_contents));
            driver.db.set_file_source_root(file_id, WORKSPACE);
            driver
                .source_root
                .insert_file(file_id, relative_path.clone());
        }

        driver
            .db
            .set_source_root(WORKSPACE, Arc::new(driver.source_root.clone()));

        let mut package_set = PackageSet::default();
        package_set.add_package(WORKSPACE);
        driver.db.set_packages(Arc::new(package_set));

        // Checking the standard library's own directory: it is the
        // workspace package already, and `root` in it is its root.
        driver.is_std_itself = crate::std_library::std_library_dir()
            .ok()
            .zip(source_directory.canonicalize().ok())
            .is_some_and(|(std_dir, dir)| std_dir.canonicalize().ok() == Some(dir));
        driver.refresh_std_package()?;

        Ok(driver)
    }
}

impl Driver {
    /// Applies a package's `[eidos]` table on top of the build mode the
    /// driver was configured with. Keys the table leaves out keep the
    /// compiler's defaults.
    ///
    /// Only an actual change is written: the options are a salsa input,
    /// and setting one -- even to the value it already has -- invalidates
    /// everything compiled from it.
    pub fn apply_eidos_settings(&mut self, settings: &codira_project::EidosSettings) {
        let current = self.db.eidos_options();
        let defaults = EidosOptions::default();
        let options = EidosOptions {
            mode: current.mode,
            max_egraph_nodes_per_region: settings
                .max_egraph_nodes_per_region
                .unwrap_or(defaults.max_egraph_nodes_per_region),
            max_egraph_iterations: settings
                .max_egraph_iterations
                .unwrap_or(defaults.max_egraph_iterations),
            exact_extraction_budget: settings
                .exact_extraction_budget
                .unwrap_or(defaults.exact_extraction_budget),
        };
        if options != current {
            self.db.set_eidos_options(options);
        }
    }

    /// Turns proof certificates on or off (`codira build --certify`): every
    /// library written gets a `<library>.cert.json` beside it with each
    /// function's code and source hashes and its verified obligations as
    /// SMT-LIB2 queries (see `codira_certificate`). Functions keep their
    /// symbols in the library so the certificate can name their code.
    pub fn set_certify(&mut self, enabled: bool) {
        self.certify = enabled;
        self.update_keep_function_symbols();
    }

    /// Turns the machine-code constant-time check on or off (`codira build
    /// --check-constant-time`): after a library is linked, the machine code
    /// of each of its `oblivious` / `@constant_time` functions is
    /// disassembled and checked (see `codira_ctcheck`), and a function that
    /// breaks a rule fails the build. Functions keep their symbols in the
    /// library so their code can be found.
    pub fn set_check_constant_time(&mut self, enabled: bool) {
        self.check_constant_time = enabled;
        self.update_keep_function_symbols();
    }

    /// Turns the experimental superoptimiser on or off (`codira build
    /// --superoptimize`): after a library is linked, the straight-line
    /// integer tail of each function is searched for a cheaper, proven-equal
    /// replacement (see `crate::superopt`). Functions keep their symbols in
    /// the library so their code can be found.
    pub fn set_superoptimize(&mut self, enabled: bool) {
        self.superoptimize = enabled;
        self.update_keep_function_symbols();
    }

    fn update_keep_function_symbols(&mut self) {
        let keep = self.certify || self.check_constant_time || self.superoptimize;
        if self.db.keep_function_symbols() != keep {
            self.db.set_keep_function_symbols(keep);
        }
    }

    /// Builds the package without hot reload (`codira build
    /// --no-hot-reload`): every module goes into one library, so a call from
    /// one module to another is a direct call and not a dispatch-table
    /// lookup. Runtime services (allocation, printing, tasks) are still
    /// reached through the runtime's table.
    pub fn set_no_hot_reload(&mut self, enabled: bool) {
        if self.db.no_hot_reload() != enabled {
            self.db.set_no_hot_reload(enabled);
        }
    }

    /// Sets the experimental hardware-safety mode for the code this driver
    /// generates (`codira build --sanitize=memtag`). Whether the target can
    /// honour it is checked when the assembly is built.
    pub fn set_hardware_safety(&mut self, safety: codira_codegen::HardwareSafety) {
        if self.db.hardware_safety() != safety {
            self.db.set_hardware_safety(safety);
        }
    }

    /// Turns fuel instrumentation on or off for the code this driver
    /// generates (`codira build --fuel`): a counter decrement at every
    /// function entry and loop back-edge.
    pub fn set_fuel_instrumentation(&mut self, enabled: bool) {
        if self.db.fuel_instrumentation() != enabled {
            self.db.set_fuel_instrumentation(enabled);
        }
    }

    /// The build mode and e-graph bounds this driver compiles with.
    pub fn eidos_options(&self) -> EidosOptions {
        self.db.eidos_options()
    }
}

impl Driver {
    /// Returns a file id for the file with the given `relative_path`. This
    /// function reuses `FileId`'s for paths to keep the cache as valid as
    /// possible.
    ///
    /// The allocation of an id might fail if more file IDs exist than can be
    /// allocated.
    pub fn alloc_file_id<P: AsRef<RelativePath>>(
        &mut self,
        relative_path: P,
    ) -> Result<FileId, anyhow::Error> {
        // Re-use existing id to get better caching performance
        if let Some(id) = self.path_to_file_id.get(relative_path.as_ref()) {
            return Ok(*id);
        }

        // Allocate a new id
        // TODO: See if we can figure out if the compiler cleared the cache of a certain
        // file, at  which point we can sort of reset the `next_file_id`
        let id = FileId(
            self.next_file_id
                .try_into()
                .map_err(|_e| anyhow::anyhow!("too many active source files"))?,
        );
        self.next_file_id += 1;

        // Update bookkeeping
        self.path_to_file_id
            .insert(relative_path.as_ref().to_relative_path_buf(), id);
        self.file_id_to_path
            .insert(id, relative_path.as_ref().to_relative_path_buf());

        Ok(id)
    }
}

impl Driver {
    /// Sets the contents of a specific file.
    pub fn set_file_text(
        &mut self,
        path: impl AsRef<RelativePath>,
        text: impl AsRef<str>,
    ) -> anyhow::Result<()> {
        let file_id = self
            .path_to_file_id
            .get(path.as_ref())
            .ok_or_else(|| anyhow::anyhow!("the path '{}' is unknown", path.as_ref()))?;
        self.db
            .set_file_text(*file_id, Arc::from(text.as_ref().to_owned()));
        Ok(())
    }
}

impl Driver {
    /// Every HIR diagnostic of `module`, plus the ones that depend on how
    /// this driver builds: a fuel-instrumented build cannot keep the promise
    /// of a `@nopanic` or `@realtime` function.
    fn module_diagnostics(&self, module: Module, sink: &mut DiagnosticSink<'_>) {
        module.diagnostics(&self.db, sink);
        if self.db.fuel_instrumentation() {
            codira_hir::realtime::fuel_diagnostics(&self.db, module, sink);
        }
    }

    /// Emits all diagnostic messages currently in the database; returns true if
    /// errors were emitted.
    pub fn emit_diagnostics(
        &self,
        writer: &mut dyn std::io::Write,
        display_color: DisplayColor,
    ) -> Result<bool, anyhow::Error> {
        let emit_colors = display_color.should_enable();
        let mut has_error = false;

        for package in codira_hir::Package::all(&self.db) {
            for module in package.modules(&self.db) {
                if let Some(file_id) = module.file_id(&self.db) {
                    let parse = self.db.parse(file_id);
                    let source_code = self.db.file_text(file_id);
                    let relative_file_path = display_path(&self.db, file_id);
                    let line_index = self.db.line_index(file_id);

                    // Emit all syntax diagnostics
                    let mut module_errors = 0;
                    for syntax_error in parse.errors().iter() {
                        emit_syntax_error(
                            syntax_error,
                            relative_file_path.as_str(),
                            &source_code,
                            &line_index,
                            emit_colors,
                            writer,
                        )?;
                        has_error = true;
                        module_errors += 1;
                    }

                    // Emit all HIR diagnostics
                    let mut error = None;
                    self.module_diagnostics(
                        module,
                        &mut DiagnosticSink::new(|d| {
                            has_error = true;
                            module_errors += 1;
                            if let Err(e) =
                                emit_hir_diagnostic(d, &self.db, file_id, emit_colors, writer)
                            {
                                error = Some(e);
                            };
                        }),
                    );

                    // If an error occurred when emitting HIR diagnostics, return early with the
                    // error.
                    if let Some(e) = error {
                        return Err(e.into());
                    }
                    self.report_std_module_errors(file_id, module_errors, writer)?;
                }
            }
        }

        Ok(has_error)
    }

    /// Renders every diagnostic like `emit_diagnostics`, and additionally
    /// returns the counts a summary line needs.
    ///
    /// A caller that wants both pretty output and a "N of M files clean"
    /// summary previously got that by calling `collect_diagnostics` (a full
    /// structured pass) *and* `emit_diagnostics` (a full rendering pass) --
    /// each one independently re-running every module's diagnostic
    /// validators, since only `infer()` itself is salsa-memoized and the
    /// validators that turn its result into diagnostics are not. On this
    /// repository's own standard library (77 files, ~3000 diagnostics) that
    /// second pass measured as the single largest phase in `codira check`,
    /// ahead of type inference. This does the walk once.
    pub fn emit_diagnostics_with_counts(
        &self,
        writer: &mut dyn std::io::Write,
        display_color: DisplayColor,
    ) -> Result<DiagnosticCounts, anyhow::Error> {
        let emit_colors = display_color.should_enable();
        let mut counts = DiagnosticCounts::default();

        for package in codira_hir::Package::all(&self.db) {
            for module in package.modules(&self.db) {
                let Some(file_id) = module.file_id(&self.db) else {
                    continue;
                };
                counts.total_files += 1;

                let parse = self.db.parse(file_id);
                let source_code = self.db.file_text(file_id);
                let relative_file_path = display_path(&self.db, file_id);
                let line_index = self.db.line_index(file_id);

                let mut file_has_diagnostic = false;
                let mut module_errors = 0;

                for syntax_error in parse.errors().iter() {
                    emit_syntax_error(
                        syntax_error,
                        relative_file_path.as_str(),
                        &source_code,
                        &line_index,
                        emit_colors,
                        writer,
                    )?;
                    counts.syntax += 1;
                    module_errors += 1;
                    file_has_diagnostic = true;
                }

                let mut error = None;
                self.module_diagnostics(
                    module,
                    &mut DiagnosticSink::new(|d| {
                        counts.semantic += 1;
                        module_errors += 1;
                        file_has_diagnostic = true;
                        if let Err(e) =
                            emit_hir_diagnostic(d, &self.db, file_id, emit_colors, writer)
                        {
                            error = Some(e);
                        }
                    }),
                );
                if let Some(e) = error {
                    return Err(e.into());
                }
                self.report_std_module_errors(file_id, module_errors, writer)?;

                if !file_has_diagnostic {
                    counts.clean_files += 1;
                }
            }
        }

        Ok(counts)
    }

    /// After the diagnostics of a standard-library file, says which module
    /// they belong to and why the program cannot use it: a program can only
    /// import standard-library modules that check clean (along with every
    /// module they import).
    fn report_std_module_errors(
        &self,
        file_id: FileId,
        errors: usize,
        writer: &mut dyn std::io::Write,
    ) -> Result<(), anyhow::Error> {
        if errors == 0 || self.db.file_source_root(file_id) != STD_SOURCE_ROOT {
            return Ok(());
        }
        let relative = self.db.file_relative_path(file_id);
        let module = crate::std_library::module_path_of(relative.as_str()).join(".");
        writeln!(
            writer,
            "error: the standard-library module `std.{module}` ({}) has {errors} error{}; \
             the program imports it, directly or through another standard-library module, \
             and only modules that check clean can be imported\n",
            display_path(&self.db, file_id),
            if errors == 1 { "" } else { "s" },
        )?;
        Ok(())
    }

    /// Collects every diagnostic in the database as structured data, one
    /// entry per source file, sorted by path.
    ///
    /// `emit_diagnostics` renders directly to a writer, which is right for a
    /// terminal and useless to anything that needs to *count* or *compare*
    /// diagnostics -- a ratchet test, a CI gate, an IDE. Those callers would
    /// otherwise have to scrape rendered snippets, so they get the data
    /// before it becomes text.
    pub fn collect_diagnostics(&self) -> Vec<FileDiagnostics> {
        let mut per_file: Vec<FileDiagnostics> = Vec::new();

        for package in codira_hir::Package::all(&self.db) {
            for module in package.modules(&self.db) {
                let Some(file_id) = module.file_id(&self.db) else {
                    continue;
                };

                let parse = self.db.parse(file_id);
                let line_index = self.db.line_index(file_id);
                let relative_path = display_path(&self.db, file_id).to_string();
                let text = self.db.file_text(file_id);

                let mut diagnostics: Vec<CheckDiagnostic> = parse
                    .errors()
                    .iter()
                    .map(|syntax_error| CheckDiagnostic {
                        kind: DiagnosticKind::Syntax,
                        line: line_index.line_col(syntax_error.location().offset()).line + 1,
                        message: syntax_error.to_string(),
                        highlight: String::new(),
                    })
                    .collect();

                self.module_diagnostics(
                    module,
                    &mut DiagnosticSink::new(|d| {
                        diagnostics.push(CheckDiagnostic {
                            kind: DiagnosticKind::Semantic,
                            line: line_index.line_col(d.highlight_range().start()).line + 1,
                            message: d.message(),
                            highlight: if d.source().file_id == file_id {
                                text.get(std::ops::Range::<usize>::from(d.highlight_range()))
                                    .unwrap_or_default()
                                    .to_owned()
                            } else {
                                String::new()
                            },
                        });
                    }),
                );

                diagnostics.sort_by(|a, b| a.line.cmp(&b.line).then(a.message.cmp(&b.message)));

                per_file.push(FileDiagnostics {
                    relative_path,
                    diagnostics,
                });
            }
        }

        per_file.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        per_file
    }

    /// Returns all diagnostics as a human readable string
    pub fn emit_diagnostics_to_string(
        &self,
        display_color: DisplayColor,
    ) -> anyhow::Result<Option<String>> {
        let mut compiler_errors: Vec<u8> = Vec::new();
        if self.emit_diagnostics(&mut Cursor::new(&mut compiler_errors), display_color)? {
            Ok(Some(String::from_utf8(compiler_errors).map_err(|e| {
                anyhow::anyhow!("could not convert compiler diagnostics to valid UTF8: {e}")
            })?))
        } else {
            Ok(None)
        }
    }
}

impl Driver {
    /// Get the path where the driver will write the assembly for the specified
    /// file.
    pub fn assembly_output_path_from_file(&self, file_id: FileId) -> PathBuf {
        let module_partition = self.db.module_partition();
        let module_group_id = module_partition
            .group_for_file(file_id)
            .expect("could not find file in module parition");
        self.path_for_module_group(&module_partition[module_group_id])
            .with_extension(TargetAssembly::EXTENSION)
    }

    /// Get the path where the driver will write the IR for the specified file.
    pub fn ir_output_path_from_file(&self, file_id: FileId) -> PathBuf {
        let module_partition = self.db.module_partition();
        let module_group_id = module_partition
            .group_for_file(file_id)
            .expect("could not find file in module parition");
        self.path_for_module_group(&module_partition[module_group_id])
            .with_extension(AssemblyIr::EXTENSION)
    }

    /// Get the path where the driver will write the assembly for the specified
    /// module.
    pub fn assembly_output_path(&self, module: Module) -> PathBuf {
        let module_partition = self.db.module_partition();
        let module_group_id = module_partition
            .group_for_module(module)
            .expect("could not find file in module parition");
        self.path_for_module_group(&module_partition[module_group_id])
            .with_extension(TargetAssembly::EXTENSION)
    }

    /// Get the path where the driver will write the IR for the specified
    /// module.
    pub fn ir_output_path(&self, module: Module) -> PathBuf {
        let module_partition = self.db.module_partition();
        let module_group_id = module_partition
            .group_for_module(module)
            .expect("could not find file in module parition");
        self.path_for_module_group(&module_partition[module_group_id])
            .with_extension(AssemblyIr::EXTENSION)
    }

    /// Returns the output path for the specified module group without an
    /// extension
    fn path_for_module_group(&self, module_group: &ModuleGroup) -> PathBuf {
        module_group.relative_file_path().to_path(&self.out_dir)
    }

    /// Writes all assemblies. If `force` is false, the binary will not be
    /// written if there are no changes since last time it was written.
    pub fn write_all_assemblies(&mut self, force: bool) -> Result<(), anyhow::Error> {
        let _lock = self.acquire_filesystem_output_lock()?;

        // `build_partition` gives every module its own independent
        // `ModuleGroup` (own LLVM context, own output file, no shared
        // state), so their `target_assembly`/`assembly_ir` queries can be
        // computed concurrently. Warm them all before the loop below so
        // that loop -- which must stay serial, since it does file I/O and
        // updates `module_to_temp_assembly_path` in a fixed order -- pays
        // for at most one group's codegen at a time (cache hit) instead of
        // the sum of every group's codegen (cache miss, one after another).
        self.warm_assemblies_in_parallel();

        // Create a copy of all current files
        for package in codira_hir::Package::all(&self.db) {
            for module in package.modules(&self.db) {
                if self.emit_ir {
                    self.write_assembly_ir(module)?;
                } else {
                    self.write_target_assembly(module, force)?;
                }
            }
        }

        Ok(())
    }

    /// Pre-computes every module group's `target_assembly`/`assembly_ir`
    /// query on a pool of worker threads, so the serial write loop in
    /// `write_all_assemblies` finds a warm Salsa cache waiting for it.
    ///
    /// Each worker gets its own `db.snapshot()`: a read-only handle onto the
    /// same revision, backed by the same underlying query storage, so a
    /// value computed through a snapshot is visible to `self.db` afterward
    /// exactly as if `self.db` had computed it directly. Workers claim
    /// groups one at a time from a shared atomic counter rather than a
    /// fixed slice per thread, because module sizes in a real package are
    /// wildly uneven -- a static split would leave threads that drew small
    /// modules idle while one thread is still working through the largest.
    ///
    /// This is sound only because nothing reachable from `CodeGenDatabase`'s
    /// query storage is un-`Send` anymore: see `codira_codegen::db`'s
    /// `build_target_machine`, which is what made `CompilerDatabase`
    /// eligible for `ParallelDatabase` at all.
    fn warm_assemblies_in_parallel(&self) {
        use codira_hir::salsa::ParallelDatabase;

        let module_partition = self.db.module_partition();
        let group_ids: Vec<ModuleGroupId> = module_partition.iter().map(|(id, _)| id).collect();

        let num_threads = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(group_ids.len());

        // Nothing to gain from spinning up threads for one group, or fewer
        // groups than there are cores to run them on.
        if num_threads <= 1 {
            return;
        }

        let next_index = AtomicUsize::new(0);
        let emit_ir = self.emit_ir;

        std::thread::scope(|scope| {
            for _ in 0..num_threads {
                let db = self.db.snapshot();
                let group_ids = &group_ids;
                let next_index = &next_index;
                scope.spawn(move || loop {
                    let idx = next_index.fetch_add(1, Ordering::Relaxed);
                    let Some(&group_id) = group_ids.get(idx) else {
                        break;
                    };
                    if emit_ir {
                        let _ = db.assembly_ir(group_id);
                    } else {
                        let _ = db.target_assembly(group_id);
                    }
                });
            }
        });
    }

    /// Acquires a filesystem lock on the output directory. This ensures that
    /// multiple instances cannot write to the same output directory and
    /// that the runtime does not start reading before we finished writing.
    ///
    /// # Why this gives up
    ///
    /// This used to be `loop { .. sleep(1s) }` with no bound and no message
    /// -- the "Blocked on acquiring lock" print was commented out awaiting a
    /// driver-level diagnostics channel (TODO #313). The result was the
    /// worst failure mode a build tool has: interrupt one build (Ctrl-C, a
    /// cancelled CI job, a crash) and `lockfile::Lockfile`'s `Drop` never
    /// runs, so the lock file survives -- and from then on *every* build in
    /// that directory hung forever, silently, with no way to discover why.
    ///
    /// The lock file carries no owner information, so a stale lock cannot be
    /// told from a live one and reclaiming it automatically could stomp a
    /// concurrent build. What it can do is fail loudly and say exactly what
    /// to delete, which turns an unrecoverable silent hang into a
    /// 30-second error with instructions.
    ///
    /// Writing directly to stderr is deliberate and matches what every build
    /// tool does when it blocks (cargo prints "Blocking waiting for file
    /// lock"): a user staring at a stalled build needs to be told *now*, not
    /// through a diagnostics channel that is collected and reported at the
    /// end of a run that may never finish.
    fn acquire_filesystem_output_lock(&self) -> Result<lockfile::Lockfile, anyhow::Error> {
        let path = self.out_dir.join(LOCKFILE_NAME);

        for attempt in 0..LOCK_ACQUIRE_ATTEMPTS {
            match lockfile::Lockfile::create(&path) {
                Ok(lockfile) => return Ok(lockfile),
                Err(_) if attempt == 0 => {
                    // Announce on the first block only, so a build that is
                    // genuinely waiting on a concurrent one does not spam.
                    eprintln!(
                        "Blocked on acquiring the lock on the output directory ({})",
                        path.display()
                    );
                }
                Err(_) => {}
            }
            std::thread::sleep(LOCK_RETRY_INTERVAL);
        }

        let seconds =
            u64::from(LOCK_ACQUIRE_ATTEMPTS) * LOCK_RETRY_INTERVAL.as_millis() as u64 / 1000;
        Err(anyhow::anyhow!(
            concat!(
                "could not acquire the lock on the output directory after {seconds} seconds.\n",
                "`{path}` exists, which means another `codira` build is writing there, ",
                "or one was interrupted and left the lock behind.\n",
                "If no other build is running, delete that file and try again."
            ),
            seconds = seconds,
            path = path.display(),
        ))
    }

    /// Generates an assembly for the target machine and specified module and
    /// stores it in the output location. If `force` is false, the binary
    /// will not be written if there are no changes since last time it was
    /// written. Returns `true` if the assembly was written, `false`
    /// if it was up to date.
    fn write_target_assembly(
        &mut self,
        module: Module,
        force: bool,
    ) -> Result<bool, anyhow::Error> {
        log::trace!("writing target assembly for {module:?}");

        // Find the module group to which the module belongs
        let module_partition = self.db.module_partition();
        let module_group_id = module_partition
            .group_for_module(module)
            .expect("could not find the module in the module partition");
        let module_group = &module_partition[module_group_id];

        // Get the compiled assembly
        let assembly = self.db.target_assembly(module_group_id);
        // A target can refuse code it cannot hold (WebAssembly refuses code
        // that needs the Codira runtime); that is the build's error.
        if let Some(reason) = assembly.error() {
            anyhow::bail!("{reason}");
        }

        // A library rewrite rule that `codira_tv` could not prove fails the
        // build: an unproven rule is never used, and silently dropping it
        // would hide that (`codira_codegen::rewrite`).
        let rewrite_rules = self.db.rewrite_rules();
        if !rewrite_rules.refused.is_empty() {
            anyhow::bail!("{}", rewrite_rules.refused.join("\n"));
        }

        // `@target(gpu)` functions are also compiled for the GPU, to PTX
        // written next to the library (`codira_codegen::gpu`). A kernel that
        // cannot run on a GPU fails the build with the reason.
        let gpu_kernels = self.db.gpu_kernels(module_group_id);
        let gpu_kernels = match &*gpu_kernels {
            Err(reason) => anyhow::bail!("{reason}"),
            Ok(kernels) => kernels.clone(),
        };

        // `@target(fpga)` functions are also written as Verilog modules,
        // `<function>.v` next to the library (`codira_circuits`). A function
        // outside the circuit subset fails the build with the reason.
        let fpga_modules = codira_circuits::fpga_modules(
            &self.db,
            module_group
                .iter()
                .flat_map(|module| module.all_functions(&self.db)),
        )
        .map_err(|e| anyhow::anyhow!("cannot compile an @target(fpga) function to Verilog: {e}"))?;

        // Determine the filename of the group (`.codiralib`, or `.wasm` for
        // a WebAssembly target)
        let assembly_path = self
            .path_for_module_group(module_group)
            .with_extension(assembly.extension());

        // Did the assembly change since last time?
        if !force
            && assembly_path.is_file()
            && self
                .module_to_temp_assembly_path
                .get(&module)
                .map(AsRef::as_ref)
                == Some(assembly.path())
        {
            return Ok(false);
        }

        // It did change or we are forced, so write it to disk
        assembly.copy_to(&assembly_path)?;
        if let Some(kernels) = gpu_kernels {
            std::fs::write(assembly_path.with_extension("ptx"), kernels.ptx)?;
        }
        for (name, verilog) in fpga_modules {
            std::fs::write(assembly_path.with_file_name(format!("{name}.v")), verilog)?;
        }

        // Rewrites come before the certificate, which hashes the machine
        // code, and before the constant-time check, which reads it.
        if self.superoptimize {
            for line in
                crate::superopt::optimize_library(&self.db, module_group_id, &assembly_path)?
            {
                eprintln!("{line}");
            }
        }

        // A certificate describes exactly one build of the library: write
        // this one's, or remove one left by an earlier certified build.
        let certificate = codira_certificate::sidecar_path(&assembly_path);
        if self.certify {
            crate::certify::write_certificate(&self.db, module_group_id, &assembly_path)?;
        } else if certificate.is_file() {
            std::fs::remove_file(&certificate)?;
        }

        // The machine code of the constant-time functions, as written.
        if self.check_constant_time {
            let (report, notes) =
                crate::ct_check::check_library(&self.db, module_group_id, &assembly_path)?;
            for note in &notes {
                eprintln!("note: machine-code constant-time check: {note}");
            }
            if !report.passed() {
                // Not a library anyone should load.
                std::fs::remove_file(&assembly_path)?;
                anyhow::bail!(
                    "{}",
                    crate::ct_check::failure_message(&report, &assembly_path)
                );
            }
            let checked = report.functions.iter().filter(|f| f.was_checked()).count();
            eprintln!(
                "machine-code constant-time check ({}): {checked} function(s) checked, no rule \
                 broken",
                report.arch
            );
        }

        // Store the information so we maybe don't have to write it next time
        self.module_to_temp_assembly_path
            .insert(module, assembly.path().to_path_buf());

        Ok(true)
    }

    /// Generates IR for the specified module and stores it in the output
    /// location.
    fn write_assembly_ir(&mut self, module: codira_hir::Module) -> Result<(), anyhow::Error> {
        log::trace!("writing assembly IR for {module:?}");

        // Find the module group to which the module belongs
        let module_partition = self.db.module_partition();
        let module_group_id = module_partition
            .group_for_module(module)
            .expect("could not find the module in the module partition");
        let module_group = &module_partition[module_group_id];

        // Get the compiled assembly
        let assembly_ir = self.db.assembly_ir(module_group_id);

        // Determine the filename of the group
        let assembly_path = self
            .path_for_module_group(module_group)
            .with_extension(AssemblyIr::EXTENSION);

        // Write to disk
        assembly_ir.copy_to(assembly_path)?;

        Ok(())
    }
}

impl Driver {
    /// Returns the `FileId` of the file with the given relative path
    pub fn get_file_id_for_path<P: AsRef<RelativePath>>(&self, path: P) -> Option<FileId> {
        self.path_to_file_id.get(path.as_ref()).copied()
    }

    /// Tells the driver that the file at the specified `path` has changed its
    /// contents. Returns the `FileId` of the modified file.
    pub fn update_file<P: AsRef<RelativePath>>(&mut self, path: P, contents: String) -> FileId {
        let file_id = *self
            .path_to_file_id
            .get(path.as_ref())
            .expect("writing to a file that is not part of the source root should never happen");
        self.db.set_file_text(file_id, Arc::from(contents));
        file_id
    }

    /// Adds a new file to the driver. Returns the `FileId` of the new file.
    pub fn add_file<P: AsRef<RelativePath>>(&mut self, path: P, contents: String) -> FileId {
        let file_id = self.alloc_file_id(path.as_ref()).unwrap();

        // Insert the new file
        self.db.set_file_text(file_id, Arc::from(contents));
        self.db.set_file_source_root(file_id, WORKSPACE);

        // Update the source root
        self.source_root
            .insert_file(file_id, path.as_ref().to_relative_path_buf());
        self.db
            .set_source_root(WORKSPACE, Arc::new(self.source_root.clone()));

        file_id
    }

    /// Removes the specified file from the driver.
    pub fn remove_file<P: AsRef<RelativePath>>(&mut self, path: P) -> FileId {
        let file_id = *self
            .path_to_file_id
            .get(path.as_ref())
            .expect("removing to a file that is not part of the source root should never happen");

        // Update the source root
        self.source_root.remove_file(file_id);
        self.db
            .set_source_root(WORKSPACE, Arc::new(self.source_root.clone()));

        file_id
    }

    /// Renames the specified file to the specified path
    pub fn rename<P1: AsRef<RelativePath>, P2: AsRef<RelativePath>>(
        &mut self,
        from: P1,
        to: P2,
    ) -> FileId {
        let file_id = *self
            .path_to_file_id
            .get(from.as_ref())
            .expect("renaming from a file that is not part of the source root should never happen");
        if let Some(previous) = self.path_to_file_id.get(to.as_ref()) {
            // If there was some other file with this path in the database, forget about it.
            self.file_id_to_path.remove(previous);
        }

        self.file_id_to_path
            .insert(file_id, to.as_ref().to_relative_path_buf());
        // The `FileId` now belongs to `to`. Both halves of that have to be
        // recorded: forgetting the old path alone leaves the file with no
        // path that leads to it, and the next edit to it under its new
        // name panics in `update_file` as "not part of the source root".
        self.path_to_file_id.remove(from.as_ref());
        self.path_to_file_id
            .insert(to.as_ref().to_relative_path_buf(), file_id);

        self.source_root.remove_file(file_id);
        self.source_root
            .insert_file(file_id, to.as_ref().to_relative_path_buf());
        self.db
            .set_source_root(WORKSPACE, Arc::new(self.source_root.clone()));

        file_id
    }
}

impl Driver {
    /// Brings the driver in line with the source files under
    /// `source_directory` as they are on disk right now: files that
    /// appeared are added, files whose contents differ are updated, and
    /// files that are gone are removed.
    ///
    /// This is what lets a driver outlive a single build without a file
    /// watcher. A watcher reports changes eventually; a build that runs
    /// before the event for the latest save arrives compiles the previous
    /// contents and reports success. Reading the directory is the only
    /// way to *know* what is in it, so that is what this does, every
    /// time -- and afterwards the driver is in exactly the state a driver
    /// freshly created from the directory would be in.
    ///
    /// Contents are compared, not timestamps. Modification times have
    /// coarse and filesystem-dependent resolution, so two saves in quick
    /// succession can share one, and tools that restore files restore
    /// their times with them. A file is only handed to salsa when its
    /// bytes actually differ, which keeps every query that depends on an
    /// untouched file cached.
    pub fn sync_source_directory(
        &mut self,
        source_directory: &Path,
    ) -> Result<SyncSummary, anyhow::Error> {
        let mut summary = SyncSummary::default();

        // Files the driver currently considers part of the package. A
        // path the driver has seen before but since removed still has a
        // `FileId` reserved in `path_to_file_id`, so being known is not
        // the same as being live.
        let live: HashSet<FileId> = self.source_root.files().collect();
        let mut on_disk: HashSet<FileId> = HashSet::with_capacity(live.len());

        for source_file_path in iter_source_files(source_directory) {
            let relative_path = compute_source_relative_path(source_directory, &source_file_path)?;

            let contents = match std::fs::read_to_string(&source_file_path) {
                Ok(contents) => contents,
                // Deleted between listing the directory and reading the
                // file: it is not on disk, which is all that matters.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "could not read contents of '{}': {}",
                        source_file_path.display(),
                        e
                    ))
                }
            };

            let live_file_id = self
                .path_to_file_id
                .get(&relative_path)
                .copied()
                .filter(|file_id| live.contains(file_id));

            let file_id = if let Some(file_id) = live_file_id {
                if *self.db.file_text(file_id) != *contents {
                    self.db.set_file_text(file_id, Arc::from(contents));
                    summary.updated += 1;
                }
                file_id
            } else {
                let file_id = self.alloc_file_id(&relative_path)?;
                self.db.set_file_text(file_id, Arc::from(contents));
                self.db.set_file_source_root(file_id, WORKSPACE);
                self.source_root.insert_file(file_id, relative_path);
                summary.added += 1;
                file_id
            };
            on_disk.insert(file_id);
        }

        for file_id in live.difference(&on_disk) {
            self.source_root.remove_file(*file_id);
            summary.removed += 1;
        }

        // The set of files is itself a salsa input. Setting it bumps the
        // revision even when the value is identical, so it is only set
        // when the set really changed.
        if summary.added + summary.removed > 0 {
            self.db
                .set_source_root(WORKSPACE, Arc::new(self.source_root.clone()));
        }

        // An import of the standard library may have appeared or gone.
        if !summary.is_unchanged() {
            self.refresh_std_package()?;
        }

        Ok(summary)
    }
}

impl Driver {
    /// Loads the standard-library modules the program's imports name, and
    /// the ones they import in turn, as the package `std` (see
    /// `crate::std_library`); drops the ones it no longer imports.
    ///
    /// The constructors and `sync_source_directory` call this. After
    /// `add_file`, `update_file`, `remove_file` or `rename` changed which
    /// standard-library modules the program imports, call it again.
    ///
    /// Nothing is loaded when the program imports nothing from `std`, or
    /// when it has a top-level module of its own called `std` (which is then
    /// what `std` means in it). Fails when the program imports `std` and the
    /// standard library cannot be found, or one of its files cannot be read.
    pub fn refresh_std_package(&mut self) -> Result<(), anyhow::Error> {
        use crate::std_library::{std_closure, std_library_dir, std_module_path, STD_PACKAGE_NAME};

        let mut roots = Vec::new();
        let mut defines_std = false;
        for file_id in self.source_root.files() {
            let relative = self.source_root.relative_path(file_id);
            let first = relative.components().next().map(|c| c.as_str().to_owned());
            if first.as_deref().is_some_and(|first| {
                first == STD_PACKAGE_NAME || first == format!("{STD_PACKAGE_NAME}.code")
            }) {
                defines_std = true;
            }
            for import in codira_hir::import_paths_in_source(&self.db.file_text(file_id)) {
                if let Some(path) = std_module_path(&import, None) {
                    roots.push(path);
                }
            }
        }

        let files = if roots.is_empty() || defines_std || self.is_std_itself {
            std::collections::BTreeMap::new()
        } else {
            let std_dir = std_library_dir().map_err(|reason| {
                anyhow::anyhow!("the program imports the standard library, but {reason}")
            })?;
            std_closure(&std_dir, roots)?
        };

        // Set only what changed: every salsa input set invalidates what was
        // computed from it, even when the value is the same.
        let mut source_root = SourceRoot::default();
        let mut std_files = std::collections::BTreeMap::new();
        for (relative, text) in files {
            let file_id = if let Some(&file_id) = self.std_files.get(&relative) {
                if *self.db.file_text(file_id) != *text {
                    self.db.set_file_text(file_id, Arc::from(text));
                }
                file_id
            } else {
                let file_id = FileId(
                    self.next_file_id
                        .try_into()
                        .map_err(|_e| anyhow::anyhow!("too many active source files"))?,
                );
                self.next_file_id += 1;
                self.db.set_file_text(file_id, Arc::from(text));
                self.db.set_file_source_root(file_id, STD_SOURCE_ROOT);
                file_id
            };
            source_root.insert_file(file_id, relative.as_str());
            std_files.insert(relative, file_id);
        }
        let file_set_changed = std_files.keys().ne(self.std_files.keys());
        self.std_files = std_files;
        if file_set_changed || !self.std_loaded {
            self.std_source_root = source_root;
            self.db
                .set_source_root(STD_SOURCE_ROOT, Arc::new(self.std_source_root.clone()));
        }

        let load = !self.std_files.is_empty();
        if load != self.std_loaded {
            let mut package_set = PackageSet::default();
            package_set.add_package(WORKSPACE);
            if load {
                package_set.add_named_package(STD_SOURCE_ROOT, STD_PACKAGE_NAME);
            }
            self.db.set_packages(Arc::new(package_set));
            self.std_loaded = load;
        }
        Ok(())
    }

    /// The standard-library files loaded for the program's imports, by path
    /// relative to the standard library's directory (`crypto/hash.code`).
    pub fn std_files(&self) -> impl Iterator<Item = &str> + '_ {
        self.std_files.keys().map(String::as_str)
    }
}

pub fn iter_source_files(source_dir: &Path) -> impl Iterator<Item = PathBuf> {
    WalkDir::new(source_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| is_source_file(e.path()))
        .map(|e| e.path().to_path_buf())
}
