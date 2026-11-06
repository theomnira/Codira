//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Checks that the runtime registers exactly the functions of the runtime
//!   function catalogue (`codira_abi::runtime_functions`), each with the
//!   signature the catalogue declares, so that neither the backends' imports
//!   nor the effect analysis can drift from what the runtime provides.

use std::collections::BTreeSet;

use codira_abi::runtime_functions::{Repr, CATALOGUE};
use codira_memory::HasStaticType;

use crate::{generated_code_functions, Type};

fn type_of(repr: Repr) -> Type {
    use std::ffi::c_void;
    match repr {
        Repr::Unit => <()>::type_info().clone(),
        Repr::U8 => u8::type_info().clone(),
        Repr::U32 => u32::type_info().clone(),
        Repr::I32 => i32::type_info().clone(),
        Repr::I64 => i64::type_info().clone(),
        Repr::U64 => u64::type_info().clone(),
        Repr::Usize => usize::type_info().clone(),
        Repr::F32 => f32::type_info().clone(),
        Repr::F64 => f64::type_info().clone(),
        Repr::ConstBytes => <*const u8>::type_info().clone(),
        Repr::MutBytes => <*mut u8>::type_info().clone(),
        Repr::Handle => <*const c_void>::type_info().clone(),
        Repr::MutHandle => <*mut c_void>::type_info().clone(),
        Repr::Object => <*const *mut c_void>::type_info().clone(),
    }
}

#[test]
fn catalogue_matches_registration() {
    let registered = generated_code_functions();
    let registered_names: BTreeSet<&str> = registered
        .iter()
        .map(|f| f.prototype.name.as_str())
        .collect();
    let catalogued: BTreeSet<&str> = CATALOGUE.iter().map(|f| f.symbol).collect();
    assert_eq!(
        registered_names.len(),
        registered.len(),
        "a function is registered twice"
    );
    let missing: Vec<_> = catalogued.difference(&registered_names).collect();
    let unlisted: Vec<_> = registered_names.difference(&catalogued).collect();
    assert!(
        missing.is_empty() && unlisted.is_empty(),
        "catalogued but not registered: {missing:?}; registered but not catalogued: {unlisted:?}"
    );

    for definition in &registered {
        let entry = codira_abi::runtime_functions::lookup(&definition.prototype.name).unwrap();
        let signature = &definition.prototype.signature;
        let expected: Vec<Type> = entry
            .params
            .iter()
            .map(|&(_, repr)| type_of(repr))
            .collect();
        assert_eq!(
            signature.arg_types, expected,
            "`{}`: the parameters registered differ from the catalogue's",
            entry.symbol
        );
        assert_eq!(
            signature.return_type,
            type_of(entry.ret),
            "`{}`: the result registered differs from the catalogue's",
            entry.symbol
        );
    }
}

#[test]
fn no_native_service_shadows_a_catalogued_function() {
    let natives: BTreeSet<String> = crate::native_services()
        .into_iter()
        .map(|f| f.prototype.name)
        .collect();
    for entry in CATALOGUE {
        assert!(!natives.contains(entry.symbol), "`{}`", entry.symbol);
    }
}
