//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - `str` and `String` on the Cranelift JIT: literals, conversion,
//!   printing, `+`, `+=` (on a local, an element and a field), comparisons,
//!   `to_string()`, the builtin methods, strings in classes and arrays and
//!   across calls, and a run under a heap limit that collects while JIT
//!   frames hold the only references to string buffers.

use codira_compiler::{Config, Driver, PathOrInline, RelativePathBuf};
use codira_cranelift::{compile, compile_with, Options, Returned};

fn driver(source: &str) -> Driver {
    let (driver, _) = Driver::with_file(
        Config::default(),
        PathOrInline::Inline {
            rel_path: RelativePathBuf::from("mod.code"),
            contents: source.to_owned(),
        },
    )
    .unwrap();
    driver
}

fn run(source: &str) -> (Returned, String) {
    let driver = driver(source);
    let program = compile(driver.db(), "main").unwrap_or_else(|e| panic!("did not compile: {e}"));
    codira_runtime::capture_output(|| program.run())
}

#[test]
fn strings_are_built_compared_and_printed() {
    let source = r#"
        class Named { name: String, tags: [String] }
        func greet(who: String) -> String { "hello, " + who }
        public func main() -> i64 {
            let lit: str = "abc"
            var s: String = "x"
            s += lit
            s += 42.to_string()
            let n = Named { tags: ["a", "b"], name: greet("you") }
            n.name += "!"
            n.tags[1] += "c"
            n.tags.push((1.5).to_string())
            println(s, " ", n.name, " ", n.tags[1], " ", n.tags[2], " ", lit.len())
            println(s == "xabc42", " ", s < "y", " ", "b" >= "a", " ", s != s)
            println(s.len(), " ", "héllo".char_count(), " ", s.contains("bc"), " ",
                s.starts_with("xa"), " ", s.ends_with("2"), " ", s.is_empty())
            println("Ab".to_upper(), "Ab".to_lower(), "ab".repeat(3), "a-b".replace("-", "+"),
                true.to_string(), (-7i8).to_string(), (250u8).to_string())
            for t in n.tags { print(t, ";") }
            println()
            s.len() as i64
        }
    "#;
    let (returned, printed) = run(source);
    assert_eq!(
        printed,
        "xabc42 hello, you! bc 1.5 3\n\
         true true true false\n\
         6 5 true true true false\n\
         ABababababa+btrue-7250\n\
         a;bc;1.5;\n"
    );
    assert_eq!(returned, Returned::Int(6));
}

/// Strings built and appended under a heap limit: collections run while the
/// only references to the buffers are in JIT frames and in the out-slots of
/// runtime calls.
#[test]
fn strings_survive_collections() {
    let source = r#"
        func build(n: i64) -> String {
            var s: String = ""
            var i: i64 = 0
            while i < n {
                s += i.to_string()
                s = s + ","
                i += 1
            }
            s
        }
        public func main() -> i64 {
            var total: i64 = 0
            var round: i64 = 0
            let keep: [String] = []
            while round < 300 {
                let s = build(40)
                if round % 30 == 0 { keep.push(s) }
                total += s.len() as i64
                round += 1
            }
            for k in keep { total += k.char_count() as i64 }
            total
        }
    "#;
    let driver = driver(source);
    let program = compile_with(
        driver.db(),
        "main",
        Options {
            // Every size class of buffer takes a block of its own, so the
            // limit leaves room for a few classes; the program allocates
            // several megabytes in all.
            heap_limit: Some(1024 * 1024),
            ..Options::default()
        },
    )
    .unwrap_or_else(|e| panic!("did not compile: {e}"));
    // "0,1,...,39," is 10 * 2 + 30 * 3 = 110 bytes.
    assert_eq!(program.run(), Returned::Int(300 * 110 + 10 * 110));
    assert!(program.emergency_collections() > 0);
}
