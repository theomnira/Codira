//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! `Driver` is a stateful compiler frontend that enables incremental
//! compilation by retaining state from previous compilation.

use codira_codegen::{AssemblyIr, CodeGenDatabase, ModuleGroup, TargetAssembly};
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
    collections::HashMap,
    convert::TryInto,
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
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
use crate::diagnostics_snippets::{emit_hir_diagnostic, emit_syntax_error};

pub const WORKSPACE: SourceRootId = SourceRootId(0);

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
            .map_err(|e| anyhow::anyhow!("could not create package output directory: {}", e))?;

        // Construct the driver
        let mut driver = Driver::with_config(config, output_dir);

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

        Ok(driver)
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
                    let relative_file_path = self.db.file_relative_path(file_id);
                    let line_index = self.db.line_index(file_id);

                    // Emit all syntax diagnostics
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
                    }

                    // Emit all HIR diagnostics
                    let mut error = None;
                    module.diagnostics(
                        &self.db,
                        &mut DiagnosticSink::new(|d| {
                            has_error = true;
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
                let relative_file_path = self.db.file_relative_path(file_id);
                let line_index = self.db.line_index(file_id);

                let mut file_has_diagnostic = false;

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
                    file_has_diagnostic = true;
                }

                let mut error = None;
                module.diagnostics(
                    &self.db,
                    &mut DiagnosticSink::new(|d| {
                        counts.semantic += 1;
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

                if !file_has_diagnostic {
                    counts.clean_files += 1;
                }
            }
        }

        Ok(counts)
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
                let relative_path = self.db.file_relative_path(file_id).to_string();

                let mut diagnostics: Vec<CheckDiagnostic> = parse
                    .errors()
                    .iter()
                    .map(|syntax_error| CheckDiagnostic {
                        kind: DiagnosticKind::Syntax,
                        line: line_index.line_col(syntax_error.location().offset()).line + 1,
                        message: syntax_error.to_string(),
                    })
                    .collect();

                module.diagnostics(
                    &self.db,
                    &mut DiagnosticSink::new(|d| {
                        diagnostics.push(CheckDiagnostic {
                            kind: DiagnosticKind::Semantic,
                            line: line_index.line_col(d.highlight_range().start()).line + 1,
                            message: d.message(),
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
                anyhow::anyhow!(
                    "could not convert compiler diagnostics to valid UTF8: {}",
                    e
                )
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
        log::trace!("writing target assembly for {:?}", module);

        // Find the module group to which the module belongs
        let module_partition = self.db.module_partition();
        let module_group_id = module_partition
            .group_for_module(module)
            .expect("could not find the module in the module partition");
        let module_group = &module_partition[module_group_id];

        // Get the compiled assembly
        let assembly = self.db.target_assembly(module_group_id);

        // Determine the filename of the group
        let assembly_path = self
            .path_for_module_group(module_group)
            .with_extension(TargetAssembly::EXTENSION);

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

        // Store the information so we maybe don't have to write it next time
        self.module_to_temp_assembly_path
            .insert(module, assembly.path().to_path_buf());

        Ok(true)
    }

    /// Generates IR for the specified module and stores it in the output
    /// location.
    fn write_assembly_ir(&mut self, module: codira_hir::Module) -> Result<(), anyhow::Error> {
        log::trace!("writing assembly IR for {:?}", module);

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
        self.path_to_file_id.remove(from.as_ref()); // FileId now belongs to to

        self.source_root.remove_file(file_id);
        self.source_root
            .insert_file(file_id, to.as_ref().to_relative_path_buf());
        self.db
            .set_source_root(WORKSPACE, Arc::new(self.source_root.clone()));

        file_id
    }
}

pub fn iter_source_files(source_dir: &Path) -> impl Iterator<Item = PathBuf> {
    WalkDir::new(source_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| is_source_file(e.path()))
        .map(|e| e.path().to_path_buf())
}
