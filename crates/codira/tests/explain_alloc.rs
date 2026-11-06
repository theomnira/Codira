//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests of `codira build --explain-alloc` and `--frame-alloc`:
//!   the report a package gets (golden), and a program that prints the same
//!   thing with frame allocation off and on.

use std::process::Command;

const PROGRAM: &str = r#"class Point { x: i64, y: i64 }
class Node { var value: i64, var next: Node? }
class Bag { var items: [Point] }

func make(n: i64) -> Point { Point { x: n, y: n } }
func norm(p: Point) -> i64 { p.x * p.x + p.y * p.y }

func link(head: Node, n: i64) {
    let node = Node { value: n, next: nil }
    head.next = node
}

func builder(n: i64) -> i64 {
    let bag = Bag { items: [] }
    for i in 0..n { bag.items.push(Point { x: i, y: i }) }
    bag.items.len() as i64
}

public func main() {
    var total = 0
    var kept = Point { x: 0, y: 0 }
    for i in 0..100 {
        let p = Point { x: i, y: 1 }
        let xs = [p.x, p.y, 3]
        total += norm(p) + xs[2]
        let q = Point { x: i, y: 2 }
        if i == 50 { kept = q }
    }
    let head = Node { value: 0, next: nil }
    link(head, 7)
    let made = make(3)
    let built = builder(10)
    let x = made.x
    println(f"{total} {kept:?} {built} {x}")
}
"#;

fn package() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_explain_alloc")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.code"), PROGRAM).unwrap();
    dir
}

fn codira(dir: &tempfile::TempDir, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
        .arg("--manifest-path")
        .arg(dir.path().join("codira.toml"))
        .args(["--color", "disable"])
        .output()
        .expect("failed to run codira");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn explain_alloc_reports_every_site_with_its_reason() {
    let dir = package();
    let (ok, stdout, stderr) = codira(&dir, &["build", "--frame-alloc", "--explain-alloc"]);
    assert!(ok, "{stderr}");
    let expected = "\
mod.code:5:30: in `make`: class `Point`: escapes: returned from `make`
mod.code:9:16: in `link`: class `Node`: escapes: stored into field `next` of parameter `head`, which may be a heap object
mod.code:14:15: in `builder`: class `Bag`: moved to the stack
mod.code:14:28: in `builder`: empty array literal: escapes: grown by `push`, which the runtime does on the heap
mod.code:15:36: in `builder`: class `Point`: escapes: stored into an element of a heap object
mod.code:21:16: in `main`: class `Point`: moved to the stack
mod.code:23:17: in `main`: class `Point`: moved to the stack
mod.code:24:18: in `main`: array literal of 3 elements: moved to the stack
mod.code:26:17: in `main`: class `Point`: stays on the heap: kept past its loop iteration in `kept` (its frame slot is reused each iteration)
mod.code:29:16: in `main`: class `Node`: moved to the stack
10 allocation sites: 5 moved to the stack, 4 escape, 1 stay on the heap for another reason
";
    assert_eq!(stdout, expected);

    // Without `--frame-alloc` the same sites are reported as movable.
    let (ok, stdout, stderr) = codira(&dir, &["build", "--explain-alloc"]);
    assert!(ok, "{stderr}");
    assert!(
        stdout.contains(
            "mod.code:21:16: in `main`: class `Point`: does not escape (moved to the stack with \
             `--frame-alloc`)"
        ),
        "{stdout}"
    );
    assert!(
        stdout.ends_with(
            "5 could move to the stack, 4 escape, 1 stay on the heap for another reason\n"
        ),
        "{stdout}"
    );
}

#[test]
fn frame_alloc_prints_the_same_output() {
    let dir = package();
    let (ok, plain, stderr) = codira(&dir, &["run"]);
    assert!(ok, "{stderr}");
    let (ok, framed, stderr) = codira(&dir, &["run", "--frame-alloc"]);
    assert!(ok, "{stderr}");
    assert_eq!(plain, framed);
    assert_eq!(plain, "328750 Point { x: 50, y: 2 } 10 3\n");
}
