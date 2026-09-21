//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_codegen::{CodeGenDatabase, CodeGenDatabaseStorage};
use codira_hir::{salsa, HirDatabase};

use crate::Config;

/// A compiler database is a salsa database that enables increment compilation.
#[salsa::database(
    codira_hir_input::SourceDatabaseStorage,
    codira_hir::InternDatabaseStorage,
    codira_hir::AstDatabaseStorage,
    codira_hir::DefDatabaseStorage,
    codira_hir::HirDatabaseStorage,
    CodeGenDatabaseStorage
)]
pub struct CompilerDatabase {
    storage: salsa::Storage<Self>,
}

impl CompilerDatabase {
    /// Constructs a new database
    pub fn new(config: &Config) -> Self {
        let mut db = CompilerDatabase {
            storage: salsa::Storage::default(),
        };

        // Set the initial configuration
        db.set_config(config);

        db
    }

    /// Applies the given configuration to the database
    pub fn set_config(&mut self, config: &Config) {
        self.set_target(config.target.clone());
        self.set_optimization_level(config.optimization_lvl);
    }
}

impl salsa::Database for CompilerDatabase {}

/// Lets `Driver::write_all_assemblies` hand each module group its own
/// read-only view of the database and compile it on its own thread.
///
/// Salsa's own requirement for this (`ParallelDatabase: Database + Send`,
/// and every query's key/value types `Send` too -- see the trait's doc
/// comment) is exactly what removing the memoized `Rc<TargetMachine>` from
/// `CodeGenDatabase` was for: an `Rc` field anywhere in the query storage
/// would have made this `impl` impossible to write, full stop, because it
/// would have made `CompilerDatabase` itself not `Send`.
impl codira_hir::salsa::ParallelDatabase for CompilerDatabase {
    fn snapshot(&self) -> codira_hir::salsa::Snapshot<Self> {
        codira_hir::salsa::Snapshot::new(CompilerDatabase {
            storage: self.storage.snapshot(),
        })
    }
}
