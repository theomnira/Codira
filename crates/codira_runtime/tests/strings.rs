//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - `String` and the runtime: strings passed to and from the host
//!   (`StringRef`), read from fields, kept alive by the collector under a
//!   small heap cap and host collections, carried through a heap snapshot and
//!   through a hot reload that changes the layout around them.
use std::sync::atomic::{AtomicBool, Ordering};

use codira_runtime::{ArrayRef, StringRef, StructRef};
use codira_test::CompileAndRunTestDriver;

const BLOCK: usize = 64 * 1024;

#[test]
fn strings_cross_the_host_boundary() {
    let driver = CompileAndRunTestDriver::new(
        r#"
        public func greet(name: String) -> String {
            "hello, " + name + "!"
        }

        public func shout(s: String, times: i64) -> String {
            s.to_upper().repeat(times)
        }

        public func length(s: String) -> usize { s.len() }

        public class Person { name: String, age: i64 }

        public func person(name: String) -> Person {
            Person { name: name.trim(), age: 30 }
        }

        public func words(s: String) -> [String] { s.split(" ") }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");
    let runtime = &driver.runtime;

    let name = StringRef::new(runtime, "wörld");
    let greeting: StringRef<'_> = runtime.invoke("greet", (name,)).unwrap();
    assert_eq!(greeting.as_str(), "hello, wörld!");

    let shouted: StringRef<'_> = runtime
        .invoke("shout", (StringRef::new(runtime, "ab"), 3i64))
        .unwrap();
    assert_eq!(shouted.to_string(), "ABABAB");

    let len: usize = runtime
        .invoke("length", (StringRef::new(runtime, "日本"),))
        .unwrap();
    assert_eq!(len, 6);

    let person: StructRef<'_> = runtime
        .invoke("person", (StringRef::new(runtime, "  ann  "),))
        .unwrap();
    assert_eq!(person.get::<StringRef<'_>>("name").unwrap().as_str(), "ann");
    assert_eq!(person.get::<i64>("age").unwrap(), 30);

    let words: ArrayRef<'_, StringRef<'_>> = runtime
        .invoke("words", (StringRef::new(runtime, "a bc def"),))
        .unwrap();
    let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
    assert_eq!(words, ["a", "bc", "def"]);

    // A rooted string survives a collection.
    let kept = greeting.root();
    let _collected = driver.runtime.gc_collect();
    assert_eq!(kept.as_ref(runtime).as_str(), "hello, wörld!");
}

const STRESS: &str = r#"
    public class Holder { s: String, n: i64 }

    func expected(seed: i64, k: i64) -> String {
        "s" + seed.to_string() + "-" + k.to_string() + "-é"
    }

    func check(h: Holder, seed: i64) -> i64 {
        if h.s == expected(seed, h.n) { 0 } else { 1 }
    }

    public func churn(seed: i64, rounds: i64) -> i64 {
        // Live only through this frame and the objects it reaches.
        let keep = Holder { s: expected(seed, 7), n: 7 }
        var built = "".to_string()
        let parts = "x,yy,zzz".to_string().split(",")
        var bad = 0
        var i = 0
        while i < rounds {
            let junk = expected(0 - i, i) + "garbage".repeat(3)
            built += "ab"
            let h = Holder { s: expected(seed, i), n: i }
            bad += check(h, seed)
            bad += check(keep, seed)
            if parts[1] != "yy" { bad += 1 }
            if junk.len() == 0 { bad += 1 }
            i += 1
        }
        if built.len() != (rounds * 2) as usize { bad += 1 }
        var j = 0
        while j < built.len() {
            if built.byte_at(j) != 97u8 + (j % 2) as u8 { bad += 1 }
            j += 1
        }
        bad
    }
"#;

/// Strings and the objects holding them are made far faster than a small
/// heap cap allows, so allocations collect while frames hold live strings
/// (and a string being appended to).
#[test]
fn heap_limit_collections_keep_live_strings_intact() {
    let driver = CompileAndRunTestDriver::new(STRESS, |builder| builder.heap_limit(32 * BLOCK))
        .expect("Failed to build test driver");
    for seed in 1..4i64 {
        let bad: i64 = driver.runtime.invoke("churn", (seed, 20_000i64)).unwrap();
        assert_eq!(bad, 0, "a collection freed or changed a live string");
    }
    let gc = driver.runtime.gc();
    assert!(gc.emergency_collections() > 0, "the cap was never reached");
    assert!(gc.committed_bytes() <= 32 * BLOCK);
}

/// A host thread collects in a loop while Codira code builds, slices and
/// compares strings.
#[test]
fn host_collections_keep_live_strings_intact() {
    let driver = CompileAndRunTestDriver::new(STRESS, |builder| builder)
        .expect("Failed to build test driver");
    let gc = driver.runtime.gc();
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                gc.collect();
                std::thread::yield_now();
            }
        });
        // Stops the collecting thread even when an assertion fails.
        struct SetOnDrop<'a>(&'a AtomicBool);
        impl Drop for SetOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _done = SetOnDrop(&done);
        for seed in 1..4i64 {
            let bad: i64 = driver.runtime.invoke("churn", (seed, 20_000i64)).unwrap();
            assert_eq!(bad, 0, "a collection freed or changed a live string");
        }
    });
}

const PROGRAM: &str = r#"
    public class Doc {
        title: String,
        tags: [String],
    }

    public func make_doc() -> Doc {
        let text = "alpha beta gamma".to_string()
        Doc { title: "Ünïcode title".to_string() + "!", tags: text.split(" ") }
    }

    public func describe(d: Doc) -> String {
        d.title + " [" + d.tags[0] + "," + d.tags[1] + "," + d.tags[2] + "]"
    }
"#;

#[test]
fn strings_survive_a_heap_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.heap");
    {
        let driver = CompileAndRunTestDriver::new(PROGRAM, |builder| builder)
            .expect("Failed to build test driver");
        let doc: StructRef<'_> = driver.runtime.invoke("make_doc", ()).unwrap();
        let doc = doc.root();
        driver.runtime.save_heap(&path).unwrap();
        drop(doc);
    }
    let driver = CompileAndRunTestDriver::new(PROGRAM, |builder| builder)
        .expect("Failed to build test driver");
    let roots = driver.runtime.restore_heap(&path).unwrap();
    assert_eq!(roots.len(), 1);
    let doc = roots[0].as_ref(&driver.runtime);
    assert_eq!(
        doc.get::<StringRef<'_>>("title").unwrap().as_str(),
        "Ünïcode title!"
    );
    let described: StringRef<'_> = driver.runtime.invoke("describe", (doc,)).unwrap();
    assert_eq!(described.as_str(), "Ünïcode title! [alpha,beta,gamma]");
}

#[test]
fn strings_survive_a_hot_reload_that_changes_the_layout() {
    let mut driver = CompileAndRunTestDriver::new(PROGRAM, |builder| builder)
        .expect("Failed to build test driver");
    let doc: StructRef<'_> = driver.runtime.invoke("make_doc", ()).unwrap();
    let doc = doc.root();
    driver.update_file(
        "mod.code",
        r#"
        public class Doc {
            version: i64,
            tags: [String],
            title: String,
            note: String,
        }

        public func describe(d: Doc) -> String {
            d.title + " (" + d.tags[2] + ")"
        }
    "#,
    );
    let doc = doc.as_ref(&driver.runtime);
    assert_eq!(
        doc.get::<StringRef<'_>>("title").unwrap().as_str(),
        "Ünïcode title!"
    );
    let described: StringRef<'_> = driver.runtime.invoke("describe", (doc,)).unwrap();
    assert_eq!(described.as_str(), "Ünïcode title! (gamma)");
}
