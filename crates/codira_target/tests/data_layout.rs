//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_target::{abi::TargetDataLayout, spec::Target};

#[test]
fn data_layout_windows() {
    let layout =
        TargetDataLayout::parse(&Target::search("x86_64-pc-windows-msvc").unwrap()).unwrap();

    insta::assert_debug_snapshot!(layout);
}

#[test]
fn data_layout_darwin() {
    let layout = TargetDataLayout::parse(&Target::search("x86_64-apple-darwin").unwrap()).unwrap();

    insta::assert_debug_snapshot!(layout);
}

#[test]
fn data_layout_linux() {
    let layout =
        TargetDataLayout::parse(&Target::search("x86_64-unknown-linux-gnu").unwrap()).unwrap();

    insta::assert_debug_snapshot!(layout);
}
