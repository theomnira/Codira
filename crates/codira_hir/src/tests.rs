//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::sync::Arc;

use codira_hir_input::{PackageId, SourceDatabase, WithFixture};

use crate::{db::DefDatabase, mock::MockDatabase};

/// This function tests that the `ModuleData` of a module does not change if the
/// contents of a function is changed.
#[test]
fn check_package_defs_does_not_change() {
    let (mut db, file_id) = MockDatabase::with_single_file(
        r#"
    func foo()->i32 {
        1+1
    }
    "#,
    );

    {
        let events = db.log_executed(|| {
            db.package_defs(PackageId(0));
        });
        assert!(
            format!("{events:?}").contains("package_defs"),
            "{events:#?}"
        );
    }
    db.set_file_text(
        file_id,
        Arc::from(
            r#"
    func foo()->i32 {
        90
    }
    "#
            .to_owned(),
        ),
    );
    {
        let events = db.log_executed(|| {
            db.package_defs(PackageId(0));
        });
        assert!(
            !format!("{events:?}").contains("package_defs"),
            "{events:#?}"
        );
    }
}
