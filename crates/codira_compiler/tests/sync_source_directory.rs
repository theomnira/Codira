//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 1, 2026
//!
//! Functionality:
//! - `Driver::sync_source_directory` must leave a long-lived driver in exactly
//!   the state a fresh one would be in, whatever happened on disk in between.

use std::path::{Path, PathBuf};

use codira_compiler::{Config, DisplayColor, Driver, SyncSummary};
use tempfile::TempDir;

const VALID: &str = "public func main() -> i64 {\n    1\n}\n";
const BROKEN: &str = "public func main() -> i64 {\n    1 +\n}\n";

struct Project {
    dir: TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("codira.toml"),
            "[package]\nname=\"sample\"\nauthors=[]\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        Self { dir }
    }

    fn source_dir(&self) -> PathBuf {
        self.dir.path().join("src")
    }

    fn write(&self, file: &str, contents: &str) {
        let path = self.source_dir().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn remove(&self, file: &str) {
        std::fs::remove_file(self.source_dir().join(file)).unwrap();
    }

    fn driver(&self) -> Driver {
        Driver::with_package_path(self.dir.path().join("codira.toml"), Config::default())
            .unwrap()
            .1
    }
}

fn sync(driver: &mut Driver, source_dir: &Path) -> SyncSummary {
    driver.sync_source_directory(source_dir).unwrap()
}

/// Diagnostics, as text: what a build of this driver would report.
fn diagnostics(driver: &Driver) -> String {
    driver
        .emit_diagnostics_to_string(DisplayColor::Disable)
        .unwrap()
        .unwrap_or_default()
}

/// What a driver created from scratch reports for the project as it is on
/// disk right now -- the answer a synced driver has to agree with.
fn fresh_diagnostics(project: &Project) -> String {
    diagnostics(&project.driver())
}

#[test]
fn an_untouched_directory_changes_nothing() {
    let project = Project::new();
    project.write("mod.code", VALID);
    let mut driver = project.driver();

    let summary = sync(&mut driver, &project.source_dir());
    assert_eq!(summary, SyncSummary::default());
    assert!(summary.is_unchanged());
    assert_eq!(summary.total(), 0);
}

#[test]
fn an_edit_is_picked_up() {
    let project = Project::new();
    project.write("mod.code", VALID);
    let mut driver = project.driver();
    assert_eq!(diagnostics(&driver), "");

    project.write("mod.code", BROKEN);
    let summary = sync(&mut driver, &project.source_dir());
    assert_eq!(
        summary,
        SyncSummary {
            updated: 1,
            ..SyncSummary::default()
        }
    );
    assert_ne!(diagnostics(&driver), "");
    assert_eq!(diagnostics(&driver), fresh_diagnostics(&project));

    project.write("mod.code", VALID);
    assert_eq!(sync(&mut driver, &project.source_dir()).updated, 1);
    assert_eq!(diagnostics(&driver), "");

    // Synced already: a second pass finds nothing left to do.
    assert!(sync(&mut driver, &project.source_dir()).is_unchanged());
}

#[test]
fn a_new_file_is_added() {
    let project = Project::new();
    project.write("mod.code", VALID);
    let mut driver = project.driver();

    project.write("nested/extra.code", BROKEN);
    let summary = sync(&mut driver, &project.source_dir());
    assert_eq!(
        summary,
        SyncSummary {
            added: 1,
            ..SyncSummary::default()
        }
    );
    assert!(driver.get_file_id_for_path("nested/extra.code").is_some());
    assert!(diagnostics(&driver).contains("extra.code"));
    assert_eq!(diagnostics(&driver), fresh_diagnostics(&project));
}

#[test]
fn a_deleted_file_is_removed() {
    let project = Project::new();
    project.write("mod.code", VALID);
    project.write("extra.code", BROKEN);
    let mut driver = project.driver();
    assert!(diagnostics(&driver).contains("extra.code"));

    project.remove("extra.code");
    let summary = sync(&mut driver, &project.source_dir());
    assert_eq!(
        summary,
        SyncSummary {
            removed: 1,
            ..SyncSummary::default()
        }
    );
    assert_eq!(diagnostics(&driver), "");
    assert_eq!(diagnostics(&driver), fresh_diagnostics(&project));

    // Removing it is not something to do twice.
    assert!(sync(&mut driver, &project.source_dir()).is_unchanged());
}

#[test]
fn a_file_that_comes_back_is_added_again() {
    let project = Project::new();
    project.write("mod.code", VALID);
    project.write("extra.code", VALID);
    let mut driver = project.driver();

    project.remove("extra.code");
    assert_eq!(sync(&mut driver, &project.source_dir()).removed, 1);

    // Back, with different contents than the driver last saw for the path.
    project.write("extra.code", BROKEN);
    let summary = sync(&mut driver, &project.source_dir());
    assert_eq!(
        summary,
        SyncSummary {
            added: 1,
            ..SyncSummary::default()
        }
    );
    assert!(diagnostics(&driver).contains("extra.code"));
    assert_eq!(diagnostics(&driver), fresh_diagnostics(&project));
}

#[test]
fn several_changes_at_once() {
    let project = Project::new();
    project.write("mod.code", VALID);
    project.write("keep.code", VALID);
    project.write("gone.code", VALID);
    let mut driver = project.driver();

    project.write("mod.code", BROKEN);
    project.write("new.code", VALID);
    project.remove("gone.code");

    let summary = sync(&mut driver, &project.source_dir());
    assert_eq!(
        summary,
        SyncSummary {
            added: 1,
            updated: 1,
            removed: 1,
        }
    );
    assert_eq!(summary.total(), 3);
    assert_eq!(diagnostics(&driver), fresh_diagnostics(&project));
}

#[test]
fn files_that_are_not_sources_are_ignored() {
    let project = Project::new();
    project.write("mod.code", VALID);
    let mut driver = project.driver();

    project.write("notes.txt", "not code");
    project.write("mod.code.bak", BROKEN);
    assert!(sync(&mut driver, &project.source_dir()).is_unchanged());
}

#[test]
fn a_renamed_file_can_still_be_edited() {
    let project = Project::new();
    project.write("old.code", VALID);
    let mut driver = project.driver();

    // What watch mode does on a rename event, followed by an edit to the
    // file under its new name.
    driver.rename("old.code", "new.code");
    assert!(driver.get_file_id_for_path("old.code").is_none());
    assert!(driver.get_file_id_for_path("new.code").is_some());

    driver.update_file("new.code", BROKEN.to_owned());
    assert!(diagnostics(&driver).contains("new.code"));
}
