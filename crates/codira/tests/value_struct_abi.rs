//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - Regression tests for `public` functions that take or return a value
//!   `struct`. Such a function gets a host-visible wrapper (the runtime's
//!   calling convention boxes value structs), and the wrapper of a function
//!   whose body ended in `return x;` used to be emitted without a
//!   terminator: invalid IR on which LLVM's optimizer segfaulted inside
//!   `codira build`. These tests build and run packages with such
//!   functions, call them from another module, and call them from the host
//!   through the runtime.

use std::{ffi::OsString, path::Path, process::Command};

use codira::run_with_args;
use codira_runtime::{Runtime, StructRef};

/// Writes a package whose `src/` holds `files` (`(name, source)` pairs).
fn package_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_value_struct_abi")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    for (name, source) in files {
        std::fs::write(dir.path().join("src").join(name), source).unwrap();
    }
    dir
}

fn build(dir: &Path) -> codira::ExitStatus {
    let args: Vec<OsString> = vec![
        "codira".into(),
        "build".into(),
        "--manifest-path".into(),
        dir.join("codira.toml").into(),
    ];
    run_with_args(args).unwrap()
}

/// The reproducer of `lab/rfcs/0004-runtime-io-and-hashing.md` §12, which
/// made `codira build` segfault.
const RFC_0004_REPRODUCER: &str = r#"
public struct File { _handle: i64, }
public func get(file: File) -> i64 { return file._handle; }
public func s1() -> i64 { get(File { _handle: 3 }) }
"#;

#[test]
fn a_public_function_taking_a_value_struct_and_ending_in_return_builds() {
    let dir = package_with(&[("mod.code", RFC_0004_REPRODUCER)]);
    assert_eq!(build(dir.path()), codira::ExitStatus::Success);
    let library = dir.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();
    let three: i64 = runtime.invoke("s1", ()).unwrap();
    assert_eq!(three, 3);
}

/// Value structs in and out of `public` functions, written both with a
/// trailing `return` and with a tail expression, in another module.
const GEOMETRY: &str = r#"
public struct Point { public x: i64, public y: i64 }

public func origin() -> Point { return Point { x: 0, y: 0 }; }

public func shift(p: Point, dx: i64, dy: i64) -> Point {
    return Point { x: p.x + dx, y: p.y + dy };
}

public func mirror(p: Point) -> Point { Point { x: p.y, y: p.x } }

public func x_of(p: Point) -> i64 { return p.x; }

public func y_of(p: Point) -> i64 { p.y }

public func manhattan(a: Point, b: Point) -> i64 {
    var dx = a.x - b.x;
    var dy = a.y - b.y;
    if dx < 0 { dx = -dx; }
    if dy < 0 { dy = -dy; }
    return dx + dy;
}
"#;

const GEOMETRY_MAIN: &str = r#"
import geometry.{Point, origin, shift, mirror, x_of, y_of, manhattan}

public func main() {
    let p = shift(origin(), 3, 4)
    println(x_of(p))
    println(y_of(p))
    let q = mirror(p)
    println(q.x * 10 + q.y)
    println(manhattan(p, Point { x: -1, y: 10 }))
}

public func make(x: i64, y: i64) -> Point { return shift(origin(), x, y); }

public func sum(p: Point) -> i64 { return x_of(p) + y_of(p); }
"#;

#[test]
fn value_structs_cross_modules_and_the_host_boundary() {
    let dir = package_with(&[("mod.code", GEOMETRY_MAIN), ("geometry.code", GEOMETRY)]);

    // Called from another module, through `codira run`.
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("run")
        .arg("--manifest-path")
        .arg(dir.path().join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .output()
        .expect("failed to spawn codira");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert!(
        output.status.success(),
        "codira run failed\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout, "3\n4\n43\n10\n");

    // Called from the host: a value struct comes back boxed, and goes in
    // boxed, through the wrappers.
    let library = dir.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();
    let point: StructRef<'_> = runtime.invoke("make", (5i64, 7i64)).unwrap();
    assert_eq!(point.get::<i64>("x").unwrap(), 5);
    assert_eq!(point.get::<i64>("y").unwrap(), 7);
    let point = point.root();
    let sum: i64 = runtime.invoke("sum", (point.as_ref(&runtime),)).unwrap();
    assert_eq!(sum, 12);
}

/// A module whose only allocation is the boxing of a value struct passed to
/// another module's function: the boxing needs the runtime's `new`, which
/// used to be missing from such a module's dispatch table (a panic in code
/// generation, "unknown function").
#[test]
fn a_module_that_only_passes_value_structs_across_modules_builds() {
    let user = r#"
import geometry.{Point, origin, x_of}

func helper(p: Point) -> i64 { x_of(p) }

public func run() -> i64 { helper(origin()) + 5 }
"#;
    let dir = package_with(&[("mod.code", user), ("geometry.code", GEOMETRY)]);
    assert_eq!(build(dir.path()), codira::ExitStatus::Success);
    let library = dir.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();
    let five: i64 = runtime.invoke("run", ()).unwrap();
    assert_eq!(five, 5);
}

/// `std/memory/region.code`'s `Region` wrappers are `public` functions
/// taking and returning a value struct, ending in `return`: they could not
/// be built before. Compiled into a package (without its `import` line, as
/// a package's `root` is not `std`) and driven by a program.
#[test]
fn the_std_region_wrappers_build_and_run() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std/memory/region.code");
    let region: String = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("import "))
        .map(|line| format!("{line}\n"))
        .collect();
    let program = r#"
public func through_regions() -> i64 {
    let r = region_with_quota(64);
    if r._handle == 0 { return 1; }
    let a = region_alloc(r, 16, 8);
    if a == 0 { return 2; }
    if region_used(r) != 16 { return 3; }
    // Over the quota: refused with 0.
    if region_alloc(r, 64, 8) != 0 { return 4; }
    if !region_reset(r) { return 5; }
    if region_used(r) != 0 { return 6; }
    if !region_free(r) { return 7; }
    // A freed handle is refused.
    if region_free(r) { return 8; }
    0
}
"#;
    let dir = package_with(&[("mod.code", &format!("{region}{program}"))]);
    assert_eq!(build(dir.path()), codira::ExitStatus::Success);
    let library = dir.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();
    let step: i64 = runtime.invoke("through_regions", ()).unwrap();
    assert_eq!(step, 0, "step {step} of `through_regions` failed");
    let region: StructRef<'_> = runtime.invoke("region_new", ()).unwrap();
    assert_ne!(region.get::<usize>("_handle").unwrap(), 0);
}
