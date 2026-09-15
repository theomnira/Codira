//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use crate::Project;

#[test]
fn test_server() {
    let _server = Project::with_fixture(
        r#"
//- /codira.toml
[package]
name = "foo"
version = "0.0.0"

//- /src/mod.code
fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#,
    )
    .server()
    .wait_until_workspace_is_loaded();
}
