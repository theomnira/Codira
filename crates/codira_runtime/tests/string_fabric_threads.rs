//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - A two-thread stress of the string buffers' owner word, across the host
//!   boundary. An owner thread builds one buffer of nested `String` values
//!   (each a prefix of the next, appended in place) and publishes them to a
//!   second host thread through a barrier. Then, at the same time, the
//!   owner appends in place past the fill of that same buffer while the
//!   other thread reads every published value and appends to copies of
//!   them. The values must never change, the other thread's appends must
//!   copy (its bytes never appear in the owner's buffer), and the owner's
//!   appended bytes must be exactly its own. In a test binary of its own:
//!   the two host threads run Codira code through raw function pointers,
//!   as `gc_threads.rs` does, and collections stop every such thread.
//!
//! What it cannot catch: interleavings the scheduler does not produce in
//! the rounds given, and the shared-buffer path (owner word zero) of
//! platforms without a thread pointer, which this platform does not take.
use std::sync::Barrier;

use codira_runtime::StructRef;
use codira_test::CompileAndRunTestDriver;

const FABRIC: &str = r#"
    public class Shared { vals: [String], cur: String, base: i64 }

    public func make_shared() -> Shared {
        Shared { vals: [], cur: "".to_string(), base: 0 }
    }

    // Owner: one buffer of values, each a prefix of the next, appended in
    // place; every 100th is published.
    public func prepare(sh: Shared, round: i64) -> i64 {
        sh.vals.clear()
        var cur = "r" + round.to_string() + ":"
        sh.base = cur.len() as i64
        var k = 0
        while k < 2000 {
            cur += "k"
            if k % 100 == 0 { sh.vals.push(cur) }
            k += 1
        }
        sh.cur = cur
        0
    }

    // Owner, while `check` runs on another thread: appends past the fill
    // of the same buffer, then checks that every byte it appended is its
    // own and the published values are unchanged.
    public func grow(sh: Shared, n: i64) -> i64 {
        let start = sh.cur.len()
        var cur = sh.cur
        var i = 0
        while i < n {
            cur += "g"
            i += 1
        }
        var bad = 0
        if cur.len() != start + n as usize { bad += 1 }
        var j = start
        while j < cur.len() {
            if cur.byte_at(j) != 103u8 { bad += 1 }
            j += 1
        }
        bad
    }

    // Another thread: the published values never change, and appending to
    // them copies.
    public func check(sh: Shared, passes: i64) -> i64 {
        var bad = 0
        var p = 0
        while p < passes {
            var j = 0
            while j < sh.vals.len() {
                let v = sh.vals[j]
                if v.len() != sh.base as usize + 1 + j * 100 { bad += 1 }
                if v.byte_at(v.len() - 1) != 107u8 { bad += 1 }
                if v.byte_at(sh.base as usize) != 107u8 { bad += 1 }
                var c = v
                c += "b"
                if c.byte_at(c.len() - 1) != 98u8 { bad += 1 }
                if v.byte_at(v.len() - 1) != 107u8 { bad += 1 }
                j += 1
            }
            p += 1
        }
        bad
    }
"#;

/// A Codira object handle the two threads share. The object is rooted by
/// the test, and the threads hand it over through the barrier.
/// (The raw handle's type, `StructRef::into_raw`'s, is not exported by
/// name, hence the type parameter.)
#[derive(Clone)]
struct Handle<T>(T);

// Safety: the handle is only used through Codira functions called inside a
// mutator guard, and the barrier orders every use of the object's fields.
unsafe impl<T> Send for Handle<T> {}
unsafe impl<T> Sync for Handle<T> {}

/// The function at `address`, taking a raw handle like `_handle` and an
/// integer.
///
/// # Safety
///
/// That must be the function's signature.
unsafe fn with_handle<T>(_handle: &T, address: usize) -> extern "C" fn(T, i64) -> i64 {
    std::mem::transmute::<usize, extern "C" fn(T, i64) -> i64>(address)
}

#[test]
fn appends_in_place_never_change_values_another_thread_reads() {
    let driver = CompileAndRunTestDriver::new(FABRIC, |builder| builder)
        .expect("Failed to build test driver");
    let runtime = &driver.runtime;
    let shared = {
        let _guard = codira_runtime::enter_codira();
        let shared: StructRef<'_> = runtime.invoke("make_shared", ()).unwrap();
        shared.root()
    };
    let handle = Handle(shared.as_ref(runtime).into_raw());
    let function = |name: &str| {
        let definition = runtime.get_function_definition(name).expect(name);
        definition.fn_ptr as usize
    };
    // Safety: these are the signatures of the functions above.
    let (prepare, grow, check) = unsafe {
        (
            with_handle(&handle.0, function("prepare")),
            with_handle(&handle.0, function("grow")),
            with_handle(&handle.0, function("check")),
        )
    };

    let rounds = 2_000;
    let barrier = Barrier::new(2);
    let (owner_bad, reader_bad) = std::thread::scope(|s| {
        let owner = s.spawn(|| {
            let mut bad = 0;
            for round in 0..rounds {
                {
                    let _guard = codira_runtime::enter_codira();
                    bad += prepare(handle.clone().0, round);
                }
                barrier.wait(); // published
                {
                    let _guard = codira_runtime::enter_codira();
                    bad += grow(handle.clone().0, 1500);
                }
                barrier.wait(); // round over
            }
            bad
        });
        let reader = s.spawn(|| {
            let mut bad = 0;
            for _ in 0..rounds {
                barrier.wait();
                {
                    let _guard = codira_runtime::enter_codira();
                    bad += check(handle.clone().0, 3);
                }
                barrier.wait();
            }
            bad
        });
        (owner.join().unwrap(), reader.join().unwrap())
    });
    assert_eq!(
        owner_bad, 0,
        "the owner's appended bytes were not all its own"
    );
    assert_eq!(
        reader_bad, 0,
        "a published value changed, or an append to it did not copy"
    );
    drop(shared);
}
