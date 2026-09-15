//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
#![allow(dead_code)]

#[macro_export]
macro_rules! fake_struct {
    ($type_table:expr, $struct_name:expr, $($field_name:expr => $field_ty:ident),+) => {{
        codira_memory::StructTypeBuilder::new(String::from($struct_name))
            $(
                .add_field(
                    String::from($field_name),
                    $type_table.find_type_info_by_name(format!("core::{}", stringify!($field_ty))).unwrap()
                )
            )+
             .finish()
    }};
}
