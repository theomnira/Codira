//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
#[cfg(test)]
mod test {
    use crate::completion::test_utils::completion_relevance_string;

    #[test]
    fn test_locals_first() {
        insta::assert_snapshot!(completion_relevance_string(
            r#"
            func a() {};

        func foo(bar: u32) {
            let zinq = 0;
            z$0
        }
        "#
        ), @r###"
        lc zinq i32
        lc bar  u32
        fn foo  -> ()
        fn a    -> ()
        "###);
    }
}
