//! Copyright (c) 2026 Omnira CJSC
//! Date: September 21, 2026
//!
//! Rendering every diagnostic must never panic, regardless of what shape
//! the diagnostic's source span takes.
//!
//! `stdlib_check_ratchet` (the sibling test) only calls
//! `Driver::collect_diagnostics`, which builds structured data and never
//! touches the `annotate_snippets` renderer at all -- so it could not have
//! caught the bug this test is against: a multi-line "generic function not
//! instantiated" diagnostic spanning an entire multi-statement function
//! crashed the renderer with "`SourceAnnotation` range is beyond the end of
//! buffer", because `fold: true` plus a wide multi-line annotation range is
//! more than the renderer can always handle. It reproduced on
//! `std/collections/binary_heap.code`'s `iter_next` and is reproduced here
//! with the same shape, independent of that file's continued existence.

use codira_compiler::{Config, DisplayColor, Driver, PathOrInline};

#[test]
fn rendering_a_multiline_generic_function_diagnostic_does_not_panic() {
    // `iter_next`'s own shape: several statements, a struct-wrapped type
    // parameter in the return type (`contains_type_parameter` has to look
    // inside `Pair[T]`'s substitution to find `T`, not just match a bare
    // type parameter directly), and enough lines that the whole-function
    // span is genuinely multi-line.
    let source = r"
    public struct Pair[T] {
        first: T,
        second: usize,
    }

    public func make_pair[T](value: T) -> (Pair[T], usize) {
        let wrapped = Pair { first: value, second: 0 };
        if wrapped.second == 0 {
            return (wrapped, 1);
        }
        let next = Pair { first: wrapped.first, second: wrapped.second + 1 };
        return (next, 2);
    }
    ";

    let (driver, _file_id) = Driver::with_file(
        Config::default(),
        PathOrInline::Inline {
            rel_path: codira_paths::RelativePathBuf::from("mod.code"),
            contents: source.to_string(),
        },
    )
    .expect("driver for the sample");

    let mut sink = Vec::new();
    driver
        .emit_diagnostics(&mut sink, DisplayColor::Disable)
        .expect("rendering diagnostics must not error, and must not panic");

    let rendered = String::from_utf8(sink).expect("rendered output is valid UTF-8");
    assert!(
        rendered.contains("Pair[T]") || rendered.contains("not instantiated"),
        "expected the generic-function-not-instantiated diagnostic to fire and render; got:\n{rendered}"
    );
}
