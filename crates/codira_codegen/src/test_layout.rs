//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Checks that `codira_hir::layout`, the backend-independent value layout
//!   the Cranelift backend reads, places every scalar where LLVM places it in
//!   the type this backend gives the value, and agrees on size and alignment:
//!   a value stored by one backend's code reads back in the other's, and the
//!   collector traces both at the same offsets.

use codira_hir::{
    layout::{array_elements, object_layout, value_layout, Scalar},
    HirDatabase, Package, Ty, TyKind,
};
use inkwell::{
    context::Context,
    targets::TargetData,
    types::{BasicType, BasicTypeEnum},
};

use crate::{ir::ty::HirTypeCache, mock::MockDatabase};

/// Every scalar of an LLVM type: its offset and kind.
fn llvm_leaves(
    context: &Context,
    target: &TargetData,
    ty: BasicTypeEnum<'_>,
    base: u64,
    out: &mut Vec<(u64, Scalar)>,
) {
    match ty {
        BasicTypeEnum::IntType(int) => out.push((
            base,
            match int.get_bit_width() {
                1 => Scalar::Bool,
                bits => Scalar::Int(u8::try_from(bits).unwrap()),
            },
        )),
        BasicTypeEnum::FloatType(float) => out.push((
            base,
            if float == context.f32_type() {
                Scalar::F32
            } else {
                Scalar::F64
            },
        )),
        BasicTypeEnum::PointerType(_) => out.push((base, Scalar::Ptr { gc: false })),
        BasicTypeEnum::StructType(s) => {
            for (index, field) in s.get_field_types().into_iter().enumerate() {
                let offset = target
                    .offset_of_element(&s, u32::try_from(index).unwrap())
                    .unwrap();
                llvm_leaves(context, target, field, base + offset, out);
            }
        }
        other => panic!("unexpected LLVM type {other:?}"),
    }
}

fn forget_gc(scalar: Scalar) -> Scalar {
    match scalar {
        Scalar::Ptr { .. } => Scalar::Ptr { gc: false },
        other => other,
    }
}

const TYPES: &str = r#"
struct Point { x: f64, y: f64 }
struct Mixed { a: u8, b: i128, c: bool, d: f32, e: u16 }
struct Wrapper { p: Point, flag: bool, m: Mixed }
class Node { value: i64, next: Node?, name: String, tag: u8 }
class Bag { small: u8, items: [i32], big: u128, s: str }
enum Shape { Circle(f64), Rect(f64, f64), Empty }
enum Either[A, B] { Left(A), Right(B) }

pub func all(
    a: i8, b: u16, c: i32, d: i64, e: i128, f: u128, g: f32, h: f64, i: bool, j: usize,
    k: str, l: String, m: [i32], n: Point, o: Mixed, p: Wrapper, q: Node, r: Bag,
    s: (i8, i64, bool), t: i32?, u: Point?, v: Shape, w: Either[u8, i128],
    x: Either[bool, Point], y: (Point?, u8), z: String?, aa: [Mixed]
) {}
"#;

#[test]
fn layout_matches_llvm() {
    let (db, _) = MockDatabase::with_single_file(TYPES);
    let target = db.target();
    let target_data = TargetData::create(&target.data_layout);
    let context = Context::create();
    let types = HirTypeCache::new(&context, &db, TargetData::create(&target.data_layout));

    let function = Package::all(&db)
        .into_iter()
        .flat_map(|p| p.modules(&db))
        .flat_map(|m| m.all_functions(&db))
        .find(|f| f.name(&db).to_string() == "all")
        .expect("the fixture's function");
    let sig = function.ty(&db).callable_sig(&db).unwrap();
    let mut checked = 0;
    let mut check = |ty: &Ty| {
        let what = codira_hir::HirDisplay::display(ty, &db).to_string();
        let ours = value_layout(&db, ty).unwrap_or_else(|| panic!("no layout for `{what}`"));
        let llvm = types.get_basic_type(ty).unwrap();
        let mut leaves = Vec::new();
        llvm_leaves(&context, &target_data, llvm, 0, &mut leaves);
        let our_leaves: Vec<_> = ours
            .leaves
            .iter()
            .map(|leaf| (leaf.offset, forget_gc(leaf.scalar)))
            .collect();
        assert_eq!(our_leaves, leaves, "the scalars of `{what}`");
        assert_eq!(
            ours.size,
            target_data.get_abi_size(&llvm),
            "the size of `{what}`"
        );
        assert_eq!(
            ours.align,
            u64::from(target_data.get_abi_alignment(&llvm)),
            "the alignment of `{what}`"
        );
        checked += 1;
    };
    for param in sig.params() {
        check(param);
        // A class's payload, as `new` allocates it.
        if let TyKind::Struct(..) = param.interned() {
            let (payload, offsets) = object_layout(&db, param).unwrap();
            let llvm = types.get_struct_type(param);
            assert_eq!(payload.size, target_data.get_abi_size(&llvm));
            for (index, offset) in offsets.iter().enumerate() {
                assert_eq!(
                    Some(*offset),
                    target_data.offset_of_element(&llvm, u32::try_from(index).unwrap()),
                    "field {index} of `{}`",
                    codira_hir::HirDisplay::display(param, &db)
                );
            }
        }
        // An array's elements.
        if let TyKind::Array(element) = param.interned() {
            let (first, stride) = array_elements(&db, element).unwrap();
            let llvm = types.get_array_type(element);
            assert_eq!(Some(first), target_data.offset_of_element(&llvm, 2));
            let element_ty = types.get_basic_type(element).unwrap();
            assert_eq!(stride, target_data.get_abi_size(&element_ty));
            let _ = element_ty.size_of();
        }
    }
    assert_eq!(checked, 27);
}
